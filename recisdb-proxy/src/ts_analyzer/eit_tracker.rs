//! EIT section-level reception tracking.
//!
//! The tracker records sections only; event contents are deliberately not
//! inspected.  This makes empty schedule sections count as received too.

use std::collections::BTreeMap;

use super::{pid, table_id, PsiSection};

/// Maximum number of services retained by a tracker by default.
pub const DEFAULT_MAX_SERVICES: usize = 1024;

/// Identity of a service being tracked. PID is included because H-EIT,
/// M-EIT, and L-EIT can carry table 0x4E on different PIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EitServiceKey {
    pub pid: u16,
    pub original_network_id: u16,
    pub transport_stream_id: u16,
    pub service_id: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EitKind {
    PresentFollowing,
    ScheduleBasic,
    ScheduleExtended,
}

/// Result of observing one CRC-validated EIT section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveOutcome {
    Ignored,
    Duplicate,
    Recorded,
    VersionChanged,
    Dropped,
}

/// Completion state for one tracked service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EitServiceCompletion {
    pub key: EitServiceKey,
    pub pf_complete: bool,
    pub schedule_basic_complete: bool,
    pub schedule_extended_complete: bool,
    pub schedule_coverage_until: Option<i64>,
    pub sections_seen: u32,
    pub last_section_at: i64,
}

struct SubTableState {
    version: u8,
    last_section_number: u8,
    last_table_id: u8,
    received: [u64; 4],
    segment_last: [Option<u8>; 32],
    updated_at: i64,
}

struct ServiceState {
    sub_tables: BTreeMap<u8, SubTableState>,
    sections_seen: u32,
    last_section_at: i64,
}

/// Pure, bounded state for EIT section reception.
pub struct EpgSectionTracker {
    services: BTreeMap<EitServiceKey, ServiceState>,
    max_services: usize,
    total_sections_seen: u64,
    dropped_sections: u64,
    last_now: Option<i64>,
}

impl EpgSectionTracker {
    pub fn new() -> Self {
        Self::with_service_limit(DEFAULT_MAX_SERVICES)
    }

    pub fn with_service_limit(max_services: usize) -> Self {
        Self {
            services: BTreeMap::new(),
            max_services,
            total_sections_seen: 0,
            dropped_sections: 0,
            last_now: None,
        }
    }

    pub fn observe(
        &mut self,
        pid_value: u16,
        section: &PsiSection<'_>,
        now: i64,
    ) -> ObserveOutcome {
        let id = section.header.table_id;
        if !table_id::is_eit_table_id(id)
            || ((pid_value == pid::EIT_MOBILE || pid_value == pid::EIT_PARTIAL_RECEPTION)
                && id != table_id::EIT_PF_ACTUAL)
            || !section.header.current_next_indicator
            || section.data.len() < 6
            || section.header.section_number > section.header.last_section_number
        {
            return ObserveOutcome::Ignored;
        }

        let transport_stream_id = u16::from_be_bytes([section.data[0], section.data[1]]);
        let original_network_id = u16::from_be_bytes([section.data[2], section.data[3]]);
        let key = EitServiceKey {
            pid: pid_value,
            original_network_id,
            transport_stream_id,
            service_id: section.header.table_id_extension,
        };

        if !self.services.contains_key(&key) && self.services.len() >= self.max_services {
            self.dropped_sections = self.dropped_sections.saturating_add(1);
            return ObserveOutcome::Dropped;
        }

        self.last_now = Some(now);
        let segment_last = section.data[4];
        let last_table_id = section.data[5];
        let section_number = section.header.section_number;
        let bit_index = section_number as usize;
        let word = bit_index / 64;
        let bit = 1u64 << (bit_index % 64);
        let service = self.services.entry(key).or_insert_with(|| ServiceState {
            sub_tables: BTreeMap::new(),
            sections_seen: 0,
            last_section_at: now,
        });

        let sub = service
            .sub_tables
            .entry(id)
            .or_insert_with(|| SubTableState {
                version: section.header.version_number,
                last_section_number: section.header.last_section_number,
                last_table_id,
                received: [0; 4],
                segment_last: [None; 32],
                updated_at: now,
            });

        let mut outcome = ObserveOutcome::Recorded;
        if sub.version != section.header.version_number {
            let delta = section.header.version_number.wrapping_sub(sub.version) & 0x1F;
            if delta == 0 || delta >= 16 {
                return ObserveOutcome::Ignored;
            }
            sub.version = section.header.version_number;
            sub.last_section_number = section.header.last_section_number;
            sub.received = [0; 4];
            sub.segment_last = [None; 32];
            outcome = ObserveOutcome::VersionChanged;
        } else if sub.received[word] & bit != 0 {
            return ObserveOutcome::Duplicate;
        }

        sub.received[word] |= bit;
        sub.last_section_number = section.header.last_section_number;
        sub.last_table_id = last_table_id;
        sub.segment_last[(section_number / 8) as usize] = Some(segment_last);
        sub.updated_at = now;
        service.sections_seen = service.sections_seen.saturating_add(1);
        service.last_section_at = now;
        self.total_sections_seen = self.total_sections_seen.saturating_add(1);
        outcome
    }

    pub fn completion(&self, key: &EitServiceKey) -> Option<EitServiceCompletion> {
        let service = self.services.get(key)?;
        let (basic, extended, coverage) =
            if let Some(basic_base) = schedule_base(&service.sub_tables) {
                let basic_last = schedule_last(&service.sub_tables, basic_base, basic_base + 7);
                let ext_base = basic_base + 8;
                let ext_cap = basic_base + 15;
                let ext_last = schedule_last(&service.sub_tables, ext_base, ext_cap);
                let extended = service
                    .sub_tables
                    .keys()
                    .any(|&id| id >= ext_base && id <= ext_cap)
                    && ext_last >= ext_base
                    && range_complete(&service.sub_tables, ext_base, ext_last);
                (
                    range_complete(&service.sub_tables, basic_base, basic_last),
                    extended,
                    coverage_until(service, basic_base, basic_last, self.last_now),
                )
            } else {
                (false, false, None)
            };
        Some(EitServiceCompletion {
            key: *key,
            pf_complete: [table_id::EIT_PF_ACTUAL, table_id::EIT_PF_OTHER]
                .iter()
                .any(|id| service.sub_tables.get(id).is_some_and(subtable_complete)),
            schedule_basic_complete: basic,
            schedule_extended_complete: extended,
            schedule_coverage_until: coverage,
            sections_seen: service.sections_seen,
            last_section_at: service.last_section_at,
        })
    }

    pub fn snapshot(&self) -> Vec<EitServiceCompletion> {
        self.services
            .keys()
            .filter_map(|key| self.completion(key))
            .collect()
    }

    pub fn total_sections_seen(&self) -> u64 {
        self.total_sections_seen
    }
    pub fn tracked_services(&self) -> usize {
        self.services.len()
    }
    pub fn dropped_sections(&self) -> u64 {
        self.dropped_sections
    }
}

impl Default for EpgSectionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify an EIT table ID. 0x4E/0x4F are present/following, 0x50..=0x57 and
/// 0x60..=0x67 are schedule basic, 0x58..=0x5F and 0x68..=0x6F are schedule extended.
pub fn eit_kind(id: u8) -> EitKind {
    if id <= 0x4F {
        EitKind::PresentFollowing
    } else if id <= 0x57 || (0x60..=0x67).contains(&id) {
        EitKind::ScheduleBasic
    } else {
        EitKind::ScheduleExtended
    }
}

fn schedule_base(tables: &BTreeMap<u8, SubTableState>) -> Option<u8> {
    if tables.keys().any(|&id| (0x50..=0x57).contains(&id)) {
        Some(0x50)
    } else if tables.keys().any(|&id| (0x60..=0x67).contains(&id)) {
        Some(0x60)
    } else {
        None
    }
}

fn schedule_last(tables: &BTreeMap<u8, SubTableState>, base: u8, cap: u8) -> u8 {
    tables
        .iter()
        .filter(|(&id, _)| {
            id >= base
                && id <= cap
                && matches!(
                    eit_kind(id),
                    EitKind::ScheduleBasic | EitKind::ScheduleExtended
                )
        })
        .map(|(_, sub)| sub.last_table_id.min(cap))
        .max()
        .unwrap_or(base.saturating_sub(1))
}

fn range_complete(tables: &BTreeMap<u8, SubTableState>, first: u8, last: u8) -> bool {
    first <= last && (first..=last).all(|id| tables.get(&id).is_some_and(subtable_complete))
}

fn subtable_complete(sub: &SubTableState) -> bool {
    (0..=sub.last_section_number)
        .all(|number| sub.received[number as usize / 64] & (1u64 << (number as usize % 64)) != 0)
}

fn segment_complete(sub: &SubTableState, segment: usize) -> bool {
    let Some(last) = sub.segment_last.get(segment).and_then(|last| *last) else {
        return false;
    };
    let first = segment * 8;
    (last as usize >= first && last <= sub.last_section_number)
        && (first..=last as usize)
            .all(|number| sub.received[number / 64] & (1u64 << (number % 64)) != 0)
}

fn coverage_until(service: &ServiceState, base: u8, last: u8, now: Option<i64>) -> Option<i64> {
    let midnight = jst_midnight(now?);
    let mut segments = 0i64;
    for id in base..=last {
        let Some(sub) = service.sub_tables.get(&id) else {
            return (segments > 0).then_some(midnight + segments * 3 * 3600);
        };
        for segment in 0..32 {
            if !segment_complete(sub, segment) {
                return (segments > 0).then_some(midnight + segments * 3 * 3600);
            }
            segments += 1;
        }
    }
    (segments > 0).then_some(midnight + segments * 3 * 3600)
}

fn jst_midnight(now: i64) -> i64 {
    ((now + 9 * 3600).div_euclid(86400) * 86400) - 9 * 3600
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::psi::crc32_mpeg2;

    fn section_bytes(
        id: u8,
        version: u8,
        number: u8,
        last: u8,
        segment_last: u8,
        current: bool,
    ) -> Vec<u8> {
        section_bytes_with_last_table(id, version, number, last, segment_last, current, id)
    }

    fn section_bytes_with_last_table(
        id: u8,
        version: u8,
        number: u8,
        last: u8,
        segment_last: u8,
        current: bool,
        announced_last_table: u8,
    ) -> Vec<u8> {
        let mut bytes = vec![
            id,
            0xB0,
            15,
            0x9A,
            0xBC,
            0xC0 | ((version & 0x1F) << 1) | current as u8,
            number,
            last,
            0x12,
            0x34,
            0x56,
            0x78,
            segment_last,
            announced_last_table,
        ];
        let crc = crc32_mpeg2(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn parse_section(bytes: &[u8]) -> PsiSection<'_> {
        PsiSection::parse(bytes).expect("valid test section")
    }

    #[test]
    fn pf_single_section_completes() {
        let mut t = EpgSectionTracker::new();
        let b = section_bytes(0x4E, 1, 0, 0, 0, true);
        let s = parse_section(&b);
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert_eq!(t.observe(pid::EIT, &s, 0), ObserveOutcome::Recorded);
        assert!(t
            .completion(&key)
            .is_some_and(|completion| completion.pf_complete));
    }

    #[test]
    fn schedule_segments_and_holes() {
        let mut t = EpgSectionTracker::new();
        for n in [0, 2] {
            let b = section_bytes(0x50, 1, n, 2, 2, true);
            let s = parse_section(&b);
            t.observe(pid::EIT, &s, 0);
        }
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert!(t
            .completion(&key)
            .is_some_and(|completion| !completion.schedule_basic_complete));
        let b = section_bytes(0x50, 1, 1, 2, 2, true);
        let s = parse_section(&b);
        t.observe(pid::EIT, &s, 0);
        assert!(t
            .completion(&key)
            .is_some_and(|completion| completion.schedule_basic_complete));
    }

    #[test]
    fn duplicate_version_wrap_and_limit() {
        let mut t = EpgSectionTracker::with_service_limit(1);
        let b = section_bytes(0x4E, 31, 0, 0, 0, true);
        let s = parse_section(&b);
        assert_eq!(t.observe(pid::EIT, &s, 0), ObserveOutcome::Recorded);
        assert_eq!(t.observe(pid::EIT, &s, 0), ObserveOutcome::Duplicate);
        let b0 = section_bytes(0x4E, 0, 0, 1, 0, true);
        let s0 = parse_section(&b0);
        assert_eq!(t.observe(pid::EIT, &s0, 1), ObserveOutcome::VersionChanged);
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert!(t
            .completion(&key)
            .is_some_and(|completion| !completion.pf_complete));
        let old_bytes = section_bytes(0x4E, 31, 0, 0, 0, true);
        let old = parse_section(&old_bytes);
        assert_eq!(t.observe(pid::EIT, &old, 2), ObserveOutcome::Ignored);
        let other_bytes = section_bytes(0x4E, 1, 0, 0, 0, true);
        let other = parse_section(&other_bytes);
        assert_eq!(t.observe(0x13, &other, 3), ObserveOutcome::Dropped);
        assert_eq!(t.dropped_sections(), 1);
    }

    #[test]
    fn ignored_current_next_short_and_mobile_schedule() {
        let mut t = EpgSectionTracker::new();
        assert_eq!(
            t.observe(
                pid::EIT,
                &parse_section(&section_bytes(0x4E, 1, 0, 0, 0, false)),
                0
            ),
            ObserveOutcome::Ignored
        );
        assert_eq!(
            t.observe(
                pid::EIT_MOBILE,
                &parse_section(&section_bytes(0x50, 1, 0, 0, 0, true)),
                0
            ),
            ObserveOutcome::Ignored
        );
        let short = PsiSection {
            header: parse_section(&section_bytes(0x4E, 1, 0, 0, 0, true)).header,
            data: &[1, 2, 3, 4, 5],
            crc32: 0,
        };
        assert_eq!(t.observe(pid::EIT, &short, 0), ObserveOutcome::Ignored);
    }

    #[test]
    fn coverage_uses_completed_segments_and_empty_sections() {
        let mut t = EpgSectionTracker::new();
        for n in 0..=7 {
            let b = section_bytes(0x50, 1, n, 15, 7, true);
            let s = parse_section(&b);
            assert_eq!(
                t.observe(pid::EIT, &s, 1_725_168_000),
                ObserveOutcome::Recorded
            );
        }
        for n in 8..=15 {
            let b = section_bytes(0x50, 1, n, 15, 15, true);
            let s = parse_section(&b);
            t.observe(pid::EIT, &s, 1_725_168_000);
        }
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert_eq!(
            t.completion(&key)
                .and_then(|completion| completion.schedule_coverage_until),
            Some(jst_midnight(1_725_168_000) + 6 * 3600)
        );
    }

    #[test]
    fn out_of_order_sections_complete_same_as_in_order() {
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        let mut ordered = EpgSectionTracker::new();
        let mut shuffled = EpgSectionTracker::new();
        for n in 0..=3 {
            let b = section_bytes(0x50, 1, n, 3, 3, true);
            ordered.observe(pid::EIT, &parse_section(&b), 0);
        }
        for n in [3, 1, 0, 2] {
            let b = section_bytes(0x50, 1, n, 3, 3, true);
            shuffled.observe(pid::EIT, &parse_section(&b), 0);
        }
        assert_eq!(ordered.completion(&key), shuffled.completion(&key));
    }

    #[test]
    fn empty_section_counts_as_received() {
        let mut tracker = EpgSectionTracker::new();
        for n in 0..=7 {
            let b = section_bytes(0x50, 1, n, 7, 7, true);
            assert_eq!(
                tracker.observe(pid::EIT, &parse_section(&b), 0),
                ObserveOutcome::Recorded
            );
        }
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert!(tracker
            .completion(&key)
            .is_some_and(|completion| completion.schedule_basic_complete));
    }

    #[test]
    fn service_limit_still_accepts_updates_for_tracked_service() {
        let mut tracker = EpgSectionTracker::with_service_limit(1);
        let first = section_bytes(0x4E, 1, 0, 1, 0, true);
        assert_eq!(
            tracker.observe(pid::EIT, &parse_section(&first), 0),
            ObserveOutcome::Recorded
        );
        let second_service = {
            let mut bytes = first.clone();
            bytes[3] = 0x9A;
            bytes[4] = 0xBD;
            let crc = crc32_mpeg2(&bytes[..bytes.len() - 4]);
            let crc_offset = bytes.len() - 4;
            bytes[crc_offset..].copy_from_slice(&crc.to_be_bytes());
            bytes
        };
        assert_eq!(
            tracker.observe(pid::EIT, &parse_section(&second_service), 1),
            ObserveOutcome::Dropped
        );
        let update = section_bytes(0x4E, 1, 1, 1, 0, true);
        assert_eq!(
            tracker.observe(pid::EIT, &parse_section(&update), 2),
            ObserveOutcome::Recorded
        );
    }

    #[test]
    fn coverage_stops_at_first_incomplete_segment() {
        let mut tracker = EpgSectionTracker::new();
        for n in 0..=7 {
            let b = section_bytes(0x50, 1, n, 15, 7, true);
            tracker.observe(pid::EIT, &parse_section(&b), 1_725_168_000);
        }
        for n in [8, 10, 11, 12, 13, 14, 15] {
            let b = section_bytes(0x50, 1, n, 15, 15, true);
            tracker.observe(pid::EIT, &parse_section(&b), 1_725_168_000);
        }
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert_eq!(
            tracker
                .completion(&key)
                .and_then(|c| c.schedule_coverage_until),
            Some(jst_midnight(1_725_168_000) + 3 * 3600)
        );
    }

    #[test]
    fn coverage_survives_missing_later_sub_table() {
        let mut tracker = EpgSectionTracker::new();
        for n in 0..=7 {
            let b = section_bytes_with_last_table(0x50, 1, n, 7, 7, true, 0x51);
            tracker.observe(pid::EIT, &parse_section(&b), 1_725_168_000);
        }
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        assert_eq!(
            tracker
                .completion(&key)
                .and_then(|c| c.schedule_coverage_until),
            Some(jst_midnight(1_725_168_000) + 3 * 3600)
        );
    }

    #[test]
    fn pf_needs_all_sections() {
        let mut tracker = EpgSectionTracker::new();
        let key = EitServiceKey {
            pid: pid::EIT,
            original_network_id: 0x5678,
            transport_stream_id: 0x1234,
            service_id: 0x9ABC,
        };
        let b0 = section_bytes(0x4E, 1, 0, 1, 0, true);
        tracker.observe(pid::EIT, &parse_section(&b0), 0);
        assert!(tracker.completion(&key).is_some_and(|c| !c.pf_complete));
        let b1 = section_bytes(0x4E, 1, 1, 1, 1, true);
        tracker.observe(pid::EIT, &parse_section(&b1), 0);
        assert!(tracker.completion(&key).is_some_and(|c| c.pf_complete));
    }

    #[test]
    fn version_wrap_31_to_0_is_newer_but_0_to_31_is_ignored() {
        let mut tracker = EpgSectionTracker::new();
        let b31 = section_bytes(0x4E, 31, 0, 1, 0, true);
        assert_eq!(
            tracker.observe(pid::EIT, &parse_section(&b31), 0),
            ObserveOutcome::Recorded
        );
        let b0 = section_bytes(0x4E, 0, 0, 1, 0, true);
        assert_eq!(
            tracker.observe(pid::EIT, &parse_section(&b0), 1),
            ObserveOutcome::VersionChanged
        );
        let b31_again = section_bytes(0x4E, 31, 0, 1, 0, true);
        assert_eq!(
            tracker.observe(pid::EIT, &parse_section(&b31_again), 2),
            ObserveOutcome::Ignored
        );
    }

    #[test]
    fn malformed_short_section_does_not_change_state() {
        let mut tracker = EpgSectionTracker::new();
        let valid = section_bytes(0x4E, 1, 0, 0, 0, true);
        let short = PsiSection {
            header: parse_section(&valid).header,
            data: &[1, 2, 3, 4, 5],
            crc32: 0,
        };
        assert_eq!(
            tracker.observe(pid::EIT, &short, 0),
            ObserveOutcome::Ignored
        );
        assert_eq!(tracker.total_sections_seen(), 0);
        assert_eq!(tracker.tracked_services(), 0);
    }
}
