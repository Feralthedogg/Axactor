use crate::*;
use std::collections::HashMap;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error, Level};
use std::time::Duration;
use futures::FutureExt;

#[async_trait::async_trait]
pub trait Actor: Send + 'static {
    type Msg: Send + 'static;
    async fn handle(&mut self, ctx: &mut Context, msg: Self::Msg);

    async fn on_start(&mut self, _ctx: &mut Context) {}
    async fn on_restart(&mut self, _ctx: &mut Context) {}
    async fn on_stop(&mut self, _ctx: &mut Context) {}
}

pub struct System {
    actors: Arc<Mutex<HashMap<String, SharedHandle>>>,
    shutdown_token: tokio_util::sync::CancellationToken,
}

impl System {
    pub fn new() -> Self {
        Self {
            actors: Arc::new(Mutex::new(HashMap::new())),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn spawn<A, M, R>(&self, actor: A, cfg: SpawnConfig) -> Result<SpawnHandle<R>, SpawnError>
    where
        A: Actor<Msg = M> + Send + 'static,
        M: MessageKind + Send + 'static,
        R: From<MailboxTx<M>>,
    {
        if !matches!(cfg.restart_policy, RestartPolicy::None) {
            return Err(SpawnError::NotRestartable);
        }

        let actor_cell = Arc::new(Mutex::new(Some(actor)));

        self.spawn_with(move || {
            actor_cell.lock().take().expect("Actor instance already used. For restarts, use spawn_with.")
        }, cfg)
    }

    pub fn spawn_with<F, A, M, R>(&self, mut factory: F, cfg: SpawnConfig) -> Result<SpawnHandle<R>, SpawnError>
    where
        F: FnMut() -> A + Send + 'static,
        A: Actor<Msg = M> + Send + 'static,
        M: MessageKind + Send + 'static,
        R: From<MailboxTx<M>>,
    {
        if self.shutdown_token.is_cancelled() {
            return Err(SpawnError::ShuttingDown);
        }

        let name = cfg.name.clone();

        {
            let map = self.actors.lock();
            if map.contains_key(&name) {
                return Err(SpawnError::NameTaken(name));
            }
        }

        let slot: SharedHandle = Arc::new(parking_lot::Mutex::new(None));
        let (tx, mut rx) = mpsc::channel(cfg.capacity);
        let meter = Arc::new(MailboxMeter::new());
        let mailbox = MailboxTx::new(tx, meter.clone());
        let actor_ref = R::from(mailbox);

        let actors_map = self.actors.clone();
        let shutdown_token = self.shutdown_token.clone();

        let restart_policy = cfg.restart_policy;
        let shutdown_mode = cfg.shutdown_mode;
        let thread_name = name.clone();

        {
            let mut map = self.actors.lock();
            if self.shutdown_token.is_cancelled() {
                return Err(SpawnError::ShuttingDown);
            }
            if map.contains_key(&name) {
                return Err(SpawnError::NameTaken(name));
            }

            let extra_guards = cfg.extra_guards.clone();

            let handle = tokio::spawn(async move {
                struct CleanupGuard {
                    map: Arc<parking_lot::Mutex<HashMap<String, SharedHandle>>>,
                    name: String,
                    meter: Arc<MailboxMeter>,
                    _extras: Vec<Arc<dyn SpawnGuard>>,
                }
                impl Drop for CleanupGuard {
                    fn drop(&mut self) {
                        self.meter.set_closed(true);
                        self.meter.set_terminated(true);
                        self.map.lock().remove(&self.name);
                    }
                }

                let _guard = CleanupGuard {
                    map: actors_map,
                    name: thread_name.clone(),
                    meter: meter.clone(),
                    _extras: extra_guards,
                };

                let mut restarts = 0usize;

                loop {
                    let mut crashed = false;

                    let factory_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory()));

                    let maybe_actor = match factory_res {
                        Ok(a) => Some(a),

                        Err(_) => {
                            error!(actor = %thread_name, "Actor factory panicked during initialization");
                            crashed = true;
                            None
                        }
                    };

                    if let Some(mut actor) = maybe_actor {
                        let mut ctx = Context::new();
                        meter.on_startup();

                        let hook_ok = if restarts == 0 {
                            run_hook(&thread_name, actor.on_start(&mut ctx)).await
                        } else {
                            meter.inc_incarnation();
                            run_hook(&thread_name, actor.on_restart(&mut ctx)).await
                        };

                        if !hook_ok {
                            crashed = true;
                        } else {
                            info!(actor = %thread_name, "Actor started");

                            let mut shutdown_deadline: Option<tokio::time::Instant> = None;

                            loop {
                                tokio::select! {
                                    _ = shutdown_token.cancelled(), if shutdown_deadline.is_none() => {
                                        match shutdown_mode {
                                            ShutdownMode::Immediate => {
                                                info!(actor = %thread_name, "Immediate shutdown");
                                                meter.set_closed(true);
                                                rx.close();
                                                break;
                                            }

                                            ShutdownMode::Graceful { deadline } => {
                                                info!(actor = %thread_name, "Graceful shutdown started, closing mailbox and draining");
                                                meter.set_closed(true);
                                                rx.close();
                                                shutdown_deadline = Some(tokio::time::Instant::now() + deadline);
                                                if meter.len() == 0 && meter.inflight_count() == 0 { break; }
                                            }

                                        }
                                    }
                                    _ = async {
                                        if let Some(dl) = shutdown_deadline {
                                            tokio::time::sleep_until(dl).await;
                                        } else {
                                            std::future::pending::<()>().await;
                                        }
                                    } => {
                                        error!(actor = %thread_name, "Graceful shutdown deadline reached, terminating");
                                        break;
                                    }

                                    msg_opt = rx.recv() => {
                                        match msg_opt {
                                            Some(msg) => {
                                                meter.on_recv();
                                                let kind = msg.kind();

                                                let _og = if tracing::enabled!(Level::DEBUG) || tracing::enabled!(Level::WARN) {
                                                    Some(crate::obs::on_message(&thread_name, kind))
                                                } else {
                                                    None
                                                };

                                                let _guard = ProcessGuard::new(&meter);
                                                let res = std::panic::AssertUnwindSafe(actor.handle(&mut ctx, msg)).catch_unwind().await;

                                                if res.is_err() {
                                                    error!(actor = %thread_name, msg_kind = %kind, "Actor panicked while handling message");
                                                    crashed = true;
                                                    break;
                                                }

                                                if ctx.is_stop_requested() {
                                                    info!(actor = %thread_name, "Actor stop requested via Context");
                                                    meter.set_closed(true);
                                                    rx.close();
                                                    break;
                                                }

                                                if shutdown_deadline.is_some() && meter.len() == 0 && meter.inflight_count() == 0 {
                                                    break;
                                                }
                                            }
                                            None => {
                                                meter.set_closed(true);
                                                info!(actor = %thread_name, "Mailbox closed, actor terminating");
                                                break;
                                            }

                                        }
                                    }
                                }
                            }
                        }

                        if !crashed {
                            run_hook(&thread_name, actor.on_stop(&mut ctx)).await;
                        }
                    }

                    if shutdown_token.is_cancelled() || !crashed {
                        break;
                    }

                    restarts += 1;
                    if let RestartPolicy::OnPanic { max_restarts, min_backoff, max_backoff } = restart_policy {
                        if restarts > max_restarts {
                            error!(actor = %thread_name, "Max restarts reached ({}), terminating", max_restarts);
                            break;
                        }

                        let mut backoff = min_backoff;
                        for _ in 1..restarts {
                            backoff = backoff.saturating_mul(2);
                        }
                        if backoff > max_backoff { backoff = max_backoff; }

                        error!(actor = %thread_name, "Restarting ({}/{}) after {:?}", restarts, max_restarts, backoff);
                        tokio::time::sleep(backoff).await;
                    } else {
                        break;
                    }
                }
                info!(actor = %thread_name, "Actor task exiting");
            });

            *slot.lock() = Some(handle);
            map.insert(name, slot.clone());
        }

        Ok(SpawnHandle::new_shared(actor_ref, slot))
    }

    pub async fn shutdown_graceful(&self, timeout: Duration) {
        info!("System shutdown requested");
        self.shutdown_token.cancel();

        let handles: Vec<_> = {
            let mut actors = self.actors.lock();
            actors.drain().map(|(_, h)| h).collect()
        };

        if tokio::time::timeout(timeout, async {
            for h in handles {
                let handle = h.lock().take();
                if let Some(handle) = handle {
                    let _ = handle.await;
                }
            }
        }).await.is_err() {
            error!("System shutdown timed out");
        } else {
            info!("System shutdown complete");
        }
    }
}

async fn run_hook<Fut>(thread_name: &str, fut: Fut) -> bool
where
    Fut: std::future::Future<Output = ()> + Send,
{
    use futures::FutureExt;
    let res = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
    if res.is_err() {
        error!(actor = %thread_name, "Actor lifecycle hook panicked");
        return false;
    }
    true
}
