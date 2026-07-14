//! Shared DB-backed channel-candidate collection for BNDP sessions.
//!
//! Keeps the NID+TSID → driver candidate lookup out of `session.rs` so
//! group-mode selection and fallback routing can reuse the same query path.

use log::{debug, error};

use crate::server::listener::DatabaseHandle;
use crate::server::session_driver_selection::DriverCandidate;

pub(super) async fn collect_group_channel_candidates(
    database: &DatabaseHandle,
    session_id: u64,
    group_driver_paths: &[String],
    entry_nid: u16,
    entry_tsid: u16,
) -> Vec<DriverCandidate> {
    let db = database.lock().await;
    match db.get_channels_by_nid_tsid(entry_nid, entry_tsid) {
        Ok(matched_channels) => matched_channels
            .into_iter()
            .filter_map(|(ch, bd_opt)| {
                let bd = bd_opt?;
                if !group_driver_paths.contains(&bd.dll_path) {
                    return None;
                }
                if ch.nid as u16 != entry_nid || ch.tsid as u16 != entry_tsid || !ch.is_enabled {
                    return None;
                }
                debug!(
                    "[Session {}] Found NID+TSID match in driver {} (space {}, ch {})",
                    session_id, bd.dll_path, ch.space, ch.channel
                );
                Some((bd.dll_path, ch.space, ch.channel))
            })
            .collect(),
        Err(e) => {
            error!("[Session {}] Failed to query channels: {}", session_id, e);
            Vec::new()
        }
    }
}
