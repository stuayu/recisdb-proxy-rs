//! Group-driver candidate ordering and selection helpers.
//!
//! Pure functions so the heuristics can be reviewed independently of the async
//! session/tuner-pool plumbing.

use std::collections::HashMap;
use std::sync::Arc;

use log::debug;
use crate::tuner::channel_key::ChannelKeySpec;

use crate::server::listener::DatabaseHandle;
use crate::server::session_channel_candidates::collect_group_channel_candidates;
use crate::tuner::{ChannelKey, TunerPool};

pub(super) type DriverCandidate = (String, u32, u32);

/// Sort candidates by rarity-aware load balancing.
pub(super) fn sort_candidate_drivers(
    candidate_drivers: &mut [DriverCandidate],
    exclusive_map: &HashMap<String, i64>,
    instances_map: &HashMap<String, i32>,
    score_map: &HashMap<String, f64>,
) {
    candidate_drivers.sort_by(|a, b| {
        let excl_a = exclusive_map.get(&a.0).copied().unwrap_or(0);
        let excl_b = exclusive_map.get(&b.0).copied().unwrap_or(0);
        excl_a
            .cmp(&excl_b)
            .then_with(|| {
                let load_a = instances_map.get(&a.0).copied().unwrap_or(0);
                let load_b = instances_map.get(&b.0).copied().unwrap_or(0);
                load_a.cmp(&load_b)
            })
            .then_with(|| {
                let score_a = score_map.get(&a.0).copied().unwrap_or(1.0);
                let score_b = score_map.get(&b.0).copied().unwrap_or(1.0);
                score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
            })
    });
}

/// Prefer a driver already running the requested physical channel.
pub(super) fn select_running_driver(
    candidate_drivers: &[DriverCandidate],
    running_channels: &[(String, ChannelKeySpec)],
) -> Option<DriverCandidate> {
    for (driver_path, driver_space, driver_bon_channel) in candidate_drivers.iter() {
        let wanted = ChannelKeySpec::SpaceChannel {
            space: *driver_space,
            channel: *driver_bon_channel,
        };
        if running_channels
            .iter()
            .any(|(path, key)| path == driver_path && *key == wanted)
        {
            return Some((driver_path.clone(), *driver_space, *driver_bon_channel));
        }
    }
    None
}

/// Otherwise choose the first driver with free capacity.
pub(super) fn select_driver_with_capacity(
    candidate_drivers: &[DriverCandidate],
    instances_map: &HashMap<String, i32>,
    max_instances_map: &HashMap<String, i32>,
) -> Option<DriverCandidate> {
    for (driver_path, driver_space, driver_bon_channel) in candidate_drivers.iter() {
        let driver_instances = instances_map.get(driver_path).copied().unwrap_or(0);
        let max_instances = max_instances_map.get(driver_path).copied().unwrap_or(1);
        if driver_instances < max_instances {
            return Some((driver_path.clone(), *driver_space, *driver_bon_channel));
        }
    }
    None
}


pub(super) struct GroupDriverSelection {
    pub selected_driver: DriverCandidate,
    pub nid_tsid_channel_keys: Vec<(String, ChannelKeySpec)>,
}

pub(super) async fn select_group_driver_for_channel(
    database: &DatabaseHandle,
    tuner_pool: &Arc<TunerPool>,
    session_id: u64,
    group_driver_paths: &[String],
    entry_nid: u16,
    entry_tsid: u16,
    old_tuner_key: Option<&ChannelKey>,
    old_tuner_will_free_slot: bool,
) -> Option<GroupDriverSelection> {
    debug!(
        "[Session {}] SetChannelSpace: In group mode, searching for NID=0x{:04X} TSID=0x{:04X}",
        session_id, entry_nid, entry_tsid
    );

    let mut candidate_drivers = collect_group_channel_candidates(
        database,
        session_id,
        group_driver_paths,
        entry_nid,
        entry_tsid,
    ).await;
    let mut max_instances_map: HashMap<String, i32> = HashMap::new();
    let mut score_map: HashMap<String, f64> = HashMap::new();
    let mut exclusive_map: HashMap<String, i64> = HashMap::new();

    {
        let db = database.lock().await;

        if !candidate_drivers.is_empty() {
            for (driver_path, _, _) in candidate_drivers.iter() {
                if score_map.contains_key(driver_path) {
                    continue;
                }
                let score = db.get_driver_quality_score_by_path(driver_path).unwrap_or(1.0);
                score_map.insert(driver_path.clone(), score);
            }
            exclusive_map = db
                .get_exclusive_channel_counts(group_driver_paths)
                .unwrap_or_default();
        }

        for (driver_path, _, _) in candidate_drivers.iter() {
            if max_instances_map.contains_key(driver_path) {
                continue;
            }
            let max_instances = db.get_max_instances_for_path(driver_path).unwrap_or(1);
            max_instances_map.insert(driver_path.clone(), max_instances);
        }
    }

    if candidate_drivers.is_empty() {
        return None;
    }

    let nid_tsid_channel_keys: Vec<(String, ChannelKeySpec)> = candidate_drivers
        .iter()
        .map(|(driver_path, space, channel)| {
            (
                driver_path.clone(),
                ChannelKeySpec::SpaceChannel {
                    space: *space,
                    channel: *channel,
                },
            )
        })
        .collect();

    let keys = tuner_pool.keys().await;
    let mut instances_map: HashMap<String, i32> = HashMap::new();
    for (driver_path, _, _) in candidate_drivers.iter() {
        if instances_map.contains_key(driver_path) {
            continue;
        }
        let mut running_instances = 0i32;
        for key in keys.iter() {
            if key.tuner_path != *driver_path {
                continue;
            }
            if old_tuner_will_free_slot && old_tuner_key == Some(key) {
                continue;
            }
            if let Some(tuner) = tuner_pool.get(key).await {
                if tuner.is_running() {
                    running_instances += 1;
                }
            }
        }
        instances_map.insert(driver_path.clone(), running_instances);
    }

    sort_candidate_drivers(
        &mut candidate_drivers,
        &exclusive_map,
        &instances_map,
        &score_map,
    );

    let mut running_channels: Vec<(String, ChannelKeySpec)> = Vec::new();
    for key in keys.iter() {
        if let Some(tuner) = tuner_pool.get(key).await {
            if tuner.is_running() {
                running_channels.push((key.tuner_path.clone(), key.channel.clone()));
            }
        }
    }

    let mut selected_driver = select_running_driver(&candidate_drivers, &running_channels);
    if let Some((ref driver_path, driver_space, driver_bon_channel)) = selected_driver {
        debug!(
            "[Session {}] Selected driver (already streaming this channel): {} (space {}, ch {})",
            session_id, driver_path, driver_space, driver_bon_channel
        );
    }

    if selected_driver.is_none() {
        selected_driver = select_driver_with_capacity(
            &candidate_drivers,
            &instances_map,
            &max_instances_map,
        );
        if let Some((ref driver_path, driver_space, driver_bon_channel)) = selected_driver {
            let driver_instances = instances_map.get(driver_path).copied().unwrap_or(0);
            let max_instances = max_instances_map.get(driver_path).copied().unwrap_or(1);
            debug!(
                "[Session {}] Driver {} has {}/{} instances",
                session_id, driver_path, driver_instances, max_instances
            );
            debug!(
                "[Session {}] Selected driver (with capacity): {} (space {}, ch {})",
                session_id, driver_path, driver_space, driver_bon_channel
            );
        }
    }

    if selected_driver.is_none() && !candidate_drivers.is_empty() {
        selected_driver = Some(candidate_drivers[0].clone());
        if let Some((ref driver_path, driver_space, driver_bon_channel)) = selected_driver {
            debug!(
                "[Session {}] Selected driver (all full, will check priority): {} (space {}, ch {})",
                session_id, driver_path, driver_space, driver_bon_channel
            );
        }
    }

    selected_driver.map(|selected_driver| GroupDriverSelection {
        selected_driver,
        nid_tsid_channel_keys,
    })
}
