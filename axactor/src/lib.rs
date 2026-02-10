
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::error;
use tokio::time::Instant;
use parking_lot::Mutex;

pub use axactor_macros::actor;
pub use async_trait;

pub type SharedHandle = Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>;

pub use rt::{System, Actor};
pub mod rt;
pub mod obs;
pub mod registry;

pub trait MessageKind {
    fn kind(&self) -> &'static str;
}

pub trait RefMetrics {
    fn mailbox_len(&self) -> usize;
    fn inflight_count(&self) -> usize;
    fn idle_for(&self) -> Duration;
    fn silence_for(&self) -> Duration;
    fn incarnation(&self) -> u64;
    fn is_closed(&self) -> bool;
    fn is_terminated(&self) -> bool;

    fn stop(&self) -> Result<(), TellError>;
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum AskError {
    #[error("Mailbox full")]
    Full,
    #[error("Actor closed")]
    Closed,
    #[error("Request timed out")]
    Timeout,
    #[error("Response canceled")]
    Canceled,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum TellError {
    #[error("Mailbox full")]
    Full,
    #[error("Actor closed")]
    Closed,
}

pub trait SpawnGuard: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> SpawnGuard for T {}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("Actor name already exists: {0}")]
    NameTaken(String),
    #[error("spawn(actor, ..) cannot restart; use spawn_with(...) for restart policies")]
    NotRestartable,
    #[error("system is shutting down")]
    ShuttingDown,
}

impl From<TellError> for AskError {

    fn from(e: TellError) -> Self {
        match e {
            TellError::Full => AskError::Full,
            TellError::Closed => AskError::Closed,
        }
    }
}

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for AskError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            AskError::Full => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            AskError::Closed => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            AskError::Timeout => axum::http::StatusCode::GATEWAY_TIMEOUT,
            AskError::Canceled => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for TellError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            TellError::Full => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            TellError::Closed => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Clone)]
pub struct SpawnConfig {
    pub name: String,
    pub capacity: usize,
    pub restart_policy: RestartPolicy,
    pub shutdown_mode: ShutdownMode,
    pub extra_guards: Vec<Arc<dyn SpawnGuard>>,
}

impl SpawnConfig {
    pub fn new(name: impl Into<String>, capacity: usize) -> Self {
        Self {
            name: name.into(),
            capacity,
            restart_policy: RestartPolicy::None,
            shutdown_mode: ShutdownMode::Immediate,
            extra_guards: Vec::new(),
        }
    }

    pub fn push_guard(&mut self, guard: Arc<dyn SpawnGuard>) {
        self.extra_guards.push(guard);
    }

    pub fn restart_on_panic(mut self, max_restarts: usize, min_backoff: Duration, max_backoff: Duration) -> Self {
        self.restart_policy = RestartPolicy::OnPanic {
            max_restarts,
            min_backoff,
            max_backoff,
        };
        self
    }

    pub fn shutdown_graceful(mut self, deadline: Duration) -> Self {
        self.shutdown_mode = ShutdownMode::Graceful { deadline };
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    None,
    OnPanic {
        max_restarts: usize,
        min_backoff: Duration,
        max_backoff: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Immediate,
    Graceful { deadline: Duration },
}

pub struct Context {
    stop_requested: bool,
}

impl Context {
    pub fn new() -> Self {
        Self { stop_requested: false }
    }

    pub fn stop(&mut self) {
        self.stop_requested = true;
    }

    pub fn is_stop_requested(&self) -> bool {
        self.stop_requested
    }
}

pub struct MailboxMeter {
    len: AtomicUsize,
    inflight: AtomicUsize,
    base: Instant,
    last_seen_ms: AtomicU64,
    last_done_ms: AtomicU64,
    incarnation: AtomicU64,
    closed: std::sync::atomic::AtomicBool,
    terminated: std::sync::atomic::AtomicBool,
}

impl MailboxMeter {
    pub fn new() -> Self {
        let base = Instant::now();
        Self {
            len: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            base,
            last_seen_ms: AtomicU64::new(0),
            last_done_ms: AtomicU64::new(0),
            incarnation: AtomicU64::new(1),
            closed: std::sync::atomic::AtomicBool::new(false),
            terminated: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn on_startup(&self) {
        let now = self.now_ms();
        self.last_seen_ms.store(now, Ordering::Relaxed);
        self.last_done_ms.store(now, Ordering::Relaxed);
    }

    #[inline]
    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }

    #[inline]
    pub fn on_send_ok(&self) {
        self.len.fetch_add(1, Ordering::Relaxed);
        self.last_seen_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    #[inline]
    pub fn on_recv(&self) {
        #[cfg(debug_assertions)]
        {
            let prev = self.len.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(prev > 0, "MailboxMeter len underflow detected");
        }
        #[cfg(not(debug_assertions))]
        {
            self.len.fetch_sub(1, Ordering::Relaxed);
        }
        self.last_seen_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    #[inline]
    pub fn on_process_start(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn on_process_end(&self) {
        #[cfg(debug_assertions)]
        {
            let prev = self.inflight.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(prev > 0, "inflight underflow detected");
        }
        #[cfg(not(debug_assertions))]
        {
            self.inflight.fetch_sub(1, Ordering::Relaxed);
        }
        self.last_done_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn inflight_count(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn idle_for(&self) -> Duration {
        let now = self.now_ms();
        let last = self.last_done_ms.load(Ordering::Relaxed);
        Duration::from_millis(now.saturating_sub(last))
    }

    #[inline]
    pub fn silence_for(&self) -> Duration {
        let now = self.now_ms();
        let last = self.last_seen_ms.load(Ordering::Relaxed);
        Duration::from_millis(now.saturating_sub(last))
    }

    #[inline]
    pub fn incarnation(&self) -> u64 {
        self.incarnation.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn inc_incarnation(&self) {
        self.incarnation.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_closed(&self, closed: bool) {
        self.closed.store(closed, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_terminated(&self, terminated: bool) {
        self.terminated.store(terminated, Ordering::Relaxed);
    }
}

pub struct ProcessGuard<'a> {
    meter: &'a MailboxMeter,
}

impl<'a> ProcessGuard<'a> {
    pub fn new(meter: &'a MailboxMeter) -> Self {
        meter.on_process_start();
        Self { meter }
    }
}

impl Drop for ProcessGuard<'_> {
    fn drop(&mut self) {
        self.meter.on_process_end();
    }
}

pub struct MailboxTx<M> {
    tx: mpsc::Sender<M>,
    meter: Arc<MailboxMeter>,
}

impl<M> Clone for MailboxTx<M> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            meter: self.meter.clone(),
        }
    }
}

impl<M> MailboxTx<M> {
    pub fn new(tx: mpsc::Sender<M>, meter: Arc<MailboxMeter>) -> Self {
        Self { tx, meter }
    }

    pub async fn send_wait(&self, msg: M) -> Result<(), TellError> {
        let permit = self.tx.reserve().await.map_err(|_| TellError::Closed)?;
        self.meter.on_send_ok();
        permit.send(msg);
        Ok(())
    }

    pub fn try_send(&self, msg: M) -> Result<(), TellError> {
        match self.tx.try_send(msg) {
            Ok(_) => {
                self.meter.on_send_ok();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(TellError::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TellError::Closed),
        }
    }

    pub fn len(&self) -> usize {
        self.meter.len()
    }

    pub fn inflight_count(&self) -> usize {
        self.meter.inflight_count()
    }

    pub fn idle_for(&self) -> Duration {
        self.meter.idle_for()
    }

    pub fn silence_for(&self) -> Duration {
        self.meter.silence_for()
    }

    pub fn incarnation(&self) -> u64 {
        self.meter.incarnation()
    }

    pub fn is_closed(&self) -> bool {
        self.meter.is_closed()
    }

    pub fn is_terminated(&self) -> bool {
        self.meter.is_terminated()
    }

    pub async fn ask_wait<F, R>(&self, factory: F) -> Result<R, AskError>
    where
        F: FnOnce(tokio::sync::oneshot::Sender<R>) -> M,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = factory(tx);
        self.send_wait(msg).await?;
        rx.await.map_err(|_| AskError::Canceled)
    }

    pub async fn ask_wait_timeout<F, R>(&self, factory: F, timeout: Duration) -> Result<R, AskError>
    where
        F: FnOnce(tokio::sync::oneshot::Sender<R>) -> M,
    {
        tokio::time::timeout(timeout, self.ask_wait(factory))
            .await
            .map_err(|_| AskError::Timeout)?
    }
}

pub struct SpawnHandle<R> {
    actor_ref: R,
    handle: SharedHandle,
}

impl<R> SpawnHandle<R> {
    pub fn new(actor_ref: R, handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            actor_ref,
            handle: Arc::new(Mutex::new(Some(handle)))
        }
    }

    pub fn new_shared(actor_ref: R, handle: SharedHandle) -> Self {
        Self { actor_ref, handle }
    }

    pub fn actor(&self) -> R
    where R: Clone
    {
        self.actor_ref.clone()
    }

    pub async fn join(&self) -> Result<(), tokio::task::JoinError> {
        let handle = self.handle.lock().take();
        if let Some(h) = handle {
            h.await
        } else {
            Ok(())
        }
    }
}