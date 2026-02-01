//! Tuner lock management for exclusive/shared access.
//!
//! This module provides a semaphore-based locking mechanism that supports:
//! - **Exclusive lock**: For physical channel selection (blocks all other access)
//! - **Shared lock**: For logical channel selection (multiple clients on same channel)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use thiserror::Error;

use crate::tuner::ChannelKey;

/// Maximum number of shared clients per tuner.
const MAX_SHARED_CLIENTS: u32 = 100;

/// Lock-related errors.
#[derive(Debug, Error)]
pub enum LockError {
    /// Tuner is locked on a different channel.
    #[error("Tuner is locked on different channel")]
    ChannelMismatch,

    /// Tuner needs exclusive lock first (not initialized).
    #[error("Tuner not initialized (needs exclusive lock first)")]
    NotInitialized,

    /// Lock system is closed.
    #[error("Lock system closed")]
    Closed,

    /// Lock acquisition timed out.
    #[error("Lock timeout")]
    Timeout,

    /// Failed to acquire lock.
    #[error("Failed to acquire lock")]
    AcquireFailed,
}

/// Tuner lock manager.
///
/// Uses a semaphore-based approach:
/// - Exclusive lock: Acquires all permits
/// - Shared lock: Acquires one permit (only if on same channel)
pub struct TunerLock {
    /// Semaphore for controlling access.
    semaphore: Arc<Semaphore>,

    /// Maximum permits (for exclusive lock calculation).
    max_permits: u32,

    /// Current channel (for shared lock validation).
    current_channel: RwLock<Option<ChannelKey>>,

    /// Number of shared clients currently connected.
    shared_count: AtomicU32,
}

impl TunerLock {
    /// Create a new tuner lock.
    pub fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(MAX_SHARED_CLIENTS as usize)),
            max_permits: MAX_SHARED_CLIENTS,
            current_channel: RwLock::new(None),
            shared_count: AtomicU32::new(0),
        }
    }

    /// Acquire an exclusive lock (for physical channel selection).
    ///
    /// This blocks until all shared clients have released their locks.
    /// Once acquired, the holder can change the channel.
    pub async fn acquire_exclusive(&self) -> Result<ExclusiveLockGuard<'_>, LockError> {
        // Acquire all permits (blocks until everyone releases)
        let permits = self
            .semaphore
            .clone()
            .acquire_many_owned(self.max_permits)
            .await
            .map_err(|_| LockError::Closed)?;

        Ok(ExclusiveLockGuard {
            permits: Some(permits),
            lock: self,
        })
    }

    /// Try to acquire an exclusive lock without waiting.
    pub fn try_acquire_exclusive(&self) -> Result<ExclusiveLockGuard<'_>, LockError> {
        let permits = self
            .semaphore
            .clone()
            .try_acquire_many_owned(self.max_permits)
            .map_err(|_| LockError::AcquireFailed)?;

        Ok(ExclusiveLockGuard {
            permits: Some(permits),
            lock: self,
        })
    }

    /// Acquire a shared lock (for logical channel selection).
    ///
    /// Only succeeds if the tuner is already tuned to the same channel.
    /// Multiple clients can hold shared locks simultaneously.
    pub async fn acquire_shared(&self, channel: &ChannelKey) -> Result<SharedLockGuard<'_>, LockError> {
        // Check current channel
        {
            let current = self.current_channel.read().await;
            match &*current {
                Some(current_ch) if current_ch == channel => {
                    // Same channel - OK to share
                }
                Some(_) => {
                    // Different channel - cannot share
                    return Err(LockError::ChannelMismatch);
                }
                None => {
                    // Not initialized - need exclusive lock first
                    return Err(LockError::NotInitialized);
                }
            }
        }

        // Acquire one permit
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| LockError::Closed)?;

        self.shared_count.fetch_add(1, Ordering::SeqCst);

        Ok(SharedLockGuard {
            permit: Some(permit),
            lock: self,
        })
    }

    /// Try to acquire a shared lock without waiting.
    pub fn try_acquire_shared(&self, channel: &ChannelKey) -> Result<SharedLockGuard<'_>, LockError> {
        // Check current channel (blocking read is OK for try_acquire)
        let current = self.current_channel.try_read().map_err(|_| LockError::AcquireFailed)?;
        match &*current {
            Some(current_ch) if current_ch == channel => {}
            Some(_) => return Err(LockError::ChannelMismatch),
            None => return Err(LockError::NotInitialized),
        }
        drop(current);

        // Try to acquire one permit
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| LockError::AcquireFailed)?;

        self.shared_count.fetch_add(1, Ordering::SeqCst);

        Ok(SharedLockGuard {
            permit: Some(permit),
            lock: self,
        })
    }

    /// Set the current channel (must hold exclusive lock).
    pub async fn set_channel(&self, channel: ChannelKey) {
        let mut current = self.current_channel.write().await;
        *current = Some(channel);
    }

    /// Clear the current channel.
    pub async fn clear_channel(&self) {
        let mut current = self.current_channel.write().await;
        *current = None;
    }

    /// Get the current channel.
    pub async fn current_channel(&self) -> Option<ChannelKey> {
        self.current_channel.read().await.clone()
    }

    /// Get the number of shared clients.
    pub fn shared_count(&self) -> u32 {
        self.shared_count.load(Ordering::SeqCst)
    }

    /// Check if the tuner is currently locked.
    pub fn is_locked(&self) -> bool {
        self.semaphore.available_permits() < self.max_permits as usize
    }

    /// Downgrade an exclusive lock to a shared lock.
    pub async fn downgrade<'a>(
        mut exclusive: ExclusiveLockGuard<'a>,
        channel: &'a ChannelKey,
    ) -> SharedLockGuard<'a> {
        // Set the channel
        exclusive.lock.set_channel(channel.clone()).await;

        let lock_ref = exclusive.lock;
        let semaphore = lock_ref.semaphore.clone();

        // Drop the exclusive permits (returns max_permits to semaphore)
        let permits = exclusive.permits.take().unwrap();
        drop(permits);

        // Acquire one permit for shared lock (semaphore now has max_permits available)
        let permit = semaphore.try_acquire_owned().expect("Should have permits available after downgrade");

        lock_ref.shared_count.fetch_add(1, Ordering::SeqCst);

        SharedLockGuard {
            permit: Some(permit),
            lock: lock_ref,
        }
    }
}

impl Default for TunerLock {
    fn default() -> Self {
        Self::new()
    }
}

/// Guard for exclusive lock.
pub struct ExclusiveLockGuard<'a> {
    permits: Option<OwnedSemaphorePermit>,
    lock: &'a TunerLock,
}

impl<'a> ExclusiveLockGuard<'a> {
    /// Set the channel while holding the exclusive lock.
    pub async fn set_channel(&self, channel: ChannelKey) {
        self.lock.set_channel(channel).await;
    }

    /// Get a reference to the underlying lock.
    pub fn lock(&self) -> &TunerLock {
        self.lock
    }
}

impl Drop for ExclusiveLockGuard<'_> {
    fn drop(&mut self) {
        // Permits are automatically released when dropped
    }
}

/// Guard for shared lock.
pub struct SharedLockGuard<'a> {
    permit: Option<OwnedSemaphorePermit>,
    lock: &'a TunerLock,
}

impl<'a> SharedLockGuard<'a> {
    /// Get a reference to the underlying lock.
    pub fn lock(&self) -> &TunerLock {
        self.lock
    }
}

impl Drop for SharedLockGuard<'_> {
    fn drop(&mut self) {
        if self.permit.is_some() {
            let count = self.lock.shared_count.fetch_sub(1, Ordering::SeqCst);

            // If this was the last shared client, we could clear the channel
            // but we leave it set for potential future shared clients
            if count == 1 {
                // Last shared client released
                // Channel remains set until next exclusive lock
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_exclusive_lock() {
        let lock = TunerLock::new();

        // Acquire exclusive
        let guard = lock.acquire_exclusive().await.unwrap();
        assert!(lock.is_locked());

        // Set channel
        guard.set_channel(ChannelKey::simple("tuner0", 13)).await;

        // Drop guard
        drop(guard);
        assert!(!lock.is_locked());
    }

    #[tokio::test]
    async fn test_shared_lock() {
        let lock = TunerLock::new();
        let channel = ChannelKey::simple("tuner0", 13);

        // Cannot acquire shared without initialization
        assert!(lock.acquire_shared(&channel).await.is_err());

        // Initialize with exclusive lock
        {
            let exclusive = lock.acquire_exclusive().await.unwrap();
            exclusive.set_channel(channel.clone()).await;
            // Exclusive lock dropped, channel remains set
        }

        // Now we can acquire shared
        let shared1 = lock.acquire_shared(&channel).await.unwrap();
        let shared2 = lock.acquire_shared(&channel).await.unwrap();

        assert_eq!(lock.shared_count(), 2);

        drop(shared1);
        assert_eq!(lock.shared_count(), 1);

        drop(shared2);
        assert_eq!(lock.shared_count(), 0);
    }

    #[tokio::test]
    async fn test_channel_mismatch() {
        let lock = TunerLock::new();
        let channel1 = ChannelKey::simple("tuner0", 13);
        let channel2 = ChannelKey::simple("tuner0", 14);

        // Set up with channel1
        {
            let exclusive = lock.acquire_exclusive().await.unwrap();
            exclusive.set_channel(channel1.clone()).await;
        }

        // Can acquire shared for channel1
        let _guard = lock.acquire_shared(&channel1).await.unwrap();

        // Cannot acquire shared for channel2 (different channel)
        let result = lock.try_acquire_shared(&channel2);
        assert!(matches!(result, Err(LockError::ChannelMismatch)));
    }

    #[tokio::test]
    async fn test_downgrade() {
        let lock = TunerLock::new();
        let channel = ChannelKey::simple("tuner0", 13);

        // Acquire exclusive
        let exclusive = lock.acquire_exclusive().await.unwrap();
        assert!(lock.is_locked());

        // Downgrade to shared
        let shared = TunerLock::downgrade(exclusive, &channel).await;
        assert_eq!(lock.shared_count(), 1);

        // Another client can now join
        let shared2 = lock.acquire_shared(&channel).await.unwrap();
        assert_eq!(lock.shared_count(), 2);

        drop(shared);
        drop(shared2);
        assert_eq!(lock.shared_count(), 0);
    }
}
