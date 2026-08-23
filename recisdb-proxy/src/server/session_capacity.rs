//! Capacity and eviction policy helpers for BNDP sessions.
//!
//! The pure predicates/eviction-choice functions that used to live here
//! (and their unit tests) moved to `tuner::policy` as part of
//! docs/TUNER_PIPELINE_REDESIGN.md P0 — re-exported below under their old
//! names/visibility so the async DB/pool orchestration in this module (none
//! of which is pure) keeps compiling unchanged.

use std::sync::Arc;

use log::{debug, info};

use crate::server::listener::DatabaseHandle;
use crate::tuner::{ChannelKey, SharedTuner, TunerPool};

pub(super) use crate::tuner::policy::should_sync_stop_old_reader;

pub(super) async fn driver_max_instances(
    database: &DatabaseHandle,
    tuner_path: &str,
) -> i32 {
    let db = database.lock().await;
    db.get_max_instances_for_path(tuner_path).unwrap_or(1)
}

/// Count how many tuner slots on `tuner_path` are currently taken.
///
/// Counts `occupies_slot()` (Starting/Running/Stopping), **not**
/// `is_running()`: a reader that is still opening the BonDriver and running
/// its SetChannel retries already holds the DLL/device, and that startup can
/// take up to `set_channel_retry_timeout_ms`. Counting only `Running` here
/// would undercount for the whole init window and let a second reader be
/// started over `max_instances`. (Before `ReaderState` existed, the old
/// `is_running` flag was set to `true` at the very top of the reader body,
/// so it covered the init window too — `occupies_slot()` preserves that.)
pub(crate) async fn count_running_instances_on_driver(
    tuner_pool: &Arc<TunerPool>,
    tuner_path: &str,
    exclude_key: Option<&ChannelKey>,
) -> i32 {
    let keys = tuner_pool.keys().await;
    let mut running_instances = 0i32;
    for key in &keys {
        if key.tuner_path != tuner_path {
            continue;
        }
        if exclude_key == Some(key) {
            continue;
        }
        if let Some(tuner) = tuner_pool.get(key).await {
            if tuner.occupies_slot() {
                running_instances += 1;
            }
        }
    }
    running_instances
}

pub(super) async fn stop_and_remove_tuner(
    tuner_pool: &Arc<TunerPool>,
    key: &ChannelKey,
    tuner: Arc<SharedTuner>,
    wait_for_stop: bool,
) {
    tuner_pool.cancel_idle_close(key).await;
    tuner.stop_reader().await;

    if wait_for_stop {
        // Wait for the slot to actually be released, not merely for TS data
        // to stop flowing: the caller's whole reason to wait is that it is
        // about to open the same DLL/device.
        let mut wait_attempts = 0;
        while tuner.occupies_slot() && wait_attempts < 50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            wait_attempts += 1;
        }
    }

    tuner_pool.remove(key).await;
}

pub(super) async fn cleanup_unused_tuner_after_switch(
    database: &DatabaseHandle,
    tuner_pool: &Arc<TunerPool>,
    session_id: u64,
    tuner: Arc<SharedTuner>,
    replacement_tuner_path: Option<&str>,
    force_stop_same_dll: bool,
    log_prefix: &str,
) {
    if tuner.subscriber_count() != 0 {
        return;
    }

    // `occupies_slot()`, not `is_running()`: an entry that is still
    // `Starting` has an in-flight reader startup behind it and must not be
    // treated as "already stopped" and yanked out of the pool.
    if !tuner.occupies_slot() {
        debug!(
            "[Session {}] {} {:?} already stopped, ensuring pool cleanup",
            session_id, log_prefix, tuner.key
        );
        tuner_pool.remove(&tuner.key).await;
        return;
    }

    let old_dll_max = driver_max_instances(database, &tuner.key.tuner_path).await;
    let old_dll_running = count_running_instances_on_driver(
        tuner_pool,
        &tuner.key.tuner_path,
        None,
    )
    .await;
    let same_dll_switch = replacement_tuner_path == Some(tuner.key.tuner_path.as_str());

    if should_sync_stop_old_reader(same_dll_switch, force_stop_same_dll, old_dll_running, old_dll_max) {
        info!(
            "[Session {}] {} stopping old reader for {:?} ({}/{})",
            session_id, log_prefix, tuner.key, old_dll_running, old_dll_max
        );
        let key = tuner.key.clone();
        stop_and_remove_tuner(tuner_pool, &key, tuner, false).await;
    } else {
        info!(
            "[Session {}] {} scheduling idle close for {:?} ({}/{})",
            session_id, log_prefix, tuner.key, old_dll_running, old_dll_max
        );
        tuner_pool.schedule_idle_close(tuner.key.clone(), tuner).await;
    }
}


// `should_sync_stop_old_reader`'s unit tests moved to `tuner::policy` along
// with the function itself (docs/TUNER_PIPELINE_REDESIGN.md P0).
