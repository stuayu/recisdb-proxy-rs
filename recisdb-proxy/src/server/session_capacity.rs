//! Capacity and eviction policy helpers for BNDP sessions.

use std::sync::Arc;

use log::{debug, info};

use crate::server::listener::DatabaseHandle;
use crate::tuner::{ChannelKey, SharedTuner, TunerPool};
use crate::tuner::channel_key::ChannelKeySpec;

pub(super) type EvictionCandidate = (ChannelKey, i32, bool);

pub(super) fn has_capacity(running_instances: i32, max_instances: i32) -> bool {
    running_instances < max_instances
}

pub(super) fn should_stop_reader_for_capacity(
    running_instances: i32,
    max_instances: i32,
) -> bool {
    running_instances >= max_instances
}

/// Prefer idle tuners first, then the lowest effective priority.
pub(super) fn choose_eviction_target(
    candidates: &[EvictionCandidate],
) -> Option<EvictionCandidate> {
    let mut best_idle: Option<EvictionCandidate> = None;
    let mut best_any: Option<EvictionCandidate> = None;

    for (key, priority, has_subscribers) in candidates.iter() {
        if !has_subscribers {
            if best_idle.as_ref().map_or(true, |(_, p, _)| priority < p) {
                best_idle = Some((key.clone(), *priority, *has_subscribers));
            }
        }
        if best_any.as_ref().map_or(true, |(_, p, _)| priority < p) {
            best_any = Some((key.clone(), *priority, *has_subscribers));
        }
    }

    best_idle.or(best_any)
}

pub(super) async fn driver_max_instances(
    database: &DatabaseHandle,
    tuner_path: &str,
) -> i32 {
    let db = database.lock().await;
    db.get_max_instances_for_path(tuner_path).unwrap_or(1)
}

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
            if tuner.is_running() {
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
        let mut wait_attempts = 0;
        while tuner.is_running() && wait_attempts < 50 {
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
    log_prefix: &str,
) {
    if tuner.subscriber_count() != 0 {
        return;
    }

    if !tuner.is_running() {
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

    if same_dll_switch || should_stop_reader_for_capacity(old_dll_running, old_dll_max) {
        info!(
            "[Session {}] {} stopping old reader for {:?} ({}/{})",
            session_id, log_prefix, tuner.key, old_dll_running, old_dll_max
        );
        stop_and_remove_tuner(tuner_pool, &tuner.key, tuner, false).await;
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
