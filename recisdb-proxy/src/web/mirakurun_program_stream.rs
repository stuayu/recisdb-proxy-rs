//! `GET /mirakurun/api/programs/:id/stream` gating (STREAMING_DESIGN.md §7.1,
//! `docs/EPGSTATION_COMPAT.md` §3/§5).
//!
//! EPGStation's EPG-reservation recording resolves a `reserve` to a program
//! id, then opens this endpoint expecting the body to start emitting bytes
//! only once the target event has actually become EIT[p/f] "present" (not at
//! `reserve.startAt` — programs slip). Real Mirakurun implements this by
//! watching its own live EIT[p/f] cache; this project has no such
//! process-wide cache (`tuner/epg_collector.rs` only *writes* to the
//! `programs` table, it does not expose "what is present right now"), so
//! this module reassembles EIT[p/f] sections itself, directly off the same
//! TS chunks the HTTP body would otherwise pass straight through.
//!
//! Two pieces, deliberately separated so the state machine is unit-testable
//! without any TS/broadcast plumbing:
//! - [`ProgramGate`]: pure state machine — "has the target event become
//!   present yet, and has it since stopped being present".
//! - [`gated_program_stream`]: wires a [`ProgramGate`] to a live
//!   [`TunerSubscription`], parsing EIT out of each chunk with a
//!   purpose-built lightweight [`EitPfCollector`] (same TS/PSI reassembly
//!   pattern as `tuner/epg_collector.rs::EpgCollector`, but keeping only the
//!   present/following table_id, and reporting straight to the gate instead
//!   of the DB).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::{self, Stream};
use log::{debug, warn};
use tokio::sync::broadcast;

use crate::tuner::TunerSubscription;
use crate::ts_analyzer::service_filter::TsServiceFilter;
use crate::ts_analyzer::{table_id, EitTable, PsiSection, SectionCollector, TsPacket, TS_PACKET_SIZE};
use crate::web::stream::{StreamCleanup, TsAligner};

/// How long past `programs.start_at + duration_secs` this gate keeps waiting
/// for the target event to become present before giving up. Programs
/// occasionally get bumped/cancelled after being scheduled (breaking news,
/// sports overrun elsewhere in the day, ...) and would then never post an
/// EIT[p/f] with this `event_id` as present; without a cutoff a client that
/// opens this endpoint for a program that never airs would pin a tuner
/// forever. One hour is a generous margin past the program's own scheduled
/// end — comfortably longer than any realistic slip while still bounded.
pub(crate) const PRESENT_WAIT_GRACE: Duration = Duration::from_secs(60 * 60);

// ============================================================================
// ProgramGate: pure state machine
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateState {
    /// Target event has not yet been observed as EIT[p/f] present.
    Waiting,
    /// Target event is (or was, as of the last observation) present.
    Streaming,
    /// A *different* event has since become present on the target service —
    /// the target program's broadcast has ended.
    Ended,
}

/// Pure EIT[p/f]-driven state machine deciding whether the body stream
/// should currently be emitting bytes. Holds no TS/broadcast state — see
/// module doc comment.
pub(crate) struct ProgramGate {
    target_sid: u16,
    target_event_id: u16,
    state: GateState,
}

impl ProgramGate {
    pub(crate) fn new(target_sid: u16, target_event_id: u16) -> Self {
        Self { target_sid, target_event_id, state: GateState::Waiting }
    }

    pub(crate) fn is_streaming(&self) -> bool {
        self.state == GateState::Streaming
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.state == GateState::Ended
    }

    /// Feed one already-parsed EIT p/f actual section (table_id == 0x4E).
    /// `first_event_id` is the `event_id` of the section's present event
    /// (`section_number == 0`); `section_number == 1` (following) sections
    /// carry no "is this present right now" information and are ignored.
    ///
    /// Callers are expected to have already filtered out anything that isn't
    /// `table_id == EIT_PF_ACTUAL` (0x4E) — 0x4F (other TS) and the
    /// 0x50-0x6F schedule range say nothing about what is present *right
    /// now* on *this* service and must never reach this method.
    pub(crate) fn observe_eit_pf_actual(&mut self, section_number: u8, service_id: u16, event_id: u16) {
        if self.state == GateState::Ended {
            return;
        }
        if section_number != 0 {
            // "following" section: no present-event information.
            return;
        }
        if service_id != self.target_sid {
            // Present/following on some other service on the same
            // multiplex — irrelevant to this program.
            return;
        }

        match self.state {
            GateState::Waiting => {
                if event_id == self.target_event_id {
                    self.state = GateState::Streaming;
                }
                // else: some other event is present right now; keep waiting
                // — the target event has not started yet.
            }
            GateState::Streaming => {
                if event_id != self.target_event_id {
                    self.state = GateState::Ended;
                }
            }
            GateState::Ended => unreachable!("handled above"),
        }
    }
}

// ============================================================================
// EitPfCollector: TS -> EIT p/f actual sections, feeding a ProgramGate
// ============================================================================

/// Lightweight EIT collector scoped to this gate's needs only: PID 0x0012,
/// `table_id == EIT_PF_ACTUAL` (0x4E) sections, forwarded straight into a
/// [`ProgramGate`]. Deliberately not `tuner/epg_collector.rs::EpgCollector`
/// reused — that collector's job is persisting *all* EIT (p/f *and*
/// schedule, actual *and* other) into the `programs` table; this one only
/// needs the narrow "what is present right now on this one service" signal,
/// and keeping it separate avoids coupling this request-scoped gate to the
/// process-wide `EPG_SENDER` channel `EpgCollector` writes through.
struct EitPfCollector {
    collector: SectionCollector,
}

impl EitPfCollector {
    fn new() -> Self {
        Self { collector: SectionCollector::new() }
    }

    /// Feed a raw TS chunk, reporting every EIT p/f actual (0x4E) section's
    /// (section_number, service_id, first event_id) into `gate`. Same
    /// best-effort parsing conventions as `EpgCollector::process_ts_chunk`:
    /// malformed packets/sections are silently skipped, never panics on
    /// garbage input.
    fn process_ts_chunk(&mut self, data: &[u8], gate: &mut ProgramGate) {
        let mut offset = 0usize;
        while offset + TS_PACKET_SIZE <= data.len() {
            if data[offset] != 0x47 {
                offset += 1;
                continue;
            }
            if let Ok(packet) = TsPacket::parse(&data[offset..offset + TS_PACKET_SIZE]) {
                if packet.header.pid == crate::ts_analyzer::pid::EIT
                    && !packet.header.transport_error
                    && !packet.header.is_scrambled()
                    && packet.header.has_payload()
                {
                    let sections = self.collector.add_data(
                        packet.payload,
                        packet.header.continuity_counter,
                        packet.header.payload_unit_start,
                    );
                    for section_data in &sections {
                        self.process_section(section_data, gate);
                    }
                }
            }
            offset += TS_PACKET_SIZE;
        }
    }

    fn process_section(&mut self, section_data: &[u8], gate: &mut ProgramGate) {
        let Ok(section) = PsiSection::parse(section_data) else { return };
        // Only present/following, actual TS — see `ProgramGate::observe_eit_pf_actual`
        // doc comment on why 0x4F/schedule must never reach the gate.
        if section.header.table_id != table_id::EIT_PF_ACTUAL {
            return;
        }
        let Ok(eit) = EitTable::parse(&section) else { return };
        let Some(first_event) = eit.events.first() else { return };
        gate.observe_eit_pf_actual(eit.section_number, eit.service_id, first_event.event_id);
    }
}

// ============================================================================
// gated_program_stream
// ============================================================================

struct GatedStreamState {
    rx: TunerSubscription,
    _cleanup: StreamCleanup,
    collector: EitPfCollector,
    gate: ProgramGate,
    deadline: DateTime<Utc>,
    /// Same single-service filter `GET /services/:id/stream` applies — see
    /// [`crate::web::stream::service_filtered_body_stream`]. Fed on *every*
    /// chunk, including those the gate withholds, so its PAT/PMT whitelist is
    /// already built when the gate opens and the recording does not start
    /// with a PSI-less lead-in.
    filter: TsServiceFilter,
    aligner: TsAligner,
}

/// Body stream for `GET /programs/:id/stream`: withholds every TS chunk
/// until the target event is observed as EIT[p/f] present on `target_sid`,
/// then passes chunks through (filtered down to `target_sid`, matching real
/// Mirakurun's per-service stream — see
/// [`crate::web::stream::service_filtered_body_stream`]) until a *different*
/// event becomes present, at which point the stream ends.
///
/// The chunk in which the gate transitions Waiting -> Streaming is forwarded
/// in full, not trimmed to start exactly at the EIT boundary: TS chunks are
/// not aligned to programme boundaries in the first place (a single 32/64KiB
/// broadcast chunk holds dozens of packets from every PID in the multiplex),
/// so "the first byte of the target programme" is not a well-defined offset
/// in a TS-passthrough model at all — the previous programme's tail packets
/// are already interleaved throughout any chunk that also carries the EIT
/// update announcing the switch. EPGStation only needs "recording starts at
/// or after present", not frame-accurate splicing (that is the downstream
/// recorder/demuxer's job), so this is an acceptable simplification.
///
/// `deadline`: once `Utc::now() >= deadline` while still `Waiting`, the
/// stream ends (empty body) rather than waiting forever — see
/// [`PRESENT_WAIT_GRACE`]. Checked once per chunk (not on a timer), so an
/// entirely silent multiplex (no EIT at all) would only be caught if some
/// other PID still carries the channel; this project has no independent
/// "give up if literally nothing was ever received" timeout at this layer
/// (the underlying tuner reader already applies its own read timeouts, see
/// `tuner/shared.rs`), so an actively broken tuner is not this function's
/// problem to solve twice.
pub(crate) fn gated_program_stream(
    rx: TunerSubscription,
    cleanup: StreamCleanup,
    target_sid: u16,
    target_event_id: u16,
    deadline: DateTime<Utc>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let state = GatedStreamState {
        rx,
        _cleanup: cleanup,
        collector: EitPfCollector::new(),
        gate: ProgramGate::new(target_sid, target_event_id),
        deadline,
        filter: TsServiceFilter::new(target_sid),
        aligner: TsAligner::new(),
    };

    stream::unfold(state, |mut state| async move {
        loop {
            if state.gate.is_ended() {
                return None;
            }
            if !state.gate.is_streaming() && Utc::now() >= state.deadline {
                debug!(
                    "[mirakurun program stream] gave up waiting for sid={} event_id={} to become present \
                     (deadline elapsed)",
                    state.gate.target_sid, state.gate.target_event_id
                );
                return None;
            }

            match state.rx.recv().await {
                Ok(data) => {
                    // Gate observation runs unconditionally (including on
                    // chunks yielded once already Streaming) so an
                    // Streaming -> Ended transition is caught mid-programme,
                    // not just while still Waiting. It reads the *unfiltered*
                    // chunk: EIT survives filtering, but the gate decides
                    // whether this stream exists at all and must not depend
                    // on the filter having warmed up.
                    state.collector.process_ts_chunk(&data, &mut state.gate);

                    // Feed the filter regardless of gate state (see
                    // `GatedStreamState::filter`).
                    let filtered = state
                        .aligner
                        .push(&data)
                        .map(|chunk| state.filter.filter(&chunk))
                        .unwrap_or_default();

                    if state.gate.is_streaming() && !filtered.is_empty() {
                        return Some((Ok(Bytes::from(filtered)), state));
                    }
                    // Still waiting (or just transitioned to Ended on this
                    // very chunk, in which case there is nothing left to
                    // deliver) -> withhold this chunk and loop for the next.
                    continue;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Same "log and continue" convention as
                    // `web/stream.rs::broadcast_to_body_stream` — losing a
                    // few chunks of a multiplex the gate hasn't opened for
                    // yet (or is mid-programme on) is harmless; the next
                    // successful recv picks up wherever the live edge is.
                    debug!("[mirakurun program stream] receiver lagged, skipped {} chunks", n);
                    state.aligner.on_gap();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // ProgramGate
    // ------------------------------------------------------------------

    #[test]
    fn waits_until_target_event_becomes_present() {
        let mut gate = ProgramGate::new(100, 5);
        assert!(!gate.is_streaming());

        // A different event is present -> keep waiting.
        gate.observe_eit_pf_actual(0, 100, 4);
        assert!(!gate.is_streaming());
        assert!(!gate.is_ended());

        // Target event becomes present -> streaming.
        gate.observe_eit_pf_actual(0, 100, 5);
        assert!(gate.is_streaming());
    }

    #[test]
    fn ends_when_a_different_event_becomes_present_after_streaming() {
        let mut gate = ProgramGate::new(100, 5);
        gate.observe_eit_pf_actual(0, 100, 5);
        assert!(gate.is_streaming());

        gate.observe_eit_pf_actual(0, 100, 6);
        assert!(gate.is_ended());
        assert!(!gate.is_streaming());
    }

    #[test]
    fn ignores_other_services_present_following() {
        let mut gate = ProgramGate::new(100, 5);
        // Present event 5 on an unrelated service must not open the gate.
        gate.observe_eit_pf_actual(0, 999, 5);
        assert!(!gate.is_streaming());
    }

    #[test]
    fn ignores_following_sections() {
        let mut gate = ProgramGate::new(100, 5);
        // section_number == 1 (following) carries no "present" information.
        gate.observe_eit_pf_actual(1, 100, 5);
        assert!(!gate.is_streaming());
    }

    #[test]
    fn state_is_stable_with_no_observations() {
        let mut gate = ProgramGate::new(100, 5);
        assert!(!gate.is_streaming());
        assert!(!gate.is_ended());
        // No `observe_*` calls at all -> state never changes on its own.
    }

    #[test]
    fn stays_ended_once_ended_even_if_target_reappears() {
        let mut gate = ProgramGate::new(100, 5);
        gate.observe_eit_pf_actual(0, 100, 5);
        gate.observe_eit_pf_actual(0, 100, 6);
        assert!(gate.is_ended());

        // Target event id somehow observed present again (e.g. EIT
        // inconsistency/rewind) must not resurrect the gate.
        gate.observe_eit_pf_actual(0, 100, 5);
        assert!(gate.is_ended());
        assert!(!gate.is_streaming());
    }

    #[test]
    fn other_table_ids_are_never_fed_to_observe_but_would_be_meaningless_if_they_were() {
        // This test documents the contract at the `EitPfCollector` level:
        // only table_id == EIT_PF_ACTUAL (0x4E) sections are ever passed to
        // `observe_eit_pf_actual`. `EitPfCollector::process_section` enforces
        // this before calling the gate at all, so 0x4F/schedule sections
        // never reach `ProgramGate` in the first place.
        assert_eq!(table_id::EIT_PF_OTHER, 0x4F);
        assert_eq!(table_id::EIT_PF_ACTUAL, 0x4E);
    }
}
