//! Shared DB-backed channel-candidate collection for BNDP sessions.
//!
//! Keeps the NID+TSID → driver candidate lookup out of `session.rs` so
//! group-mode selection and fallback routing can reuse the same query path.

use log::{debug, error};

use crate::server::listener::DatabaseHandle;
use crate::tuner::policy::DriverCandidate;

pub(super) async fn collect_group_channel_candidates(
    database: &DatabaseHandle,
    session_id: u64,
    group_driver_paths: &[String],
    entry_nid: u16,
    entry_tsid: u16,
) -> Vec<DriverCandidate> {
    let db = database.lock().await;
    match db.get_channels_by_nid_tsid(entry_nid, entry_tsid) {
        Ok(matched_channels) => {
            // `get_channels_by_nid_tsid` returns one row per SID on the
            // matched transport stream, so the same (path, space, channel)
            // physical target can appear more than once here (e.g. a TS
            // carrying several services). Dedup while keeping first-seen
            // order so candidate ordering downstream (sort_candidate_drivers
            // etc.) stays stable.
            let mut seen: std::collections::HashSet<(String, u32, u32)> =
                std::collections::HashSet::new();
            matched_channels
                .into_iter()
                .filter_map(|(ch, bd_opt)| {
                    let bd = bd_opt?;
                    if !group_driver_paths.contains(&bd.dll_path) {
                        return None;
                    }
                    if ch.nid as u16 != entry_nid || ch.tsid as u16 != entry_tsid || !ch.is_enabled {
                        return None;
                    }
                    let candidate = (bd.dll_path, ch.space, ch.channel);
                    if !seen.insert(candidate.clone()) {
                        return None;
                    }
                    debug!(
                        "[Session {}] Found NID+TSID match in driver {} (space {}, ch {})",
                        session_id, candidate.0, candidate.1, candidate.2
                    );
                    Some(candidate)
                })
                .collect()
        }
        Err(e) => {
            error!("[Session {}] Failed to query channels: {}", session_id, e);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use recisdb_protocol::ChannelInfo;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn test_channel(nid: u16, sid: u16, tsid: u16, space: u32, channel: u32) -> ChannelInfo {
        let mut info = ChannelInfo::new(nid, sid, tsid);
        info.channel_name = Some(format!("Test {}", sid));
        info.bon_space = Some(space);
        info.bon_channel = Some(channel);
        info
    }

    /// Regression test: two group drivers carry the same logical (NID,
    /// TSID) channel at DIFFERENT physical (space, channel) numbers (e.g.
    /// driver A at space0/ch27, driver B at space0/ch5). Each returned
    /// candidate must pair a driver with ITS OWN physical numbers — never
    /// driver A's path with driver B's channel or vice versa.
    #[tokio::test]
    async fn candidates_pair_each_driver_with_its_own_physical_numbers() {
        let db = Database::open_in_memory().unwrap();
        let driver_a = db.get_or_create_bon_driver("A.dll").unwrap();
        let driver_b = db.get_or_create_bon_driver("B.dll").unwrap();
        db.set_group_name(driver_a, Some("G")).unwrap();
        db.set_group_name(driver_b, Some("G")).unwrap();

        // Same logical channel (NID=0x7FE8, TSID=0x7FE8, SID=1024), but a
        // different physical channel number per driver.
        db.insert_channel(driver_a, &test_channel(0x7FE8, 1024, 0x7FE8, 0, 27))
            .unwrap();
        db.insert_channel(driver_b, &test_channel(0x7FE8, 1024, 0x7FE8, 0, 5))
            .unwrap();

        let database: DatabaseHandle = Arc::new(Mutex::new(db));
        let group_paths = vec!["A.dll".to_string(), "B.dll".to_string()];
        let candidates =
            collect_group_channel_candidates(&database, 1, &group_paths, 0x7FE8, 0x7FE8).await;

        assert_eq!(candidates.len(), 2);
        let a = candidates.iter().find(|c| c.0 == "A.dll").expect("A.dll candidate");
        assert_eq!((a.1, a.2), (0, 27), "A.dll must keep its own channel 27, not B.dll's 5");
        let b = candidates.iter().find(|c| c.0 == "B.dll").expect("B.dll candidate");
        assert_eq!((b.1, b.2), (0, 5), "B.dll must keep its own channel 5, not A.dll's 27");
    }

    /// A TS carrying multiple services produces one DB row per SID, but the
    /// resulting (path, space, channel) physical target is identical across
    /// those rows. Candidates must be deduplicated to one entry per driver.
    #[tokio::test]
    async fn candidates_are_deduplicated_by_physical_target() {
        let db = Database::open_in_memory().unwrap();
        let driver_a = db.get_or_create_bon_driver("A.dll").unwrap();
        db.set_group_name(driver_a, Some("G")).unwrap();

        // Two services (different SID) on the same TS/driver/physical channel.
        db.insert_channel(driver_a, &test_channel(0x7FE8, 1024, 0x7FE8, 0, 27))
            .unwrap();
        db.insert_channel(driver_a, &test_channel(0x7FE8, 1025, 0x7FE8, 0, 27))
            .unwrap();

        let database: DatabaseHandle = Arc::new(Mutex::new(db));
        let group_paths = vec!["A.dll".to_string()];
        let candidates =
            collect_group_channel_candidates(&database, 1, &group_paths, 0x7FE8, 0x7FE8).await;

        assert_eq!(candidates, vec![("A.dll".to_string(), 0, 27)]);
    }
}
