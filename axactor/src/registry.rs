use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use crate::{MailboxTx, SpawnConfig, RefMetrics, rt::{System, Actor}};
use tokio::sync::OnceCell;

const NUM_SHARDS: usize = 16;

struct RegistryGuard<K, R>
where
    K: Eq + Hash + Clone + Send + 'static,
    R: Clone + Send + Sync + RefMetrics + 'static,
{
    shards: Arc<[Mutex<HashMap<K, Arc<OnceCell<R>>>>; NUM_SHARDS]>,
    shard_idx: usize,
    key: K,
    cell: Arc<OnceCell<R>>,
}

impl<K, R> Drop for RegistryGuard<K, R>
where
    K: Eq + Hash + Clone + Send + 'static,
    R: Clone + Send + Sync + RefMetrics + 'static,
{
    fn drop(&mut self) {
        let mut shard = self.shards[self.shard_idx].lock();
        if let Some(cur) = shard.get(&self.key) {
            if Arc::ptr_eq(cur, &self.cell) {
                shard.remove(&self.key);
            }
        }
    }
}

pub struct Registry<K, R> {
    system: Arc<System>,
    shards: Arc<[Mutex<HashMap<K, Arc<OnceCell<R>>>>; NUM_SHARDS]>,
}

impl<K, R> Registry<K, R>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    R: Clone + Send + Sync + RefMetrics + 'static,
{

    pub fn new(system: Arc<System>) -> Self {
        let shards = std::array::from_fn(|_| Mutex::new(HashMap::new()));

        Self {
            system,
            shards: Arc::new(shards),
        }
    }

    #[inline]
    fn get_shard(&self, key: &K) -> usize {
        let mut s = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut s);
        (s.finish() as usize) % NUM_SHARDS
    }

    pub async fn get_or_spawn<F, A, M>(&self, key: K, mut cfg: SpawnConfig, factory: F) -> R
    where
        F: FnMut() -> A + Send + 'static,
        A: Actor<Msg = M> + Send + 'static,
        M: crate::MessageKind + Send + 'static,

        R: From<MailboxTx<M>>,
    {
        let shard_idx = self.get_shard(&key);
        let cell = {
            let mut shard = self.shards[shard_idx].lock();
            shard.entry(key.clone()).or_insert_with(|| Arc::new(OnceCell::new())).clone()
        };

        let shards_for_guard = self.shards.clone();
        let key_for_guard = key.clone();

        let base_name = cfg.name.clone();
        cfg.name = hashed_name(&base_name, shard_idx, &key);

        let factory = Arc::new(Mutex::new(factory));

        let res = cell.get_or_init(|| {
            let factory_inner = factory.clone();
            let shards_for_guard = shards_for_guard.clone();
            let key_for_guard = key_for_guard.clone();
            let cell_for_guard = cell.clone();
            let system = self.system.clone();
            let mut cfg = cfg.clone();

            async move {

                let guard = Arc::new(RegistryGuard {
                    shards: shards_for_guard,
                    shard_idx,
                    key: key_for_guard,
                    cell: cell_for_guard,
                });

                cfg.push_guard(guard);

                let handle = system.spawn_with(move || {
                    let mut f = factory_inner.lock();
                    (&mut *f)()
                }, cfg).expect("Global actor name collision in registry");
                handle.actor()
            }
        }).await;

        res.clone()
    }

    pub fn start_reaper(&self, idle_timeout: Duration, check_interval: Duration) {
        let shards = self.shards.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(check_interval);
            loop {
                interval.tick().await;

                for shard_idx in 0..NUM_SHARDS {
                    let refs_to_stop: Vec<R> = {
                        let shard = shards[shard_idx].lock();
                        shard.values()
                            .filter_map(|cell| cell.get().cloned())
                            .filter(|r| r.mailbox_len() == 0 && r.inflight_count() == 0 && r.idle_for() >= idle_timeout)
                            .collect()

                    };

                    if !refs_to_stop.is_empty() {
                        for r in refs_to_stop {
                            let _ = r.stop();
                        }
                        tracing::info!(shard = shard_idx, "Registry reaper triggered stops for idle actors");
                    }
                }

            }
        });
    }
}

fn hashed_name<K: Hash>(base: &str, shard: usize, key: &K) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    format!("{}:{}:{:016x}", base, shard, h.finish())
}
