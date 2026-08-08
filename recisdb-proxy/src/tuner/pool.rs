//! Tuner pool for managing shared tuner instances.

use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, info, warn};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::sync::oneshot;

use crate::tuner::channel_key::ChannelKey;
use crate::tuner::shared::SharedTuner;

/// Priority levels for tuner requests.
pub mod priority {
    pub const SCAN: u8 = 0;
    pub const VIEWING: u8 = 10;
    pub const RECORDING_NORMAL: u8 = 200;
    pub const RECORDING_EXCLUSIVE: u8 = 255;
}

/// Error type for tuner pool operations.
#[derive(Debug, thiserror::Error)]
pub enum TunerPoolError {
    /// Failed to open the tuner.
    #[error("Failed to open tuner: {0}")]
    OpenFailed(String),

    /// Failed to tune to the channel.
    #[error("Failed to tune to channel: {0}")]
    TuneFailed(String),

    /// Tuner not found.
    #[error("Tuner not found: {0}")]
    NotFound(String),
}

/// Tuner pool configuration for optimization behavior.
#[derive(Debug, Clone)]
pub struct TunerPoolConfig {
    pub keep_alive_secs: u64,
    pub prewarm_enabled: bool,
    pub prewarm_timeout_secs: u64,
    pub set_channel_retry_interval_ms: u64,
    pub set_channel_retry_timeout_ms: u64,
    pub signal_poll_interval_ms: u64,
    pub signal_wait_timeout_ms: u64,
}

impl Default for TunerPoolConfig {
    fn default() -> Self {
        Self {
            keep_alive_secs: 60,
            prewarm_enabled: true,
            prewarm_timeout_secs: 30,
            set_channel_retry_interval_ms: 500,
            set_channel_retry_timeout_ms: 10_000,
            signal_poll_interval_ms: 500,
            signal_wait_timeout_ms: 10_000,
        }
    }
}

/// Pool of shared tuner instances.
///
/// Manages tuner lifecycle and enables channel sharing between clients.
pub struct TunerPool {
    /// Map of channel keys to shared tuner instances.
    tuners: RwLock<HashMap<ChannelKey, Arc<SharedTuner>>>,
    /// Pending idle-close tasks.
    idle_tasks: Mutex<HashMap<ChannelKey, IdleHandle>>,
    /// Maximum number of concurrent tuner instances.
    max_tuners: usize,
    /// Tuner optimization configuration.
    config: RwLock<TunerPoolConfig>,
    /// Per-DLL initialization locks.
    ///
    /// Serializes `CreateBonDriver + OpenTuner + SetChannel` sequences on the
    /// same DLL path.  Many BonDriver DLLs use global/static state internally
    /// (singleton `IBonDriver*`); concurrent initialization from multiple
    /// `spawn_blocking` threads can corrupt that state and cause one reader to
    /// "steal" another's channel.  The lock is held only during the init phase
    /// (up to ~10 s); the reader loop runs without it.
    dll_init_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

struct IdleHandle {
    cancel_tx: oneshot::Sender<()>,
}

impl TunerPool {
    /// Create a new tuner pool.
    pub fn new(max_tuners: usize) -> Self {
        Self::new_with_config(max_tuners, TunerPoolConfig::default())
    }

    /// Create a new tuner pool with configuration.
    pub fn new_with_config(max_tuners: usize, config: TunerPoolConfig) -> Self {
        Self {
            tuners: RwLock::new(HashMap::new()),
            idle_tasks: Mutex::new(HashMap::new()),
            max_tuners,
            config: RwLock::new(config),
            dll_init_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Update tuner optimization configuration.
    pub async fn update_config(self: &Arc<Self>, config: TunerPoolConfig) {
        let old_keep_alive = {
            let mut guard = self.config.write().await;
            let old = guard.keep_alive_secs;
            *guard = config.clone();
            old
        };

        if old_keep_alive != config.keep_alive_secs {
            self.cancel_all_idle().await;

            let idle_tuners: Vec<(ChannelKey, Arc<SharedTuner>)> = {
                let tuners = self.tuners.read().await;
                tuners
                    .iter()
                    .filter(|(_, tuner)| !tuner.has_subscribers())
                    .map(|(key, tuner)| (key.clone(), Arc::clone(tuner)))
                    .collect()
            };

            for (key, tuner) in idle_tuners {
                self.schedule_idle_close(key, tuner).await;
            }
        }
    }

    /// Get current tuner optimization configuration.
    pub async fn config(&self) -> TunerPoolConfig {
        self.config.read().await.clone()
    }

    /// Acquire a per-DLL initialization lock.
    ///
    /// Returns an `OwnedMutexGuard` that serializes BonDriver DLL operations
    /// (CreateBonDriver + OpenTuner + SetChannel) for the given DLL path.
    /// The guard should be held during `start_bondriver_reader` or
    /// `WarmTunerHandle::activate` and dropped once the reader is confirmed
    /// ready, so that subsequent initializations on the same DLL do not
    /// overlap.
    pub async fn acquire_dll_init_lock(&self, dll_path: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.dll_init_locks.lock().await;
            locks.entry(dll_path.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }

    /// Cancel an idle-close timer if it exists.
    pub async fn cancel_idle_close(&self, key: &ChannelKey) {
        let mut idle_tasks = self.idle_tasks.lock().await;
        if let Some(handle) = idle_tasks.remove(key) {
            let _ = handle.cancel_tx.send(());
        }
    }

    /// Cancel all idle-close timers.
    pub async fn cancel_all_idle(&self) {
        let mut idle_tasks = self.idle_tasks.lock().await;
        for (_, handle) in idle_tasks.drain() {
            let _ = handle.cancel_tx.send(());
        }
    }

    /// Schedule a delayed close when the tuner becomes idle.
    pub async fn schedule_idle_close(self: &Arc<Self>, key: ChannelKey, tuner: Arc<SharedTuner>) {
        let keep_alive_secs = self.config.read().await.keep_alive_secs;
        if keep_alive_secs == 0 {
            info!("Keep-alive disabled, stopping reader for {:?}", key);
            tuner.stop_reader().await;
            let _ = self.remove(&key).await;
            return;
        }

        {
            let idle_tasks = self.idle_tasks.lock().await;
            if idle_tasks.contains_key(&key) {
                info!("Keep-alive already scheduled for {:?}", key);
                return;
            }
        }

        self.cancel_idle_close(&key).await;

        info!("Scheduling keep-alive close in {}s for {:?}", keep_alive_secs, key);

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        {
            let mut idle_tasks = self.idle_tasks.lock().await;
            idle_tasks.insert(key.clone(), IdleHandle { cancel_tx });
        }

        let pool = Arc::downgrade(self);
        tokio::spawn(async move {
            let sleep = tokio::time::sleep(std::time::Duration::from_secs(keep_alive_secs));
            tokio::pin!(sleep);

            tokio::select! {
                _ = &mut sleep => {
                    if let Some(pool) = pool.upgrade() {
                        if !tuner.has_subscribers() {
                            info!("Keep-alive timeout reached, stopping reader for {:?}", key);
                            tuner.stop_reader().await;
                            // ★ Bug F revised fix: Always remove the pool entry after stop_reader().
                            // stop_reader() is async and yields; a concurrent subscribe() +
                            // cancel_idle_close() may have run during that await window.
                            // The reader is now dead regardless.  Leaving a stopped entry in
                            // the pool would cause the reuse path in SetChannelSpace to find
                            // an is_running()==false SharedTuner, and get_or_create() would
                            // return it without restarting the reader — resulting in subscribers
                            // that never receive data.
                            // By removing the entry, the next SetChannelSpace will create a
                            // fresh SharedTuner and start a new reader via get_or_create().
                            //
                            // `stop_reader()` always leaves `state() ==
                            // Stopped` (P1), so at this point
                            // `tuner.is_reclaimable() == !tuner.has_subscribers()`
                            // exactly — the two checks below are equivalent
                            // to a single `is_reclaimable()` call, spelled
                            // out for the warn-log branch.
                            if tuner.has_subscribers() {
                                warn!("Keep-alive: subscriber appeared during stop_reader for {:?}; \
                                       removing stale pool entry (reader is stopped, new reader will be created on next access)",
                                      key);
                            }
                            {
                                let mut tuners = pool.tuners.write().await;
                                if let Some(current) = tuners.get(&key) {
                                    if Arc::ptr_eq(current, &tuner) {
                                        tuners.remove(&key);
                                    }
                                }
                            }
                        } else {
                            info!("Keep-alive timeout reached but subscribers present for {:?}", key);
                        }
                        let mut idle_tasks = pool.idle_tasks.lock().await;
                        idle_tasks.remove(&key);
                    }
                }
                _ = cancel_rx => {
                    if let Some(pool) = pool.upgrade() {
                        info!("Keep-alive close canceled for {:?}", key);
                        let mut idle_tasks = pool.idle_tasks.lock().await;
                        idle_tasks.remove(&key);
                    }
                }
            }
        });
    }

    /// Get an existing shared tuner for the given key, if one exists.
    pub async fn get(&self, key: &ChannelKey) -> Option<Arc<SharedTuner>> {
        self.tuners.read().await.get(key).cloned()
    }

    /// Get or create a shared tuner for the given key.
    ///
    /// If a tuner for this key already exists, it is returned.
    /// Otherwise, the factory function is called to create a new tuner.
    pub async fn get_or_create<F, Fut>(
        &self,
        key: ChannelKey,
        bondriver_version: u8,
        factory: F,
    ) -> Result<Arc<SharedTuner>, TunerPoolError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), TunerPoolError>>,
    {
        // Fast path: check if tuner already exists
        {
            let tuners = self.tuners.read().await;
            if let Some(tuner) = tuners.get(&key) {
                // ★ Evict stale entries: if the reader has stopped (or never
                //   started) and there are no subscribers, this entry was
                //   left over from an idle-close race. `is_reclaimable()`
                //   deliberately excludes `ReaderState::Starting` — a reader
                //   still mid-startup is not stale (SYSTEM_REVIEW_2026-07.md
                //   M8). Drop the read lock and remove it so we create a
                //   fresh one.
                if tuner.is_reclaimable() {
                    drop(tuners);
                    warn!("get_or_create: evicting stale tuner for {:?}", key);
                    self.cancel_idle_close(&key).await;
                    let mut w = self.tuners.write().await;
                    if let Some(current) = w.get(&key) {
                        if current.is_reclaimable() {
                            w.remove(&key);
                        }
                    }
                    drop(w);
                    // fall through to slow-path creation below
                } else {
                    self.cancel_idle_close(&key).await;
                    debug!("Reusing existing tuner for {:?}", key);
                    return Ok(Arc::clone(tuner));
                }
            }
        }

        // Slow path: need to create a new tuner
        let mut tuners = self.tuners.write().await;

        // Double-check after acquiring write lock
        if let Some(tuner) = tuners.get(&key) {
            // Same stale check under write lock
            if tuner.is_reclaimable() {
                warn!("get_or_create: evicting stale tuner for {:?} (under write lock)", key);
                self.cancel_idle_close(&key).await;
                tuners.remove(&key);
            } else {
                self.cancel_idle_close(&key).await;
                debug!("Reusing existing tuner for {:?} (after lock)", key);
                return Ok(Arc::clone(tuner));
            }
        }

        // Check capacity
        if tuners.len() >= self.max_tuners {
            // Try to clean up unused tuners first. Entries that are merely
            // `Starting` (occupying a slot, no subscribers yet) must survive
            // this — only genuinely reclaimable entries are removed.
            tuners.retain(|k, t| {
                if t.has_subscribers() || t.occupies_slot() {
                    true
                } else {
                    info!("Removing unused tuner {:?}", k);
                    false
                }
            });

            if tuners.len() >= self.max_tuners {
                warn!(
                    "Tuner pool at capacity ({}/{}), cannot create new tuner",
                    tuners.len(),
                    self.max_tuners
                );
                return Err(TunerPoolError::OpenFailed(
                    "Tuner pool at capacity".to_string(),
                ));
            }
        }

        // Create the tuner via the factory
        factory().await?;

        // Create the shared tuner wrapper. Mark it `Reserved` immediately —
        // before it is even inserted into the map — so no concurrent
        // `cleanup`/`evict_idle_on_path`/capacity-retain call can ever
        // observe it as `Idle` (reclaimable) between insertion and whichever
        // caller-side `start_bondriver_reader`/`WarmTunerHandle::activate`
        // call is about to happen (SYSTEM_REVIEW_2026-07.md M8).
        //
        // `Reserved`, not `Starting`: no reader is in flight yet, and the
        // caller still has to start one — see `ReaderState::Reserved`. A
        // caller that gives up instead (capacity conflict, error) owns the
        // job of removing the entry again (`SharedTuner::is_orphanable`).
        let shared = SharedTuner::new(key.clone(), bondriver_version);
        shared.set_state(crate::tuner::shared::ReaderState::Reserved);
        info!("Created new shared tuner for {:?}", key);

        tuners.insert(key, Arc::clone(&shared));
        Ok(shared)
    }

    /// Remove a tuner from the pool.
    pub async fn remove(&self, key: &ChannelKey) -> Option<Arc<SharedTuner>> {
        let mut tuners = self.tuners.write().await;
        let removed = tuners.remove(key);
        if removed.is_some() {
            info!("Removed tuner {:?} from pool", key);
        }
        removed
    }

    /// Get the number of active tuners in the pool.
    pub async fn count(&self) -> usize {
        self.tuners.read().await.len()
    }

    /// Clean up stale tuners: no subscribers *and* not occupying a slot
    /// (`ReaderState::Idle`/`Stopped`). A `Starting` entry with no
    /// subscribers yet is deliberately left alone — see
    /// `SharedTuner::is_reclaimable`'s doc comment (SYSTEM_REVIEW_2026-07.md
    /// M8: this used to remove *any* subscriber-less entry, including one
    /// whose reader was still mid-startup).
    pub async fn cleanup(&self) -> usize {
        let mut tuners = self.tuners.write().await;
        let before = tuners.len();
        tuners.retain(|k, t| {
            if t.is_reclaimable() {
                info!("Cleaning up unused tuner {:?}", k);
                false
            } else {
                true
            }
        });
        before - tuners.len()
    }

    /// Get all active tuner keys.
    pub async fn keys(&self) -> Vec<ChannelKey> {
        self.tuners.read().await.keys().cloned().collect()
    }

    /// Evict all idle (running, no-subscriber) tuners on `tuner_path`, except
    /// `except`.
    ///
    /// Unlike [`Self::cleanup`] (which only removes *stopped* tuners with no
    /// subscribers), this stops and removes tuners that are still *running*
    /// but currently have zero subscribers — i.e. readers kept warm only by
    /// `schedule_idle_close`'s keep-alive window. This exists for physical
    /// devices that allow only a single concurrent `open()` per device path
    /// (e.g. px4-drv character devices returning `EALREADY`/errno 114 on a
    /// second open): an idle-but-still-open reader on the same path blocks a
    /// brand new one from opening at all, so it must be evicted before retrying,
    /// not just left to expire on its own keep-alive timer.
    ///
    /// Returns the number of tuners stopped and removed.
    pub async fn evict_idle_on_path(
        self: &Arc<Self>,
        tuner_path: &str,
        except: Option<&ChannelKey>,
    ) -> usize {
        let keys = self.keys().await;
        let mut evicted = 0usize;

        for key in keys {
            if key.tuner_path != tuner_path {
                continue;
            }
            if except == Some(&key) {
                continue;
            }

            let Some(tuner) = self.get(&key).await else {
                continue;
            };
            // Deliberately `state() == Running` (i.e. `is_running()`), not
            // `occupies_slot()`: a `Starting` entry has not finished opening
            // the DLL yet, so stopping it here would race the in-flight
            // `SetChannel` rather than free up an already-idle reader.
            if !tuner.is_running() || tuner.has_subscribers() {
                continue;
            }

            info!(
                "evict_idle_on_path: evicting idle reader {:?} to free device path {}",
                key, tuner_path
            );
            self.cancel_idle_close(&key).await;
            // stop_reader() is async and yields; a concurrent subscribe() may
            // have raced in during that await window, so re-check identity
            // (not just presence) before removing the pool entry — mirrors
            // the same pattern in schedule_idle_close() above.
            tuner.stop_reader().await;
            {
                let mut tuners = self.tuners.write().await;
                if let Some(current) = tuners.get(&key) {
                    if Arc::ptr_eq(current, &tuner) {
                        tuners.remove(&key);
                    }
                }
            }
            evicted += 1;
        }

        evicted
    }
}

impl Default for TunerPool {
    fn default() -> Self {
        Self::new(16) // Default to 16 concurrent tuners
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuner::shared::{ReaderStartupConfig, ReaderState};
    use crate::tuner::ts_source::FakeTsSource;

    /// Fast, deterministic startup timings for `spawn_fake_reader` in tests
    /// (mirrors `shared.rs`'s private `test_startup_config`, duplicated here
    /// since that helper isn't `pub(crate)` and pool tests need their own).
    fn fast_startup_config() -> ReaderStartupConfig {
        ReaderStartupConfig {
            set_channel_retry_interval_ms: 5,
            set_channel_retry_timeout_ms: 50,
            signal_poll_interval_ms: 5,
            signal_wait_timeout_ms: 50,
        }
    }

    #[tokio::test]
    async fn test_pool_cleanup() {
        let pool = TunerPool::new(10);
        let key = ChannelKey::simple("/dev/test", 1);

        // Create a tuner
        let tuner = pool
            .get_or_create(key.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();

        assert_eq!(pool.count().await, 1);

        // Subscribe to keep the tuner active
        let rx = tuner.subscribe();
        assert!(tuner.has_subscribers());

        // Cleanup should not remove it (has subscriber)
        pool.cleanup().await;
        assert_eq!(pool.count().await, 1);

        // Unsubscribe (RAII: dropping the `TunerSubscription`)
        drop(rx);
        assert!(!tuner.has_subscribers());

        // Still `Reserved` (get_or_create handed the entry over but no reader
        // was ever started) and now subscriber-less: cleanup() must NOT
        // remove it — this is the M8 fix (SYSTEM_REVIEW_2026-07.md): an entry
        // whose reader start is still owed/in flight is not stale just
        // because nothing has subscribed to it yet.
        assert_eq!(tuner.state(), ReaderState::Reserved);
        pool.cleanup().await;
        assert_eq!(pool.count().await, 1, "a Starting entry must survive cleanup() even with 0 subscribers");

        // Once the (fake) reader actually stops, the entry becomes
        // reclaimable and cleanup() removes it.
        tuner.stop_reader().await;
        assert_eq!(tuner.state(), ReaderState::Stopped);
        pool.cleanup().await;
        assert_eq!(pool.count().await, 0);
    }

    /// A `Reserved` entry occupies its slot but still owes a reader start,
    /// and its owner can hand it back. This is the leak guard for the
    /// abandon paths (capacity conflict detected *after* `get_or_create`:
    /// `session::remove_orphaned_tuner_if_unused`,
    /// `channel_resolve::start_tuner_for_service`'s `Busy` return).
    #[tokio::test]
    async fn reserved_entry_occupies_a_slot_but_is_orphanable_by_its_owner() {
        let pool = TunerPool::new(4);
        let key = ChannelKey::simple("/dev/test", 1);

        let tuner = pool
            .get_or_create(key.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();

        assert_eq!(tuner.state(), ReaderState::Reserved);
        assert!(tuner.occupies_slot(), "Reserved must count against driver capacity");
        assert!(tuner.needs_reader_start(), "nobody has started a reader yet");
        assert!(!tuner.is_reclaimable(), "another task must not sweep it away (M8)");
        assert!(tuner.is_orphanable(), "but its own creator may hand it back");

        pool.remove(&key).await;
        assert_eq!(pool.count().await, 0);
    }

    /// Once a reader start is in flight (`Starting`) or live (`Running`), no
    /// second start may be issued for the same entry — that would open the
    /// same DLL twice.
    #[tokio::test]
    async fn starting_and_running_entries_do_not_need_another_reader_start() {
        let pool = TunerPool::new(4);
        let key = ChannelKey::simple("/dev/test", 1);
        let tuner = pool
            .get_or_create(key.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();

        let source = FakeTsSource::new().with_startup_delay(std::time::Duration::from_millis(200));
        let _ready_rx = tuner.spawn_fake_reader(source, 0, 1, fast_startup_config()).await;
        assert_eq!(tuner.state(), ReaderState::Starting);
        assert!(!tuner.needs_reader_start(), "a start is already in flight");
        assert!(!tuner.is_orphanable(), "an in-flight start owns this entry");

        tuner.stop_reader().await;
        assert_eq!(tuner.state(), ReaderState::Stopped);
        assert!(tuner.needs_reader_start(), "a stopped entry may be started again");
    }

    /// M8 (SYSTEM_REVIEW_2026-07.md): a freshly created entry, still
    /// `Reserved`, must not be evicted by `get_or_create`'s own
    /// capacity-pressure retain pass just because it has no subscribers yet.
    #[tokio::test]
    async fn get_or_create_capacity_retain_does_not_evict_reserved_entry() {
        let pool = TunerPool::new(1); // capacity 1 forces the retain path below
        let key_a = ChannelKey::simple("/dev/test-a", 1);
        let key_b = ChannelKey::simple("/dev/test-b", 1);

        let tuner_a = pool
            .get_or_create(key_a.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(tuner_a.state(), ReaderState::Reserved);
        assert!(!tuner_a.has_subscribers());

        // At capacity (1/1) with a second, different key: get_or_create's
        // capacity-retain pass runs. Before the M8 fix this would have
        // dropped tuner_a (no subscribers, and `is_running()` was `false`
        // for a not-yet-started entry too) purely because it "looked" idle.
        let result = pool.get_or_create(key_b.clone(), 2, || async { Ok(()) }).await;
        assert!(result.is_err(), "still at capacity: Reserved entry must count as occupying its slot");
        assert_eq!(pool.count().await, 1, "tuner_a must not have been evicted while Reserved");
        assert!(pool.get(&key_a).await.is_some());
    }

    /// M8 corollary: `evict_idle_on_path` must never touch a `Starting`
    /// entry (it isn't even open yet, let alone idle) — driven through a
    /// real `spawn_fake_reader` with a long startup delay so the entry is
    /// genuinely mid-`SetChannel` when eviction is attempted.
    #[tokio::test]
    async fn evict_idle_on_path_does_not_touch_starting_entry() {
        let pool = Arc::new(TunerPool::new(10));
        let key = ChannelKey::simple("/dev/px4video0", 1);

        let tuner = pool
            .get_or_create(key.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();
        let source = FakeTsSource::new().with_startup_delay(std::time::Duration::from_millis(200));
        let _ready_rx = tuner.spawn_fake_reader(source, 0, 1, fast_startup_config()).await;
        assert_eq!(tuner.state(), ReaderState::Starting);
        assert!(!tuner.has_subscribers());

        let evicted = pool.evict_idle_on_path("/dev/px4video0", None).await;
        assert_eq!(evicted, 0, "a Starting reader must not be evicted as if it were idle");
        assert_eq!(pool.count().await, 1);
        assert_eq!(tuner.state(), ReaderState::Starting, "eviction must not have touched the in-flight startup");

        // Every test that spawns a fake reader must stop and join it before
        // returning — an un-joined `spawn_blocking` task otherwise wedges
        // the whole test binary's shutdown (`BlockingPool::shutdown` waits
        // for it indefinitely). `stop_reader()` requests the stop now, while
        // the fake is still mid-`set_channel` (`Starting`); the reader must
        // honor that instead of clobbering it back to `Running` once its
        // startup delay elapses (see `SharedTuner::try_transition_starting_to_running`).
        tuner.stop_reader().await;
        assert_eq!(tuner.state(), ReaderState::Stopped);
    }

    /// The real "stop a running reader" branch of `evict_idle_on_path`,
    /// exercised end-to-end via `FakeTsSource` — previously untestable (see
    /// the removed comment above this test) because there was no way to
    /// drive a `SharedTuner` into a genuinely running state without a real
    /// BonDriver DLL.
    #[tokio::test]
    async fn evict_idle_on_path_stops_and_removes_running_idle_reader() {
        let pool = Arc::new(TunerPool::new(10));
        let key = ChannelKey::simple("/dev/px4video0", 1);

        let tuner = pool
            .get_or_create(key.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();
        let ready_rx = tuner
            .spawn_fake_reader(FakeTsSource::new(), 0, 1, fast_startup_config())
            .await;
        ready_rx.await.unwrap().unwrap();
        assert_eq!(tuner.state(), ReaderState::Running);
        assert!(!tuner.has_subscribers(), "no one has subscribed yet -> eligible for idle eviction");

        let evicted = pool.evict_idle_on_path("/dev/px4video0", None).await;
        assert_eq!(evicted, 1);
        assert_eq!(pool.count().await, 0);
        assert_eq!(tuner.state(), ReaderState::Stopped);
    }

    #[tokio::test]
    async fn evict_idle_on_path_is_noop_when_nothing_on_path() {
        let pool = Arc::new(TunerPool::new(10));
        let evicted = pool.evict_idle_on_path("/dev/px4video0", None).await;
        assert_eq!(evicted, 0);
    }

    #[tokio::test]
    async fn evict_idle_on_path_ignores_other_paths_and_subscribed_entries() {
        let pool = Arc::new(TunerPool::new(10));
        let key_a = ChannelKey::simple("/dev/px4video0", 1);
        let key_b = ChannelKey::simple("/dev/px4video1", 1);

        let tuner_a = pool
            .get_or_create(key_a.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();
        // Keep tuner_a alive across get_or_create's own stale-entry eviction
        // (a Starting/Stopped entry with no subscribers is otherwise treated
        // as stale) by giving it a subscriber, matching the pattern the
        // existing `test_pool_cleanup` test above uses.
        let _sub_a = tuner_a.subscribe();

        let tuner_b = pool
            .get_or_create(key_b.clone(), 2, || async { Ok(()) })
            .await
            .unwrap();
        let _sub_b = tuner_b.subscribe();

        assert_eq!(pool.count().await, 2);

        // Neither entry is running (`Starting`, no real reader started), so
        // evict_idle_on_path must not touch either regardless of path match:
        // key_a's own path is excluded by `!is_running()`, key_b's by both
        // `!is_running()` and the path filter.
        let evicted = pool.evict_idle_on_path("/dev/px4video0", None).await;
        assert_eq!(evicted, 0);
        assert_eq!(pool.count().await, 2);

        // `except` filtering: even a hypothetical self-match must be skipped.
        let evicted = pool.evict_idle_on_path("/dev/px4video0", Some(&key_a)).await;
        assert_eq!(evicted, 0);
        assert_eq!(pool.count().await, 2);
    }
}
