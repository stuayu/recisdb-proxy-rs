//! Client-facing tuning-space / channel enumeration.
//!
//! A BonDriver client (TVTest + BonDriver_NetworkProxy 等) sees *virtual*
//! tuning spaces and channel indices enumerated by the session
//! (`EnumTuningSpace` / `EnumChannelName`), not the physical
//! `bon_space`/`bon_channel` values stored in the `channels` table. This
//! module holds that enumeration as pure functions over database rows so
//! that both the session (server/session.rs) and the web dashboard's
//! 「クライアント設定ガイド」 (web/api.rs) produce exactly the same view —
//! the whole point of the guide is that what the GUI shows equals what the
//! client must specify.
//!
//! Semantics (must stay in sync with SetChannelSpace handling in
//! session.rs):
//! - A *space* is a broadcast region: one per terrestrial 広域圏 (関東,
//!   東北, ...), plus "BS" and "CS". Terrestrial spaces are sorted by
//!   region-key string, then BS, then CS. The space *index* the client
//!   passes to `SetChannel(space, channel)` is the position in that list.
//! - A *channel* within a space is a transport stream, deduplicated by
//!   (NID, TSID) and sorted by that key; the channel *index* is the
//!   position in that list. The display name is the first enabled
//!   service's name.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use recisdb_protocol::broadcast_region::{classify_nid, generate_space_name, TerrestrialRegion};
use recisdb_protocol::types::BroadcastType;

use crate::database::{BonDriverRecord, ClientChannelRecord};

/// One row of `Database::get_all_channels_with_drivers()`.
pub type ChannelRow = (ClientChannelRecord, Option<BonDriverRecord>);

/// A channel as enumerated to the client within one tuning space.
/// The client-facing channel index is the position in the returned Vec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelEntry {
    /// Physical space of the representative driver (same row that
    /// `bon_channel` came from — see [`build_channels_by_region`]). Not
    /// meaningful to end users; used internally so single-tuner mode can
    /// pair space/channel from the same physical row instead of falling
    /// back to the region's representative space, which may belong to a
    /// different driver/row when NID differs across drivers in a group.
    pub bon_space: u32,
    /// Physical channel number on the representative driver.
    pub bon_channel: u32,
    /// Display name returned by EnumChannelName.
    pub name: String,
    pub nid: u16,
    pub tsid: u16,
}

/// A physical (driver, space, channel) tuning target for one logical
/// (NID, TSID) channel. In group mode one logical channel may be reachable
/// through several drivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalMapping {
    pub driver_path: String,
    pub actual_space: u32,
    pub actual_channel: u32,
}

/// A tuning space as enumerated to the client.
/// The client-facing space index is the position in the returned Vec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceEntry {
    /// Physical space of the first channel seen in this region (kept for
    /// session bookkeeping; not meaningful to end users).
    pub actual_space: u32,
    /// Display name returned by EnumTuningSpace (e.g. "地デジ (関東)").
    pub display_name: String,
    /// Region key used to match channels into this space
    /// (e.g. "関東", "BS", "CS", "Unknown").
    pub region_key: String,
}

/// Result of [`build_space_list`]: the ordered space list plus, as a
/// byproduct of the same row scan, every physical mapping per (NID, TSID).
#[derive(Debug, Default)]
pub struct SpaceListResult {
    pub spaces: Vec<SpaceEntry>,
    pub nid_tsid_mappings: HashMap<(u16, u16), Vec<PhysicalMapping>>,
}

/// Region key for a NID, exactly as the session classifies channels into
/// spaces. Static so per-row filtering allocates nothing.
pub fn region_key_for_nid(nid: u16) -> &'static str {
    let (btype, terrestrial_region) = classify_nid(nid);
    match btype {
        BroadcastType::BS => "BS",
        BroadcastType::CS => "CS",
        BroadcastType::FourK => "BS4K",
        BroadcastType::Terrestrial => match terrestrial_region {
            Some(TerrestrialRegion::Unknown(_)) | None => "Unknown",
            Some(r) => r.display_name(),
        },
    }
}

/// Display name for the space containing `nid`, matching the historical
/// session behavior: `generate_space_name` for classified channels
/// ("地デジ (関東)" / "BS" / "CS"), bare "Unknown" for unclassifiable
/// terrestrial NIDs (generate_space_name would say "地デジ (その他)", but
/// clients have always been shown "Unknown" for these).
fn display_name_for_nid(nid: u16) -> String {
    match classify_nid(nid) {
        (BroadcastType::Terrestrial, Some(TerrestrialRegion::Unknown(_)))
        | (BroadcastType::Terrestrial, None) => "Unknown".to_string(),
        (btype, region) => generate_space_name(btype, region),
    }
}

/// Build the client-visible tuning-space list from channel rows.
///
/// `driver_matches` selects which drivers are in scope: a single DLL path
/// in single-tuner mode, or membership in `group_driver_paths` in group
/// mode — the only difference between session.rs's two former copies of
/// this logic.
pub fn build_space_list<F: Fn(&str) -> bool>(
    rows: &[ChannelRow],
    driver_matches: F,
) -> SpaceListResult {
    let mut nid_tsid_seen: BTreeSet<(u16, u16)> = BTreeSet::new();
    let mut region_seen: BTreeSet<String> = BTreeSet::new();
    // region_key -> (actual_space, display_name)
    let mut space_region_names: HashMap<String, (u32, String)> = HashMap::new();
    let mut nid_tsid_mappings: HashMap<(u16, u16), Vec<PhysicalMapping>> = HashMap::new();

    for (ch, bd_opt) in rows {
        let Some(bd) = bd_opt else { continue };
        if !driver_matches(&bd.dll_path) {
            continue;
        }
        if !ch.is_enabled {
            continue;
        }

        let nid_tsid = (ch.nid as u16, ch.tsid as u16);

        // Record every physical mapping (multiple drivers may carry the
        // same logical channel).
        nid_tsid_mappings
            .entry(nid_tsid)
            .or_default()
            .push(PhysicalMapping {
                driver_path: bd.dll_path.clone(),
                actual_space: ch.space,
                actual_channel: ch.channel,
            });

        // For the display list, only the first row per (NID, TSID) counts.
        if !nid_tsid_seen.insert(nid_tsid) {
            continue;
        }

        let region_key = region_key_for_nid(nid_tsid.0);
        if !region_seen.insert(region_key.to_string()) {
            continue;
        }
        space_region_names.insert(
            region_key.to_string(),
            (ch.space, display_name_for_nid(nid_tsid.0)),
        );
    }

    // Order: terrestrial regions (sorted by region key), then BS, then CS,
    // then BS4K.
    //
    // BS4K goes last on purpose. The space index is the client's addressing
    // scheme (.ch2 / ChSet5), so inserting a space anywhere but the end
    // renumbers everything after it and silently repoints existing presets.
    // Appended last, a setup with no 4K channels keeps every index it had.
    let mut terrestrial: Vec<SpaceEntry> = Vec::new();
    let mut bs: Option<SpaceEntry> = None;
    let mut cs: Option<SpaceEntry> = None;
    let mut four_k: Option<SpaceEntry> = None;

    for (region_key, (actual_space, display_name)) in space_region_names {
        let entry = SpaceEntry {
            actual_space,
            display_name,
            region_key: region_key.clone(),
        };
        match region_key.as_str() {
            "BS" => bs = Some(entry),
            "CS" => cs = Some(entry),
            "BS4K" => four_k = Some(entry),
            _ => terrestrial.push(entry),
        }
    }
    terrestrial.sort_by(|a, b| a.region_key.cmp(&b.region_key));

    let mut spaces = terrestrial;
    spaces.extend(bs);
    spaces.extend(cs);
    spaces.extend(four_k);

    SpaceListResult {
        spaces,
        nid_tsid_mappings,
    }
}

/// Build the client-visible channel lists for every tuning space in one
/// pass, keyed by region key. The index into each Vec is the channel number
/// the client passes to `SetChannel(space, channel)`.
pub fn build_channels_by_region<F: Fn(&str) -> bool>(
    rows: &[ChannelRow],
    driver_matches: F,
) -> BTreeMap<&'static str, Vec<ChannelEntry>> {
    // Per region, dedupe by (NID, TSID): different drivers may use
    // different bon_space/bon_channel values for the same logical channel.
    // bon_space and bon_channel are always taken from the SAME row (the
    // first one seen), so they stay a valid physical pair even when driver
    // NID ranges/space numbering differ across a group.
    let mut uniq: BTreeMap<&'static str, BTreeMap<(u16, u16), (u32, u32, String)>> =
        BTreeMap::new();

    for (ch, bd_opt) in rows {
        let Some(bd) = bd_opt else { continue };
        if !driver_matches(&bd.dll_path) {
            continue;
        }
        if !ch.is_enabled {
            continue;
        }

        let bspace = ch.space;
        let bch = ch.channel;
        let name = ch
            .service_name
            .clone()
            .or_else(|| ch.ts_name.clone())
            .unwrap_or_else(|| format!("CH{}", bch));

        uniq.entry(region_key_for_nid(ch.nid as u16))
            .or_default()
            .entry((ch.nid as u16, ch.tsid as u16))
            .or_insert((bspace, bch, name));
    }

    uniq.into_iter()
        .map(|(region, channels)| {
            let list = channels
                .into_iter()
                .map(
                    |((nid, tsid), (bon_space, bon_channel, name))| ChannelEntry {
                        bon_space,
                        bon_channel,
                        name,
                        nid,
                        tsid,
                    },
                )
                .collect();
            (region, list)
        })
        .collect()
}

/// Build the client-visible channel list for one tuning space
/// (identified by its region key). See [`build_channels_by_region`].
pub fn build_channel_list<F: Fn(&str) -> bool>(
    rows: &[ChannelRow],
    driver_matches: F,
    region_key: &str,
) -> Vec<ChannelEntry> {
    build_channels_by_region(rows, driver_matches)
        .remove(region_key)
        .unwrap_or_default()
}

/// One broadcast service (SID) within a transport stream, for channel-file
/// generation (TVTest .ch2 / EDCB ChSet4/ChSet5): those files list one row
/// per *service*, whereas the client enumeration above is one row per TS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceEntry {
    pub nid: u16,
    pub tsid: u16,
    pub sid: u16,
    pub name: String,
    pub service_type: Option<i32>,
    pub remote_control_key: Option<i32>,
    /// TS name (network_name column) if recorded during scan.
    pub ts_name: Option<String>,
}

/// Enumerate every enabled service per (NID, TSID) in scope, deduplicated
/// by (NID, TSID, SID) (first row wins, same rule as the channel dedup)
/// and sorted by SID. Keyed by the same (NID, TSID) identity as
/// [`ChannelEntry`], so callers can join services onto the enumerated
/// channel/space indices.
pub fn build_services_by_ts<F: Fn(&str) -> bool>(
    rows: &[ChannelRow],
    driver_matches: F,
) -> HashMap<(u16, u16), Vec<ServiceEntry>> {
    let mut uniq: BTreeMap<(u16, u16, u16), ServiceEntry> = BTreeMap::new();

    for (ch, bd_opt) in rows {
        let Some(bd) = bd_opt else { continue };
        if !driver_matches(&bd.dll_path) {
            continue;
        }
        if !ch.is_enabled {
            continue;
        }

        let key = (ch.nid as u16, ch.tsid as u16, ch.sid as u16);
        uniq.entry(key).or_insert_with(|| ServiceEntry {
            nid: key.0,
            tsid: key.1,
            sid: key.2,
            name: ch
                .service_name
                .clone()
                .or_else(|| ch.ts_name.clone())
                .unwrap_or_else(|| format!("SID{}", key.2)),
            service_type: ch.service_type,
            remote_control_key: ch.remote_control_key,
            ts_name: ch.ts_name.clone(),
        });
    }

    let mut by_ts: HashMap<(u16, u16), Vec<ServiceEntry>> = HashMap::new();
    for ((nid, tsid, _sid), entry) in uniq {
        by_ts.entry((nid, tsid)).or_default().push(entry);
    }
    by_ts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(id: i64, path: &str, group: Option<&str>) -> BonDriverRecord {
        BonDriverRecord {
            id,
            dll_path: path.to_string(),
            driver_name: None,
            version: None,
            group_name: group.map(str::to_string),
            auto_scan_enabled: true,
            scan_interval_hours: 24,
            scan_priority: 0,
            last_scan: None,
            next_scan_at: None,
            passive_scan_enabled: true,
            max_instances: 1,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn row(
        driver_rec: &BonDriverRecord,
        nid: i32,
        tsid: i32,
        sid: i32,
        name: Option<&str>,
        space: u32,
        channel: u32,
        enabled: bool,
    ) -> ChannelRow {
        (
            ClientChannelRecord {
                id: 0,
                bon_driver_id: driver_rec.id,
                nid,
                sid,
                tsid,
                service_name: name.map(str::to_string),
                ts_name: None,
                service_type: None,
                remote_control_key: None,
                space,
                channel,
                is_enabled: enabled,
                priority: 0,
            },
            Some(driver_rec.clone()),
        )
    }

    #[test]
    fn spaces_are_ordered_terrestrial_then_bs_then_cs() {
        let d = driver(1, "BonDriver_A.dll", None);
        let rows = vec![
            row(&d, 0x0004, 0x4010, 101, Some("BS朝日"), 1, 0, true), // BS
            row(&d, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 27, true), // 関東
            row(&d, 0x0006, 0x6020, 100, Some("CS100"), 2, 0, true),  // CS
        ];
        let result = build_space_list(&rows, |p| p == "BonDriver_A.dll");
        let names: Vec<_> = result
            .spaces
            .iter()
            .map(|s| s.region_key.as_str())
            .collect();
        assert_eq!(names, vec!["関東", "BS", "CS"]);
        assert_eq!(result.spaces[0].display_name, "地デジ (関東)");
    }

    /// 4K (advanced BS, NID 0x000B) must get its own space instead of being
    /// swept into a bogus terrestrial one. It goes last so that a setup
    /// without 4K keeps the space indices its .ch2 / ChSet5 files were built
    /// against.
    #[test]
    fn four_k_gets_its_own_space_and_is_appended_last() {
        let d = driver(1, "BonDriver_A.dll", None);
        let without_4k = vec![
            row(&d, 0x0004, 0x4010, 101, Some("BS朝日"), 1, 0, true),
            row(&d, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 27, true),
            row(&d, 0x0006, 0x6020, 100, Some("CS100"), 2, 0, true),
        ];
        let baseline: Vec<_> = build_space_list(&without_4k, |p| p == "BonDriver_A.dll")
            .spaces
            .iter()
            .map(|s| s.region_key.clone())
            .collect();

        let mut with_4k = without_4k.clone();
        // Real capture: BS朝日4K is NID 0x000B / TSID 0xB070 / SID 0x97.
        with_4k.push(row(&d, 0x000B, 0xB070, 0x97, Some("BS朝日 4K"), 3, 0, true));

        let result = build_space_list(&with_4k, |p| p == "BonDriver_A.dll");
        let keys: Vec<_> = result.spaces.iter().map(|s| s.region_key.clone()).collect();

        assert_eq!(keys, vec!["関東", "BS", "CS", "BS4K"]);
        assert_eq!(
            keys[..baseline.len()],
            baseline[..],
            "existing spaces must keep their indices when 4K appears"
        );
        assert_eq!(result.spaces[3].display_name, "BS4K");
    }

    /// Before 4K was classified, NID 0x000B fell through to the terrestrial
    /// branch and produced an "Unknown" space — which also sorted *among* the
    /// terrestrial regions and so shifted BS/CS.
    #[test]
    fn four_k_is_not_classified_as_unknown_terrestrial() {
        assert_eq!(region_key_for_nid(0x000B), "BS4K");
        assert_eq!(region_key_for_nid(0x000C), "BS4K");
        assert_ne!(region_key_for_nid(0x000B), "Unknown");
    }

    #[test]
    fn channels_in_space_are_deduped_by_nid_tsid_and_sorted() {
        let d1 = driver(1, "BonDriver_A.dll", Some("G"));
        let d2 = driver(2, "BonDriver_B.dll", Some("G"));
        let rows = vec![
            // Same logical channel on two drivers with different bon_channel.
            row(&d1, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 27, true),
            row(&d2, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 5, true),
            row(&d1, 0x7FE9, 0x7FE9, 2056, Some("日テレ"), 0, 25, true),
            // Disabled channels are excluded.
            row(&d1, 0x7FEA, 0x7FEA, 3000, Some("無効"), 0, 20, false),
        ];
        let in_group = |p: &str| p == "BonDriver_A.dll" || p == "BonDriver_B.dll";

        let list = build_channel_list(&rows, in_group, "関東");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "NHK総合");
        assert_eq!(list[0].bon_channel, 27); // first row wins
        assert_eq!(list[0].bon_space, 0); // same row as bon_channel above
        assert_eq!(list[1].name, "日テレ");

        // Mappings record both drivers for the shared channel.
        let result = build_space_list(&rows, in_group);
        let mappings = &result.nid_tsid_mappings[&(0x7FE8, 0x7FE8)];
        assert_eq!(mappings.len(), 2);
    }

    /// Regression test for the space/channel mispairing bug: a ChannelEntry
    /// must always report `bon_space` and `bon_channel` from the SAME
    /// physical row, even when the group's drivers disagree on which
    /// space/channel numbers carry the logical (NID, TSID) channel.
    #[test]
    fn channel_entry_pairs_bon_space_with_bon_channel_from_same_row() {
        let d1 = driver(1, "BonDriver_A.dll", Some("G"));
        let d2 = driver(2, "BonDriver_B.dll", Some("G"));
        // Driver A carries this logical channel at (space=1, ch=27);
        // driver B carries the SAME logical channel at (space=0, ch=5).
        // A naive implementation that takes bon_channel from the first row
        // but bon_space from some other "representative" source (e.g. the
        // region's first-seen space) could produce the invalid pair
        // (space=0, ch=27), which doesn't exist on either driver.
        let rows_a_first = vec![
            row(&d1, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 1, 27, true),
            row(&d2, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 5, true),
        ];
        let in_group = |p: &str| p == "BonDriver_A.dll" || p == "BonDriver_B.dll";

        let list = build_channel_list(&rows_a_first, in_group, "関東");
        assert_eq!(list.len(), 1);
        // First row wins (driver A): the pair must be (1, 27), not a mix
        // like (0, 27) or (1, 5).
        assert_eq!((list[0].bon_space, list[0].bon_channel), (1, 27));

        // Swapping row order: driver B's row now comes first, so the
        // representative pair must consistently become (0, 5).
        let rows_b_first = vec![
            row(&d2, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 5, true),
            row(&d1, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 1, 27, true),
        ];
        let list2 = build_channel_list(&rows_b_first, in_group, "関東");
        assert_eq!(list2.len(), 1);
        assert_eq!((list2[0].bon_space, list2[0].bon_channel), (0, 5));
    }

    #[test]
    fn driver_filter_limits_scope() {
        let d1 = driver(1, "BonDriver_A.dll", None);
        let d2 = driver(2, "BonDriver_B.dll", None);
        let rows = vec![
            row(&d1, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 27, true),
            row(&d2, 0x0004, 0x4010, 101, Some("BS朝日"), 1, 0, true),
        ];
        let result = build_space_list(&rows, |p| p == "BonDriver_A.dll");
        assert_eq!(result.spaces.len(), 1);
        assert_eq!(result.spaces[0].region_key, "関東");

        assert!(build_channel_list(&rows, |p| p == "BonDriver_A.dll", "BS").is_empty());
    }

    #[test]
    fn services_are_grouped_per_ts_and_sorted_by_sid() {
        let d1 = driver(1, "BonDriver_A.dll", Some("G"));
        let d2 = driver(2, "BonDriver_B.dll", Some("G"));
        let rows = vec![
            // Two services on the same TS (out of SID order), one duplicated
            // on a second driver, plus a disabled service.
            row(
                &d1,
                0x7FE8,
                0x7FE8,
                1025,
                Some("NHK総合・サブ"),
                0,
                27,
                true,
            ),
            row(&d1, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 27, true),
            row(&d2, 0x7FE8, 0x7FE8, 1024, Some("NHK総合"), 0, 5, true),
            row(
                &d1,
                0x7FE8,
                0x7FE8,
                1026,
                Some("無効サービス"),
                0,
                27,
                false,
            ),
            row(&d1, 0x0004, 0x4010, 101, Some("BS朝日"), 1, 0, true),
        ];
        let by_ts = build_services_by_ts(&rows, |_| true);
        let terr = &by_ts[&(0x7FE8, 0x7FE8)];
        assert_eq!(
            terr.iter()
                .map(|s| (s.sid, s.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(1024, "NHK総合"), (1025, "NHK総合・サブ")]
        );
        assert_eq!(by_ts[&(0x0004, 0x4010)].len(), 1);
    }

    #[test]
    fn name_falls_back_to_ch_number() {
        let d = driver(1, "BonDriver_A.dll", None);
        let rows = vec![row(&d, 0x7FE8, 0x7FE8, 1024, None, 0, 27, true)];
        let list = build_channel_list(&rows, |_| true, "関東");
        assert_eq!(list[0].name, "CH27");
    }
}
