//! Group-driver candidate ordering and selection helpers.
//!
//! The pure ordering/selection functions that used to live here (and their
//! unit tests) moved to `tuner::policy` as part of
//! docs/TUNER_PIPELINE_REDESIGN.md P0 — that module now owns the
//! `decide()` decision logic and needed these as building blocks. They are
//! re-exported below under their old names/visibility so
//! `select_group_driver_for_channel` (the async DB/pool orchestration that
//! stays here — it isn't pure) keeps compiling unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use log::debug;
use crate::tuner::channel_key::ChannelKeySpec;

use crate::server::listener::DatabaseHandle;
use crate::server::session_channel_candidates::collect_group_channel_candidates;
use crate::tuner::TunerPool;

pub(super) use crate::tuner::policy::{
    select_driver_with_capacity, select_running_driver, sort_candidate_drivers, DriverCandidate,
};

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
            if let Some(tuner) = tuner_pool.get(key).await {
                // Slot occupancy, not "TS flowing" — a reader still opening
                // the DLL already holds the slot (see
                // `session_capacity::count_running_instances_on_driver`).
                //
                // docs/TUNER_PIPELINE_REDESIGN.md P1b §4: this used to
                // exclude this session's own about-to-be-vacated tuner
                // (`old_tuner_will_free_slot`) from the count, as a stand-in
                // for capacity it hadn't actually reserved yet. That
                // exclusion is gone — actual capacity enforcement is now the
                // slot permit acquired in `server/session.rs`'s
                // `finish_set_channel_space_with_new_tuner` (with the old
                // tuner's own permit transferred there directly when
                // eligible), so this count is purely a same-channel /
                // least-loaded *selection* heuristic among group drivers,
                // not a capacity gate.
                if tuner.occupies_slot() {
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
            // A driver mid-startup on this exact channel is still the right
            // one to prefer — joining it avoids opening a second instance
            // for a channel that is already being tuned.
            if tuner.occupies_slot() {
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
