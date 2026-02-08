//! Tuner pool for managing shared tuner instances.

use std::collections::HashMap;
use std::sync::Arc;

use log::{debug, info, warn};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::sync::oneshot;

use crate::tuner::channel_key::ChannelKey;
use crate::tuner::shared::SharedTuner;

/// Key for identifying a TS (Transport Stream) for tuner sharing.
/// Used for TSID/SID-based tuner merging.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MuxKey {
    pub driver_id: i64,
    pub nid: u16,
    pub tsid: u16,
}

impl MuxKey {
    pub fn new(driver_id: i64, nid: u16, tsid: u16) -> Self {
        Self {
            driver_id,
            nid,
            tsid,
        }
    }
}

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
}

impl Default for TunerPoolConfig {
    fn default() -> Self {
        Self {
            keep_alive_secs: 60,
            prewarm_enabled: true,
            prewarm_timeout_secs: 30,
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
                            let mut tuners = pool.tuners.write().await;
                            if let Some(current) = tuners.get(&key) {
                                if Arc::ptr_eq(current, &tuner) {
                                    tuners.remove(&key);
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
                self.cancel_idle_close(&key).await;
                debug!("Reusing existing tuner for {:?}", key);
                return Ok(Arc::clone(tuner));
            }
        }

        // Slow path: need to create a new tuner
        let mut tuners = self.tuners.write().await;

        // Double-check after acquiring write lock
        if let Some(tuner) = tuners.get(&key) {
            self.cancel_idle_close(&key).await;
            debug!("Reusing existing tuner for {:?} (after lock)", key);
            return Ok(Arc::clone(tuner));
        }

        // Check capacity
        if tuners.len() >= self.max_tuners {
            // Try to clean up unused tuners first
            tuners.retain(|k, t| {
                if t.has_subscribers() {
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

        // Create the shared tuner wrapper
        let shared = SharedTuner::new(key.clone(), bondriver_version);
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

    /// Clean up tuners with no subscribers.
    pub async fn cleanup(&self) -> usize {
        let mut tuners = self.tuners.write().await;
        let before = tuners.len();
        tuners.retain(|k, t| {
            if t.has_subscribers() {
                true
            } else {
                info!("Cleaning up unused tuner {:?}", k);
                false
            }
        });
        before - tuners.len()
    }

    /// Get all active tuner keys.
    pub async fn keys(&self) -> Vec<ChannelKey> {
        self.tuners.read().await.keys().cloned().collect()
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
        let _rx = tuner.subscribe();
        assert!(tuner.has_subscribers());

        // Cleanup should not remove it (has subscriber)
        pool.cleanup().await;
        assert_eq!(pool.count().await, 1);

        // Unsubscribe
        tuner.unsubscribe();
        assert!(!tuner.has_subscribers());

        // Now cleanup should remove it (no subscribers)
        pool.cleanup().await;
        assert_eq!(pool.count().await, 0);
    }
}
