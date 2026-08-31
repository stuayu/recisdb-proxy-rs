//! Cached virtual-space and channel-list views for BNDP sessions.
//!
//! TVTest enumerates spaces/channels one entry at a time, so these helpers keep
//! the DB scan and grouping logic outside the main session loop and cache the
//! derived lists per tuner or group.

use crate::server::client_view::{self, ChannelEntry};
use crate::server::listener::DatabaseHandle;
use log::{debug, trace};
use std::collections::HashMap;

pub(super) type ChannelMapCache = HashMap<String, Vec<ChannelEntry>>;
pub(super) type SpaceListCache = HashMap<String, Vec<(u32, String, String)>>;

pub(super) fn clear_caches(
    channel_map_cache: &mut ChannelMapCache,
    space_list_cache: &mut SpaceListCache,
) {
    channel_map_cache.clear();
    space_list_cache.clear();
}

pub(super) fn current_or_default_tuner_path(
    current_tuner_path: &Option<String>,
    default_tuner: &Option<String>,
) -> String {
    current_tuner_path
        .as_ref()
        .or(default_tuner.as_ref())
        .cloned()
        .unwrap_or_default()
}

pub(super) async fn ensure_channel_map_with_region(
    database: &DatabaseHandle,
    session_id: u64,
    group_driver_paths: &[String],
    current_tuner_path: &str,
    channel_map_cache: &mut ChannelMapCache,
    region_name: &str,
) -> Vec<ChannelEntry> {
    if let Some(v) = channel_map_cache.get(region_name) {
        return v.clone();
    }

    let all = {
        let db = database.lock().await;
        match db.get_all_channels_with_drivers() {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    "[Session {}] ensure_channel_map_with_region: failed to get channels: {}",
                    session_id, e
                );
                Vec::new()
            }
        }
    };

    let map = if !group_driver_paths.is_empty() {
        client_view::build_channel_list(
            &all,
            |path| group_driver_paths.iter().any(|p| p == path),
            region_name,
        )
    } else {
        client_view::build_channel_list(&all, |path| path == current_tuner_path, region_name)
    };

    channel_map_cache.insert(region_name.to_string(), map.clone());
    map
}

pub(super) async fn ensure_space_list(
    database: &DatabaseHandle,
    session_id: u64,
    group_driver_paths: &[String],
    current_group_name: Option<&str>,
    current_tuner_path: &str,
    space_list_cache: &mut SpaceListCache,
) -> Vec<u32> {
    let (cache_key, is_group) = if !group_driver_paths.is_empty() {
        (
            format!("group_{}", current_group_name.unwrap_or("unknown")),
            true,
        )
    } else {
        if current_tuner_path.is_empty() {
            debug!(
                "[Session {}] ensure_space_list: tuner_path is empty",
                session_id
            );
            return Vec::new();
        }
        (current_tuner_path.to_string(), false)
    };

    if let Some(v) = space_list_cache.get(&cache_key) {
        trace!(
            "[Session {}] ensure_space_list: using cache for {} (spaces: {:?})",
            session_id,
            cache_key,
            v
        );
        return v.iter().map(|(actual_space, _, _)| *actual_space).collect();
    }

    let all = {
        let db = database.lock().await;
        match db.get_all_channels_with_drivers() {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    "[Session {}] ensure_space_list: failed to get channels: {}",
                    session_id, e
                );
                Vec::new()
            }
        }
    };

    let result = if is_group {
        client_view::build_space_list(&all, |path| group_driver_paths.iter().any(|p| p == path))
    } else {
        client_view::build_space_list(&all, |path| path == cache_key)
    };

    let list: Vec<(u32, String, String)> = result
        .spaces
        .into_iter()
        .map(|s| (s.actual_space, s.display_name, s.region_key))
        .collect();

    debug!(
        "[Session {}] ensure_space_list: final spaces for {}: {:?}",
        session_id, cache_key, list
    );
    space_list_cache.insert(cache_key, list.clone());
    list.iter()
        .map(|(actual_space, _, _)| *actual_space)
        .collect()
}

pub(super) fn map_space_idx_to_actual_with_region(
    group_driver_paths: &[String],
    current_group_name: Option<&str>,
    current_tuner_path: &str,
    space_list_cache: &SpaceListCache,
    space_idx: u32,
) -> Option<(u32, String)> {
    let list = get_space_list_with_names(
        group_driver_paths,
        current_group_name,
        current_tuner_path,
        space_list_cache,
    );
    list.get(space_idx as usize)
        .map(|(actual_space, _display_name, region_key)| (*actual_space, region_key.clone()))
}

pub(super) fn get_space_list_with_names(
    group_driver_paths: &[String],
    current_group_name: Option<&str>,
    current_tuner_path: &str,
    space_list_cache: &SpaceListCache,
) -> Vec<(u32, String, String)> {
    if !group_driver_paths.is_empty() {
        let cache_key = format!("group_{}", current_group_name.unwrap_or("unknown"));
        return space_list_cache
            .get(&cache_key)
            .cloned()
            .unwrap_or_default();
    }
    if current_tuner_path.is_empty() {
        return Vec::new();
    }
    space_list_cache
        .get(current_tuner_path)
        .cloned()
        .unwrap_or_default()
}
