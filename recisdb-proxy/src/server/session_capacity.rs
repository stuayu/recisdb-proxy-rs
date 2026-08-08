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
use crate::tuner::channel_key::ChannelKeySpec;

pub(super) use crate::tuner::policy::{
    choose_eviction_target, has_capacity, should_stop_reader_for_capacity,
    should_sync_stop_old_reader,
};

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
pub(super) async fn count_running_instances_on_driver(
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

pub(super) async fn find_lowest_priority_idle_tuner(
    database: &DatabaseHandle,
    tuner_pool: &Arc<TunerPool>,
    session_id: u64,
    tuner_path: &str,
) -> Option<(ChannelKey, i32)> {
    let keys = tuner_pool.keys().await;
    let mut lowest_priority_key: Option<ChannelKey> = None;
    let mut lowest_priority_value = i32::MAX;

    for existing_key in keys.iter() {
        if existing_key.tuner_path != tuner_path {
            continue;
        }

        if let Some(candidate) = tuner_pool.get(existing_key).await {
            if candidate.has_subscribers() {
                debug!(
                    "[Session {}] Skipping {:?} for priority eviction: has {} active subscriber(s)",
                    session_id,
                    existing_key,
                    candidate.subscriber_count()
                );
                continue;
            }
        }

        let (existing_space, existing_channel) = match &existing_key.channel {
            ChannelKeySpec::SpaceChannel { space, channel } => (*space, *channel),
            ChannelKeySpec::Simple(ch) => (0, *ch as u32),
        };

        let existing_priority = {
            let db = database.lock().await;
            db.get_channel_priority(&existing_key.tuner_path, existing_space, existing_channel)
                .unwrap_or(Some(0))
                .unwrap_or(0)
        };

        if existing_priority < lowest_priority_value {
            lowest_priority_value = existing_priority;
            lowest_priority_key = Some(existing_key.clone());
        }
    }

    lowest_priority_key.map(|key| (key, lowest_priority_value))
}


/// Ensure `driver_path` has room for `new_key` by evicting one idle (no-subscriber)
/// tuner on that driver if it is currently at/over capacity. Returns true if there is
/// (now) capacity, false if the driver stayed over capacity because nothing evictable
/// was found.
pub(super) async fn ensure_driver_capacity_with_idle_eviction(
    tuner_pool: &Arc<TunerPool>,
    session_id: u64,
    driver_path: &str,
    new_key: &ChannelKey,
    max_instances: i32,
) -> bool {
    let running = count_running_instances_on_driver(tuner_pool, driver_path, Some(new_key)).await;
    if has_capacity(running, max_instances) {
        return true;
    }

    let keys = tuner_pool.keys().await;
    let mut idle_candidate: Option<Arc<SharedTuner>> = None;
    for key in keys.iter() {
        if key.tuner_path != driver_path || key == new_key {
            continue;
        }
        if let Some(tuner) = tuner_pool.get(key).await {
            // Deliberately `is_running()`, not `occupies_slot()`: only a
            // fully-started reader with no subscribers is "idle" and safe to
            // evict. A `Starting` entry has a caller awaiting its readiness,
            // and a `Stopping` one is already on its way out.
            if tuner.is_running() && !tuner.has_subscribers() {
                idle_candidate = Some(tuner);
                break;
            }
        }
    }

    if let Some(tuner) = idle_candidate {
        let key = tuner.key.clone();
        info!(
            "[Session {}] Evicting idle tuner {:?} to free capacity on driver {} ({}/{})",
            session_id, key, driver_path, running, max_instances
        );
        stop_and_remove_tuner(tuner_pool, &key, tuner, true).await;
        true
    } else {
        debug!(
            "[Session {}] No idle tuner to evict on driver {} ({}/{}), cannot free capacity",
            session_id, driver_path, running, max_instances
        );
        false
    }
}

/// Repeatedly evict the lowest-priority idle tuner on `tuner_path` (never evicting
/// `key`, which is this session's own tuner) until the driver is back at/under
/// `max_instances`, or until no evictable candidate remains.
pub(super) async fn evict_interlopers_until_capacity(
    database: &DatabaseHandle,
    tuner_pool: &Arc<TunerPool>,
    session_id: u64,
    tuner_path: &str,
    key: &ChannelKey,
    max_instances: i32,
) {
    loop {
        let running = count_running_instances_on_driver(tuner_pool, tuner_path, None).await;
        if !should_stop_reader_for_capacity(running, max_instances) {
            break;
        }

        let Some((evict_key, priority)) =
            find_lowest_priority_idle_tuner(database, tuner_pool, session_id, tuner_path).await
        else {
            debug!(
                "[Session {}] Over capacity on {} ({}/{}) but no evictable interloper found",
                session_id, tuner_path, running, max_instances
            );
            break;
        };

        if &evict_key == key {
            debug!(
                "[Session {}] Lowest-priority idle tuner on {} is our own key {:?}; stopping eviction",
                session_id, tuner_path, evict_key
            );
            break;
        }

        if let Some(tuner) = tuner_pool.get(&evict_key).await {
            info!(
                "[Session {}] Evicting interloper {:?} (priority {}) on {} ({}/{})",
                session_id, evict_key, priority, tuner_path, running, max_instances
            );
            stop_and_remove_tuner(tuner_pool, &evict_key, tuner, true).await;
        } else {
            break;
        }
    }
}

// `should_sync_stop_old_reader`'s unit tests moved to `tuner::policy` along
// with the function itself (docs/TUNER_PIPELINE_REDESIGN.md P0).
