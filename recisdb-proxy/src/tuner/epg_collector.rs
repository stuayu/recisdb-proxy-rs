//! Lightweight EIT (EPG) collector.
//!
//! Same shape as [`crate::tuner::logo_collector::ChannelLogoCollector`]: it
//! listens to raw TS chunks from the live tuner reader loop, reassembles
//! EIT sections (PID 0x0012), and forwards parsed events best-effort. It is
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

use std::sync::OnceLock;

use crate::database::ProgramUpsert;
use crate::ts_analyzer::{pid, table_id, EitTable, PsiSection, SectionCollector, TsPacket, TS_PACKET_SIZE};

/// Process-wide sender for parsed EPG rows. See module doc comment.
static EPG_SENDER: OnceLock<mpsc::UnboundedSender<ProgramUpsert>> = OnceLock::new();

/// Install the process-wide EPG sender. Called once from
/// `crate::epg_writer::EpgWriter::new`. Returns `false` (leaving the
/// previously-installed sender in place) if a sender was already set.
pub fn set_global_sender(tx: mpsc::UnboundedSender<ProgramUpsert>) -> bool {
    EPG_SENDER.set(tx).is_ok()
}

fn global_sender() -> Option<&'static mpsc::UnboundedSender<ProgramUpsert>> {
    EPG_SENDER.get()
}

const EIT_PID: u16 = pid::EIT;

/// Collects EIT sections from a live TS stream and forwards parsed events.
pub struct EpgCollector {
    collector: SectionCollector,
}

impl EpgCollector {
    pub fn new() -> Self {
        Self { collector: SectionCollector::new() }
    }

    /// Feed a raw chunk of TS packets (as read from the tuner). Best-effort:
    /// malformed packets/sections are silently skipped, same convention as
    /// [`crate::tuner::logo_collector::ChannelLogoCollector::process_ts_chunk`].
    pub fn process_ts_chunk(&mut self, data: &[u8]) {
        let mut offset = 0usize;
        while offset + TS_PACKET_SIZE <= data.len() {
            if data[offset] != 0x47 {
                offset += 1;
                continue;
            }

            if let Ok(packet) = TsPacket::parse(&data[offset..offset + TS_PACKET_SIZE]) {
                self.process_packet(&packet);
            }

            offset += TS_PACKET_SIZE;
        }
    }

    fn process_packet(&mut self, packet: &TsPacket<'_>) {
        if packet.header.pid != EIT_PID {
            return;
        }
        if packet.header.transport_error || packet.header.is_scrambled() || !packet.header.has_payload() {
            return;
        }

        let sections = self.collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );
        for section_data in &sections {
            self.process_section(section_data);
        }
    }

    fn process_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };
        if !table_id::is_eit_table_id(section.header.table_id) {
            return;
        }
        let Ok(eit) = EitTable::parse(&section) else {
            return;
        };
        if eit.events.is_empty() {
            return;
        }

        let Some(tx) = global_sender() else {
            trace!(
                "[EpgCollector] no writer installed, dropping {} event(s) for sid={}",
                eit.events.len(),
                eit.service_id
            );
            return;
        };

        let now = chrono::Utc::now().timestamp();
        for event in eit.events {
            let record = ProgramUpsert {
                nid: eit.original_network_id,
                sid: eit.service_id,
                tsid: eit.transport_stream_id,
                event_id: event.event_id,
                start_at: event.start_at,
                duration_secs: event.duration_secs as i64,
                name: non_empty(event.name),
                description: non_empty(event.description),
                extended: non_empty(event.extended),
                genre: event.genre.map(|g| g as i64),
                updated_at: now,
            };
            if tx.send(record).is_err() {
                debug!("[EpgCollector] writer task gone, dropping remaining events for this section");
                break;
            }
        }
    }
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

    #[test]
    fn test_non_empty() {
        assert_eq!(non_empty(String::new()), None);
        assert_eq!(non_empty("x".to_string()), Some("x".to_string()));
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
