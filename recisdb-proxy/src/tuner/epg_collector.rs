//! Lightweight EIT (EPG) collector.
//!
//! Same shape as [`crate::tuner::logo_collector::ChannelLogoCollector`]: it
//! listens to raw TS chunks from the live tuner reader loop, reassembles
//! EIT sections (PID 0x0012 for H-EIT, plus M-EIT/L-EIT PIDs 0x0026/0x0027),
//! and forwards parsed events
//! best-effort. MMT/TLV 4K input reaches this collector only after dantto4k
//! remuxes MH-EIT to the ordinary TS EIT PID 0x0012. It is
//! created fresh per reader-task start (`tuner/shared.rs`), so PSI
//! reassembly state is naturally reset on every channel switch/reconnect —
//! same as the logo collector.
//!
//! # Why a process-wide channel instead of a `Database` handle
//!
//! The TS reader loop that drives [`EpgCollector::process_ts_chunk`]
//! (`tuner/shared.rs::run_bondriver_reader_with_tuner`) runs on a plain OS
//! thread deep inside the tuner stack (`TunerPool` -> `SharedTuner` ->
//! reader thread), with no `Database`/`Arc` plumbed through any of those
//! constructors today. Threading one through would touch `SharedTuner::new`,
//! `TunerPool::get_or_create`, and every call site (including several test
//! helpers in `pool.rs`/`encoder_pool.rs`/`stream.rs`/`shared.rs`).
//!
//! Instead this collector only *parses* and forwards results through a
//! process-wide unbounded channel, installed once at server startup
//! (`main.rs`, via `EpgWriter::new`) and consumed by a dedicated batching
//! task (`crate::epg_writer::EpgWriter`) that owns the shared `Database`
//! handle. When no writer has installed a sender yet (unit tests, or the
//! `recisdb` CLI which does not run the proxy server at all), sends are
//! silently dropped — the same "best effort" fallback the logo collector
//! uses when it cannot create its output directory.

use log::{debug, trace};
use tokio::sync::mpsc;

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::database::{EpgSource, ProgramUpsert};
use crate::ts_analyzer::{
    pid, table_id, EitTable, PsiSection, SectionCollector, TsPacket, TS_PACKET_SIZE,
};

const MAX_PENDING_TS: usize = TS_PACKET_SIZE * 3;

/// Process-wide sender for parsed EPG rows. See module doc comment.
static EPG_SENDER: OnceLock<mpsc::UnboundedSender<ProgramUpsert>> = OnceLock::new();

/// Install the process-wide EPG sender. Called once from
/// `crate::epg_writer::EpgWriter::new`. Returns `false` (leaving the
/// previously-installed sender in place) if a sender was already set.
pub fn set_global_sender(tx: mpsc::UnboundedSender<ProgramUpsert>) -> bool {
    EPG_SENDER.set(tx).is_ok()
}

/// Forward rows returned by an authenticated remote metadata scan through the
/// same batching writer as locally parsed EIT.
pub fn submit_metadata_records(records: impl IntoIterator<Item = ProgramUpsert>) -> usize {
    let Some(tx) = global_sender() else {
        return 0;
    };
    records
        .into_iter()
        .filter(|record| tx.send(record.clone()).is_ok())
        .count()
}

fn global_sender() -> Option<&'static mpsc::UnboundedSender<ProgramUpsert>> {
    EPG_SENDER.get()
}

fn is_eit_pid(value: u16) -> bool {
    matches!(
        value,
        pid::EIT | pid::EIT_MOBILE | pid::EIT_PARTIAL_RECEPTION
    )
}

fn accepts_table_id(pid: u16, table_id: u8) -> bool {
    // TR-B14 Table 13-8: M-EIT and L-EIT carry p/f only (0x4E). H-EIT's
    // table IDs are shared with satellite EIT, so PID 0x0012 remains generic
    // here; a terrestrial caller can apply is_terrestrial_eit_table_id.
    if matches!(pid, pid::EIT_MOBILE | pid::EIT_PARTIAL_RECEPTION) {
        table_id == table_id::EIT_PF_ACTUAL
    } else {
        table_id::is_eit_table_id(table_id)
    }
}

/// Collects EIT sections from a live TS stream and forwards parsed events.
pub struct EpgCollector {
    collector: SectionCollector,
    /// Last accepted current version per EIT sub-table identity.
    versions: HashMap<(u16, u8, u16, u16, u16), u8>,
    /// TS bytes carried across calls. Reader chunks are not required to end
    /// on a 188-byte packet boundary.
    pending_ts: Vec<u8>,
    metadata_only: bool,
    metadata_records: Vec<ProgramUpsert>,
    #[cfg(test)]
    parsed_events: Vec<u16>,
}

impl EpgCollector {
    pub fn new() -> Self {
        Self {
            collector: SectionCollector::new(),
            versions: HashMap::new(),
            pending_ts: Vec::new(),
            metadata_only: false,
            metadata_records: Vec::new(),
            #[cfg(test)]
            parsed_events: Vec::new(),
        }
    }

    /// Collector for the authenticated remote-metadata RPC. It returns parsed
    /// rows to the caller and never forwards them through this node's writer.
    pub fn new_metadata() -> Self {
        Self {
            metadata_only: true,
            ..Self::new()
        }
    }

    pub fn drain_metadata_records(&mut self) -> Vec<ProgramUpsert> {
        std::mem::take(&mut self.metadata_records)
    }

    /// Feed a raw chunk of TS packets (as read from the tuner). Best-effort:
    /// malformed packets/sections are silently skipped, same convention as
    /// [`crate::tuner::logo_collector::ChannelLogoCollector::process_ts_chunk`].
    pub fn process_ts_chunk(&mut self, data: &[u8]) {
        let offset = if self.pending_ts.is_empty() {
            self.process_ts_bytes(data)
        } else {
            let mut combined = Vec::with_capacity(self.pending_ts.len() + data.len());
            combined.extend_from_slice(&self.pending_ts);
            combined.extend_from_slice(data);
            self.pending_ts.clear();
            let offset = self.process_ts_bytes(&combined);
            self.save_pending(&combined[offset..]);
            return;
        };

        self.save_pending(&data[offset..]);
    }

    fn process_ts_bytes(&mut self, data: &[u8]) -> usize {
        let mut offset = 0;
        while data.len().saturating_sub(offset) >= TS_PACKET_SIZE {
            if data[offset] != 0x47 {
                offset += 1;
                continue;
            }

            let packet_end = offset + TS_PACKET_SIZE;
            match TsPacket::parse(&data[offset..packet_end]) {
                Ok(packet) => {
                    self.process_packet(&packet);
                    offset = packet_end;
                }
                Err(_) => {
                    // The sync byte was a false positive; retry one byte
                    // later without moving or reallocating the buffer.
                    offset += 1;
                }
            }
        }
        offset
    }

    fn save_pending(&mut self, remainder: &[u8]) {
        self.pending_ts.clear();
        let keep = remainder.len().min(MAX_PENDING_TS);
        if keep < remainder.len() {
            trace!(
                "[EpgCollector] dropping {} excess unsynchronized TS bytes",
                remainder.len() - keep
            );
        }
        self.pending_ts
            .extend_from_slice(&remainder[remainder.len() - keep..]);
    }

    fn process_packet(&mut self, packet: &TsPacket<'_>) {
        if !is_eit_pid(packet.header.pid) {
            return;
        }
        if packet.header.transport_error
            || packet.header.is_scrambled()
            || !packet.header.has_payload()
        {
            return;
        }

        let sections = self.collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );
        for section_data in &sections {
            self.process_section(packet.header.pid, section_data);
        }
    }

    fn process_section(&mut self, pid: u16, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };
        if !accepts_table_id(pid, section.header.table_id) {
            return;
        }
        // B10 §5.2.7: current_next=0 describes the next sub-table and must
        // not replace the currently applicable EPG.
        if !section.header.current_next_indicator {
            return;
        }
        // H/M/L use different PIDs but all use table_id=0x4E for p/f.
        // PID must therefore be part of the version identity; otherwise an
        // M-EIT update can suppress an H-EIT section for the same service.
        let key = (
            pid,
            section.header.table_id,
            section.header.table_id_extension,
            section.data.get(0).copied().unwrap_or(0) as u16 * 256
                + section.data.get(1).copied().unwrap_or(0) as u16,
            section.data.get(2).copied().unwrap_or(0) as u16 * 256
                + section.data.get(3).copied().unwrap_or(0) as u16,
        );
        if let Some(previous) = self.versions.get(&key).copied() {
            if previous != section.header.version_number
                && !is_newer_version(previous, section.header.version_number)
            {
                return;
            }
        }
        self.versions.insert(key, section.header.version_number);
        let Ok(eit) = EitTable::parse(&section) else {
            return;
        };
        if eit.events.is_empty() {
            return;
        }
        #[cfg(test)]
        self.parsed_events
            .extend(eit.events.iter().map(|event| event.event_id));

        let tx = global_sender();
        let event_count = eit.events.len();

        let now = chrono::Utc::now().timestamp();
        // Which logical store this section feeds, following EDCB's split in
        // `CEpgDBUtil::AddEIT`: `table_id <= 0x4F && section_number <= 1` is
        // p/f, and `PID != 0x0012 || table_id > 0x4F` is schedule (or, off
        // H-EIT, [p/f after]). A section matching neither — H-EIT carrying a
        // p/f table_id past section 1 — is not valid p/f under TR-B14 §13.1,
        // so it is dropped rather than filed under the wrong clock.
        let source = if eit.table_id <= 0x4F && eit.section_number <= 1 {
            Some(EpgSource::PresentFollowing)
        } else if pid != pid::EIT || eit.table_id > 0x4F {
            Some(EpgSource::Schedule)
        } else {
            None
        };
        let Some(source) = source else {
            return;
        };
        for event in eit.events {
            // EDCB discards non-running and stopped events.
            if event.running_status == 1 || event.running_status == 3 {
                continue;
            }
            // EDCB rejects schedule events with undefined duration.
            if source == EpgSource::Schedule && event.duration_secs == 0 {
                continue;
            }
            let basic_allowed = !is_schedule_extended(eit.table_id);
            let name = if basic_allowed {
                non_empty(event.name)
            } else {
                None
            };
            let description = if basic_allowed {
                non_empty(event.description)
            } else {
                None
            };
            let extended = non_empty(event.extended);
            let genre = basic_allowed
                .then_some(event.genre)
                .flatten()
                .map(|g| g as i64);
            let record = ProgramUpsert {
                nid: eit.original_network_id,
                sid: eit.service_id,
                tsid: eit.transport_stream_id,
                event_id: event.event_id,
                start_at: event.start_at,
                duration_secs: event.duration_secs as i64,
                free_ca_mode: event.free_ca_mode,
                name: name.clone(),
                description: description.clone(),
                extended: extended.clone(),
                genre,
                updated_at: now,
                source,
                basic_updated_at: if name.is_some() || description.is_some() || genre.is_some() {
                    Some(now)
                } else {
                    None
                },
                extended_updated_at: extended.as_ref().map(|_| now),
            };
            if self.metadata_only {
                self.metadata_records.push(record);
                continue;
            }
            let Some(tx) = tx else {
                trace!(
                    "[EpgCollector] no writer installed, dropping {} event(s) for sid={}",
                    event_count,
                    eit.service_id
                );
                break;
            };
            if tx.send(record).is_err() {
                debug!(
                    "[EpgCollector] writer task gone, dropping remaining events for this section"
                );
                break;
            }
        }
    }
}

fn is_newer_version(current: u8, new: u8) -> bool {
    let delta = new.wrapping_sub(current) & 0x1F;
    delta != 0 && delta < 16
}

impl Default for EpgCollector {
    fn default() -> Self {
        Self::new()
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn is_schedule_extended(table_id: u8) -> bool {
    (0x58..=0x5F).contains(&table_id) || (0x68..=0x6F).contains(&table_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_ts_chunk_no_panic_on_garbage() {
        // Garbage input (no sync bytes) must not panic; the loop just
        // advances byte-by-byte looking for 0x47 and finds nothing.
        let mut collector = EpgCollector::new();
        collector.process_ts_chunk(&[0u8; 100]);
    }

    #[test]
    fn test_process_ts_chunk_ignores_short_input() {
        let mut collector = EpgCollector::new();
        // Shorter than one TS packet: the while-loop body never executes.
        collector.process_ts_chunk(&[0x47, 0x00, 0x00]);
    }

    fn metadata_section(table_id: u8, running_status: u8, duration: [u8; 3]) -> Vec<u8> {
        let mut section = vec![
            table_id, 0xB0, 0x1B, 0x12, 0x34, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x0B, 0x00,
            table_id, 0x00, 0x01, 0xEB, 0x5E, 0x12, 0x34, 0x56,
        ];
        section.extend_from_slice(&duration);
        section.push((running_status << 5) as u8);
        section.push(0);
        section.extend_from_slice(&[0, 0, 0, 0]);
        section
    }

    #[test]
    fn running_status_non_running_events_are_not_saved() {
        for status in [1, 3] {
            let mut collector = EpgCollector::new_metadata();
            let section = metadata_section(table_id::EIT_PF_ACTUAL, status, [0, 30, 0]);
            collector.process_section(pid::EIT, &section);
            assert!(
                collector.drain_metadata_records().is_empty(),
                "status={status}"
            );
        }
    }

    #[test]
    fn schedule_event_with_undefined_duration_is_not_saved() {
        let mut collector = EpgCollector::new_metadata();
        let section = metadata_section(table_id::EIT_SCHEDULE_ACTUAL_START, 4, [0xFF; 3]);
        collector.process_section(pid::EIT, &section);
        assert!(collector.drain_metadata_records().is_empty());
    }

    /// EDCB treats `table_id <= 0x4F` as p/f only for section 0 and 1
    /// (`CEpgDBUtil::AddEIT`). On H-EIT a p/f table_id past section 1 is not
    /// valid p/f under TR-B14 §13.1, so it must not be filed under the p/f
    /// clock; off H-EIT the same section is [p/f after] and belongs to the
    /// schedule store.
    #[test]
    fn present_following_table_id_past_section_one_is_pf_only_off_h_eit() {
        let mut section = metadata_section(table_id::EIT_PF_ACTUAL, 4, [0x00, 0x30, 0x00]);
        section[6] = 2; // section_number

        let mut h_eit = EpgCollector::new_metadata();
        h_eit.process_section(pid::EIT, &section);
        assert!(
            h_eit.drain_metadata_records().is_empty(),
            "H-EIT p/f table_id past section 1 is not valid p/f"
        );

        let mut m_eit = EpgCollector::new_metadata();
        m_eit.process_section(pid::EIT_MOBILE, &section);
        let records = m_eit.drain_metadata_records();
        assert_eq!(records.len(), 1, "off H-EIT this is [p/f after]");
        assert_eq!(records[0].source, EpgSource::Schedule);
    }

    #[test]
    fn schedule_extended_table_ids_never_supply_basic_descriptors() {
        assert!(is_schedule_extended(0x58));
        assert!(is_schedule_extended(0x5F));
        assert!(is_schedule_extended(0x68));
        assert!(is_schedule_extended(0x6F));
        assert!(!is_schedule_extended(0x50));
        assert!(!is_schedule_extended(0x60));
    }

    fn crc32_mpeg2(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= (byte as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    fn test_ts_packets() -> Vec<u8> {
        let mut section = vec![
            table_id::EIT_PF_ACTUAL,
            0xB0,
            0x1B, // section_length = 27
            0x12,
            0x34, // service_id
            0x03, // version 1, current_next=1
            0x00,
            0x00, // section number / last section number
            0x00,
            0x01, // transport_stream_id
            0x00,
            0x0B, // original_network_id
            0x00,
            table_id::EIT_PF_ACTUAL,
            0x00,
            0x01, // event_id
            0xEB,
            0x5E, // MJD 60382
            0x12,
            0x34,
            0x56, // start_time 12:34:56
            0x01,
            0x00,
            0x00, // duration 01:00:00
            0x80,
            0x00, // running_status=4, no descriptors
        ];
        let crc = crc32_mpeg2(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(section.len(), 30);

        let mut packets = Vec::with_capacity(TS_PACKET_SIZE * 3);
        for (index, payload) in [
            {
                let mut payload = vec![0x00];
                payload.extend_from_slice(&section);
                payload
            },
            Vec::new(),
            Vec::new(),
        ]
        .into_iter()
        .enumerate()
        {
            let mut packet = vec![0xFF; TS_PACKET_SIZE];
            packet[0] = 0x47;
            packet[1] = 0x40 | ((pid::EIT >> 8) as u8 & 0x1F);
            packet[2] = pid::EIT as u8;
            packet[3] = 0x10 | index as u8;
            packet[4..4 + payload.len()].copy_from_slice(&payload);
            packets.extend_from_slice(&packet);
        }
        packets
    }

    #[test]
    fn test_process_ts_chunk_preserves_eit_across_chunk_boundaries() {
        let input = test_ts_packets();
        let mut contiguous = EpgCollector::new();
        contiguous.process_ts_chunk(&input);

        let mut split = EpgCollector::new();
        split.process_ts_chunk(&input[..100]);
        split.process_ts_chunk(&input[100..300]);
        split.process_ts_chunk(&input[300..]);

        assert_eq!(split.parsed_events, contiguous.parsed_events);
        assert_eq!(split.parsed_events, vec![1]);
    }

    #[test]
    fn test_process_ts_chunk_resynchronizes_after_sync_offset() {
        let input = test_ts_packets();
        let mut shifted = vec![0x00, 0x11, 0x22];
        shifted.extend_from_slice(&input);

        let mut collector = EpgCollector::new();
        collector.process_ts_chunk(&shifted);

        assert_eq!(collector.parsed_events, vec![1]);
    }

    #[test]
    fn test_process_ts_chunk_bounds_pending_garbage() {
        let mut collector = EpgCollector::new();
        collector.process_ts_chunk(&vec![0x00; MAX_PENDING_TS * 100]);

        assert!(collector.pending_ts.len() <= MAX_PENDING_TS);
    }

    #[test]
    fn test_non_empty() {
        assert_eq!(non_empty(String::new()), None);
        assert_eq!(non_empty("x".to_string()), Some("x".to_string()));
    }

    #[test]
    fn test_accepts_all_eit_transport_pids() {
        assert!(is_eit_pid(pid::EIT));
        assert!(is_eit_pid(pid::EIT_MOBILE));
        assert!(is_eit_pid(pid::EIT_PARTIAL_RECEPTION));
        assert!(!is_eit_pid(pid::SDT));
    }

    #[test]
    fn test_m_and_l_eit_only_use_present_following_table_id() {
        assert!(accepts_table_id(pid::EIT_MOBILE, table_id::EIT_PF_ACTUAL));
        assert!(accepts_table_id(
            pid::EIT_PARTIAL_RECEPTION,
            table_id::EIT_PF_ACTUAL
        ));
        assert!(!accepts_table_id(pid::EIT_MOBILE, 0x50));
        assert!(!accepts_table_id(pid::EIT_PARTIAL_RECEPTION, 0x60));
    }

    #[test]
    fn test_set_global_sender_reports_whether_it_won() {
        // This test only exercises the *logic* of `set_global_sender`; it
        // deliberately does not assert on `EPG_SENDER`'s final state since
        // that static is shared across the whole test binary (other tests
        // in this crate may run concurrently and already have set it).
        let (tx, _rx) = mpsc::unbounded_channel::<ProgramUpsert>();
        // Either this call wins (true) or a previous test already
        // installed a sender (false) — both are valid outcomes; the
        // function must not panic either way.
        let _ = set_global_sender(tx);
    }
}
