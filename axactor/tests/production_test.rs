use axactor::{rt::System, SpawnConfig, SpawnHandle};

use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct PanickingActor {
    count: Arc<AtomicUsize>,
}

#[axactor::actor]
impl PanickingActor {
    #[msg]
    async fn do_panic(&mut self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        panic!("Actor intentional panic");
    }

    #[msg]
    async fn get_count(&mut self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn test_supervision_restart_backoff() {
    let system = System::new();
    let count = Arc::new(AtomicUsize::new(0));

    let cfg = SpawnConfig::new("panicker", 10)
        .restart_on_panic(3, Duration::from_millis(100), Duration::from_secs(1));

    let count_for_factory = count.clone();
    let spawned = system.spawn_with(move || PanickingActor { count: count_for_factory.clone() }, cfg).unwrap();
    let actor: PanickingActorRef = spawned.actor();

    let _ = actor.do_panic();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = actor.do_panic();
    tokio::time::sleep(Duration::from_millis(50)).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let current_count = count.load(Ordering::SeqCst);
    assert!(current_count >= 2, "Actor should have been called at least twice");

    system.shutdown_graceful(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn test_graceful_shutdown_drain() {
    let system = System::new();

    pub struct SlowActor;
    #[axactor::actor]
    impl SlowActor {
        #[msg]
        async fn slow_op(&mut self) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let cfg = SpawnConfig::new("slow", 10).shutdown_graceful(Duration::from_secs(1));
    let spawned = system.spawn(SlowActor, cfg).unwrap();
    let actor: SlowActorRef = spawned.actor();

    let _ = actor.slow_op();

    let start = std::time::Instant::now();
    system.shutdown_graceful(Duration::from_secs(1)).await;
    let elapsed = start.elapsed();

    assert!(elapsed >= Duration::from_millis(200), "Shutdown should have waited for the slow op");
}

#[tokio::test]
async fn test_name_collision() {
    let system = System::new();

    pub struct DummyActor;
    #[axactor::actor]
    impl DummyActor {}

    let cfg = SpawnConfig::new("duplicate", 10);
    let _handle1: SpawnHandle<DummyActorRef> = system.spawn(DummyActor, cfg.clone()).unwrap();

    let res: Result<SpawnHandle<DummyActorRef>, _> = system.spawn(DummyActor, cfg);
    assert!(matches!(res, Err(axactor::SpawnError::NameTaken(_))));
}

#[tokio::test]
async fn test_macro_context_comma() {
    let system = System::new();

    pub struct ContextActor;
    #[axactor::actor]
    impl ContextActor {
        #[msg]
        async fn only_context(&mut self, _ctx: &mut axactor::Context) {
        }
    }

    let cfg = SpawnConfig::new("context", 10);
    let spawned = system.spawn(ContextActor, cfg).unwrap();
    let actor: ContextActorRef = spawned.actor();

    let _ = actor.only_context();
}

pub struct DummyActorB;
#[axactor::actor]
impl DummyActorB {
    #[msg]
    async fn test_reply(&self, reply: String) -> String {
        reply
    }

    #[msg]
    async fn test_reply_delay(&self, delay: Duration) -> Duration {
        tokio::time::sleep(delay).await;
        delay
    }
}

#[tokio::test]
async fn test_macro_reply_collision() {
    let system = System::new();
    let spawned = system.spawn(DummyActorB, SpawnConfig::new("reply_collision", 10)).unwrap();
    let actor: DummyActorBRef = spawned.actor();

    let s = "hello_reply".to_string();
    let res = actor.test_reply(s.clone()).await.unwrap();
    assert_eq!(res, s);
}

#[tokio::test]
async fn test_macro_timeout_collision() {
    let system = System::new();
    let spawned = system.spawn(DummyActorB, SpawnConfig::new("timeout_collision", 10)).unwrap();
    let actor: DummyActorBRef = spawned.actor();

    let d = Duration::from_millis(100);

    let res = actor.test_reply_delay_wait_timeout(d, Duration::from_secs(1)).await.unwrap();
    assert_eq!(res, d);
}

#[tokio::test]
async fn test_panicking_factory_cleanup() {
    let system = System::new();

    let cfg = SpawnConfig::new("panicking_factory", 10);

    let _: Result<SpawnHandle<DummyActorBRef>, _> = system.spawn_with(|| {
        panic!("Factory panic");
        #[allow(unreachable_code)]
        DummyActorB
    }, cfg.clone());

    tokio::time::sleep(Duration::from_millis(50)).await;

    let res: Result<SpawnHandle<DummyActorBRef>, _> = system.spawn(DummyActorB, cfg);
    assert!(res.is_ok(), "Name should have been cleaned up after factory panic");
}

#[tokio::test]
async fn test_cancel_safe_meter() {
    let system = System::new();

    pub struct BlockedActor;
    #[axactor::actor]
    impl BlockedActor {
        #[msg]
        async fn block(&mut self) {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    let cfg = SpawnConfig::new("blocked", 1);
    let spawned = system.spawn(BlockedActor, cfg).unwrap();
    let actor: BlockedActorRef = spawned.actor();

    let _ = actor.block();
    let _ = actor.block();

    assert!(actor.mailbox_len() >= 1);

    let fut = actor.block_wait();
    let _ = tokio::time::timeout(Duration::from_millis(50), fut).await;

    assert!(actor.mailbox_len() <= 2, "Meter drifted significantly after cancelled wait");
}

#[tokio::test]
async fn test_ask_wait_timeout_backpressure() {
    let system = System::new();

    pub struct SlowAskActor;
    #[axactor::actor]
    impl SlowAskActor {
        #[msg]
        async fn slow_op(&mut self) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        #[msg]
        async fn slow_ask(&mut self) -> &'static str {
            "ok"
        }
    }

    let cfg = SpawnConfig::new("slow_ask", 1);
    let spawned = system.spawn(SlowAskActor, cfg).unwrap();
    let actor: SlowAskActorRef = spawned.actor();

    for _ in 0..5 {
        let _ = actor.slow_op();
    }

    let res = actor.slow_ask().await;
    assert!(matches!(res, Err(axactor::AskError::Full)));

    let start = std::time::Instant::now();
    let res = actor.slow_ask_wait_timeout(Duration::from_millis(500)).await;
    assert!(res.is_ok());
    assert!(start.elapsed() >= Duration::from_millis(50));
}

#[tokio::test]
async fn test_registry_self_cleanup_race() {
    let system = Arc::new(System::new());
    let registry: Arc<axactor::registry::Registry<String, DummyActorBRef>> = Arc::new(axactor::registry::Registry::new(system.clone()));

    let cfg = SpawnConfig::new("reaped_actor", 10);

    let actor = registry.get_or_spawn("key1".to_string(), cfg.clone(), || DummyActorB).await;

    let _ = actor.stop();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let actor2 = registry.get_or_spawn("key1".to_string(), cfg, || DummyActorB).await;

    assert_eq!(actor2.mailbox_len(), 0);
}

#[tokio::test]

async fn test_spawn_restart_error() {
    let system = System::new();
    let cfg = SpawnConfig::new("no_restart", 10)
        .restart_on_panic(3, Duration::from_millis(10), Duration::from_millis(100));

    let res: Result<SpawnHandle<DummyActorBRef>, _> = system.spawn(DummyActorB, cfg);

    assert!(matches!(res, Err(axactor::SpawnError::NotRestartable)));
}

#[tokio::test]

async fn test_spawn_shutting_down() {
    let system = System::new();
    system.shutdown_graceful(Duration::from_millis(100)).await;

    let cfg = SpawnConfig::new("post_shutdown", 10);
    let res: Result<SpawnHandle<DummyActorBRef>, _> = system.spawn(DummyActorB, cfg);
    assert!(matches!(res, Err(axactor::SpawnError::ShuttingDown)));
}

#[tokio::test]
async fn test_registry_aba_protection() {
    let system = Arc::new(System::new());
    let registry: Arc<axactor::registry::Registry<String, DummyActorBRef>> = Arc::new(axactor::registry::Registry::new(system.clone()));

    let cfg = SpawnConfig::new("aba_actor", 10);

    let actor1 = registry.get_or_spawn("key1".to_string(), cfg.clone(), || DummyActorB).await;

    let actor2 = registry.get_or_spawn("key1".to_string(), cfg.clone(), || DummyActorB).await;
    assert_eq!(actor1.mailbox_len(), 0);
    assert_eq!(actor2.mailbox_len(), 0);
}

#[tokio::test]
async fn test_registry_supervision() {
    let system = Arc::new(System::new());
    let registry: Arc<axactor::registry::Registry<String, PanicActorRef>> = Arc::new(axactor::registry::Registry::new(system.clone()));

    pub struct PanicActor {
        count: Arc<AtomicUsize>,
    }

    #[axactor::actor]
    impl PanicActor {
        #[msg]
        async fn fail(&mut self) {
            self.count.fetch_add(1, Ordering::SeqCst);
            panic!("Registry actor panic");
        }
        #[msg]
        async fn get_count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    let count = Arc::new(AtomicUsize::new(0));
    let cfg = SpawnConfig::new("registry_panicker", 10)
        .restart_on_panic(3, Duration::from_millis(50), Duration::from_secs(1));

    let c = count.clone();
    let actor = registry.get_or_spawn("key1".into(), cfg, move || PanicActor { count: c.clone() }).await;

    let _ = actor.fail();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let c_val = actor.get_count().await.unwrap();
    assert!(c_val >= 1, "Actor should have been initialized at least once");

    system.shutdown_graceful(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn test_reaper_busy_safety() {

    let system = Arc::new(System::new());
    let registry: Arc<axactor::registry::Registry<String, DummyActorBRef>> = Arc::new(axactor::registry::Registry::new(system.clone()));

    registry.start_reaper(Duration::from_millis(100), Duration::from_millis(50));

    let cfg = SpawnConfig::new("busy_actor", 10);
    let actor = registry.get_or_spawn("busy_key".into(), cfg, || DummyActorB).await;

    let d = Duration::from_millis(400);
    let start = Instant::now();
    let res = actor.test_reply_delay_wait(d).await.unwrap();
    assert_eq!(res, d);

    let elapsed = start.elapsed();
    assert!(elapsed >= d);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut reaped = false;
    for _ in 0..5 {
        if actor.test_reply_wait("test".into()).await.is_err() {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
        assert!(reaped, "Actor should have been reaped after becoming truly idle");

    system.shutdown_graceful(Duration::from_secs(1)).await;
}

pub struct LifecycleActor {
    started: Arc<AtomicUsize>,
    restarted: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}

#[axactor::actor]
impl LifecycleActor {
    pub async fn on_start(&mut self, _ctx: &mut axactor::Context) {
        self.started.fetch_add(1, Ordering::SeqCst);
    }
    pub async fn on_restart(&mut self, _ctx: &mut axactor::Context) {
        self.restarted.fetch_add(1, Ordering::SeqCst);
    }
    pub async fn on_stop(&mut self, _ctx: &mut axactor::Context) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }

    #[msg]
    pub async fn fail(&mut self) {
        panic!("Boom");
    }
}

#[tokio::test]
async fn test_actor_lifecycle_hooks() {
    let system = System::new();

    let s = Arc::new(AtomicUsize::new(0));
    let r = Arc::new(AtomicUsize::new(0));
    let st = Arc::new(AtomicUsize::new(0));

    let s2 = s.clone();
    let r2 = r.clone();
    let st2 = st.clone();

    let cfg = SpawnConfig::new("lifecycle", 10)
        .restart_on_panic(1, Duration::from_millis(10), Duration::from_millis(50));

    let spawned = system.spawn_with(move || LifecycleActor {
        started: s2.clone(),
        restarted: r2.clone(),
        stopped: st2.clone(),
    }, cfg).unwrap();
    let actor: LifecycleActorRef = spawned.actor();

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(s.load(Ordering::SeqCst), 1);
    assert_eq!(r.load(Ordering::SeqCst), 0);
    assert_eq!(st.load(Ordering::SeqCst), 0);

    let _ = actor.fail();
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert_eq!(s.load(Ordering::SeqCst), 1);
    assert_eq!(r.load(Ordering::SeqCst), 1);
    assert_eq!(st.load(Ordering::SeqCst), 0, "on_stop should be SKIPPED after crash for safety");

    system.shutdown_graceful(Duration::from_millis(100)).await;
    assert_eq!(st.load(Ordering::SeqCst), 1, "Should have stopped ONCE on shutdown");
}

pub struct HookPanicActor {
    attempts: Arc<AtomicUsize>,
}

#[axactor::actor]
impl HookPanicActor {
    pub async fn on_start(&mut self, _ctx: &mut axactor::Context) {
        let prev = self.attempts.fetch_add(1, Ordering::SeqCst);
        if prev < 2 {
            panic!("Hook Panic in on_start!");
        }
    }

    pub async fn on_restart(&mut self, _ctx: &mut axactor::Context) {
        let prev = self.attempts.fetch_add(1, Ordering::SeqCst);
        if prev < 2 {
            panic!("Hook Panic in on_restart!");
        }
    }

    #[msg]
    pub async fn ping(&self) -> bool { true }
}

#[tokio::test]
async fn test_lifecycle_hook_panic() {
    let system = System::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts2 = attempts.clone();

    let cfg = SpawnConfig::new("hook_panic", 10)
        .restart_on_panic(5, Duration::from_millis(10), Duration::from_millis(50));

    let spawned = system.spawn_with(move || HookPanicActor {
        attempts: attempts2.clone(),
    }, cfg).unwrap();
    let actor: HookPanicActorRef = spawned.actor();

    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(attempts.load(Ordering::SeqCst) >= 3);
    assert_eq!(actor.ping().await.unwrap(), true);
}

#[tokio::test]
async fn test_incarnation_tracking() {
    let system = System::new();

    let cfg = SpawnConfig::new("incarnation", 10)
        .restart_on_panic(5, Duration::from_millis(10), Duration::from_millis(50));

    let spawned = system.spawn_with(|| PanickingActor { count: Arc::new(AtomicUsize::new(0)) }, cfg).unwrap();
    let actor: PanickingActorRef = spawned.actor();

    assert_eq!(actor.incarnation(), 1);

    let _ = actor.do_panic();
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(actor.incarnation(), 2);
}

#[tokio::test]
async fn test_dual_status_monitoring() {
    let system = System::new();
    let spawned = system.spawn(DummyActorB, SpawnConfig::new("status_test", 10)).unwrap();
    let actor: DummyActorBRef = spawned.actor();

    assert!(!actor.is_closed());
    assert!(!actor.is_terminated());

    system.shutdown_graceful(Duration::from_millis(200)).await;

    assert!(actor.is_closed());

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(actor.is_terminated());
    assert!(actor.is_closed());
}

#[tokio::test]
async fn test_restart_message_continuity() {
    let system = System::new();
    let count = Arc::new(AtomicUsize::new(0));

    let cfg = SpawnConfig::new("continuity", 10)
        .restart_on_panic(3, Duration::from_millis(10), Duration::from_millis(50));

    let c = count.clone();
    let spawned = system.spawn_with(move || PanickingActor { count: c.clone() }, cfg).unwrap();
    let actor: PanickingActorRef = spawned.actor();

    assert_eq!(actor.get_count().await.unwrap(), 0);

    let _ = actor.do_panic();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let final_count = actor.get_count().await.unwrap();
    assert!(final_count >= 1);

    system.shutdown_graceful(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn test_ask_panic_cancellation() {
    let system = System::new();

    pub struct ChaosActor;
    #[axactor::actor]
    impl ChaosActor {
        #[msg]
        async fn panic_ask(&self) -> String {
            panic!("Inside ask panic");
        }
    }

    let cfg = SpawnConfig::new("chaos", 10)
        .restart_on_panic(1, Duration::from_millis(10), Duration::from_millis(50));
    let spawned = system.spawn_with(|| ChaosActor, cfg).unwrap();
    let actor: ChaosActorRef = spawned.actor();

    let res = actor.panic_ask().await;

    assert!(res.is_err(), "Ask future should have failed when actor panicked during execution");

    system.shutdown_graceful(Duration::from_millis(100)).await;
}
