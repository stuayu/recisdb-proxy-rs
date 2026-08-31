//! TS packet quality analyzer.

use std::collections::HashMap;

use crate::tuner::ts_parser::{SYNC_BYTE, TS_PACKET_SIZE};

/// Maximum number of distinct PIDs tracked individually.
///
/// Real-world ARIB multiplexes rarely carry more than a few dozen PIDs, so
/// this is a generous headroom while still bounding memory for pathological
/// inputs (e.g. corrupted streams with garbage PIDs). Packets for PIDs
/// beyond this cap are still counted in the aggregate totals but are not
/// tracked individually (see [`TsPacketAnalyzer::overflow_packets`]).
pub const MAX_TRACKED_PIDS: usize = 256;

/// NULL packet PID (padding), excluded from continuity-counter checks.
const NULL_PID: u16 = 0x1FFF;

/// Quality counters for TS stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsStreamQuality {
    pub packets_total: u64,
    pub packets_dropped: u64,
    pub packets_scrambled: u64,
    pub packets_error: u64,
}

/// Delta counters for a single analyze call.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsStreamQualityDelta {
    pub packets_total: u64,
    pub packets_dropped: u64,
    pub packets_scrambled: u64,
    pub packets_error: u64,
}

/// Per-PID continuity/quality statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct PidStat {
    /// Total TS packets observed for this PID.
    pub packets: u64,
    /// Continuity-counter errors observed for this PID (unexpected CC jump,
    /// excluding allowed duplicates and flagged discontinuities).
    pub cc_errors: u64,
    /// Unix timestamp (ms) of the most recent CC error for this PID.
    /// `0` if no error has been observed yet.
    pub last_error_unix_ms: i64,
}

/// Internal per-PID tracking state (continuity counter + published stat).
#[derive(Debug, Clone, Copy, Default)]
struct PidTrack {
    /// Last observed continuity counter for this PID, if any packet with a
    /// payload has been seen yet.
    last_cc: Option<u8>,
    /// Whether the previous packet was already accepted as a duplicate
    /// (same CC repeated once). Only one consecutive duplicate is allowed;
    /// a second repeat is treated as an error.
    dup_used: bool,
    /// Published counters for this PID.
    stat: PidStat,
}

/// TS packet analyzer for continuity and error tracking.
#[derive(Debug, Default)]
pub struct TsPacketAnalyzer {
    pid_tracks: HashMap<u16, PidTrack>,
    quality: TsStreamQuality,
    /// Packets belonging to PIDs seen after the [`MAX_TRACKED_PIDS`] cap was
    /// reached. These are counted in `quality`/delta totals but not tracked
    /// per-PID (continuity is not checked for them).
    overflow_packets: u64,
}

impl TsPacketAnalyzer {
    /// Create a new analyzer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Analyze a chunk of TS data and return the delta counters.
    pub fn analyze(&mut self, data: &[u8]) -> TsStreamQualityDelta {
        let mut delta = TsStreamQualityDelta::default();

        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= data.len() {
            let packet = &data[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if packet[0] != SYNC_BYTE {
                continue;
            }

            let transport_error = (packet[1] & 0x80) != 0;
            let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
            let scrambling = (packet[3] >> 6) & 0x03;
            let adaptation_field_control = (packet[3] >> 4) & 0x03;
            let continuity_counter = packet[3] & 0x0F;

            delta.packets_total += 1;
            self.quality.packets_total += 1;

            if transport_error {
                delta.packets_error += 1;
                self.quality.packets_error += 1;
            }

            if scrambling != 0 {
                delta.packets_scrambled += 1;
                self.quality.packets_scrambled += 1;
            }

            // NULL (padding) packets are not part of any elementary stream
            // and are excluded from continuity-counter checks entirely.
            if pid == NULL_PID {
                continue;
            }

            // adaptation_field_control: bit0 = payload present, bit1 = adaptation field present.
            let has_payload = (adaptation_field_control & 0b01) != 0;
            let has_adaptation = (adaptation_field_control & 0b10) != 0;

            // discontinuity_indicator lives in the first adaptation-field byte
            // (packet[5]) when an adaptation field with nonzero length is present.
            let discontinuity =
                has_adaptation && packet.len() > 5 && packet[4] > 0 && (packet[5] & 0x80) != 0;

            let track = match self.pid_tracks.get_mut(&pid) {
                Some(t) => Some(t),
                None => {
                    if self.pid_tracks.len() < MAX_TRACKED_PIDS {
                        Some(self.pid_tracks.entry(pid).or_default())
                    } else {
                        None
                    }
                }
            };

            let Some(track) = track else {
                // Per-PID tracking capacity exhausted; count in aggregate only.
                self.overflow_packets += 1;
                continue;
            };

            track.stat.packets += 1;

            if !has_payload {
                // CC is not incremented on packets without a payload.
                continue;
            }

            if let Some(last_cc) = track.last_cc {
                let expected = (last_cc + 1) & 0x0F;
                if continuity_counter == expected {
                    track.dup_used = false;
                } else if continuity_counter == last_cc && !track.dup_used {
                    // A single repeated CC is a legitimate duplicate packet
                    // (used for retransmission robustness), not an error.
                    track.dup_used = true;
                } else if discontinuity {
                    // Encoder-signaled discontinuity (e.g. after a splice or
                    // source switch): resynchronize without counting an error.
                    track.dup_used = false;
                } else {
                    track.stat.cc_errors += 1;
                    track.stat.last_error_unix_ms = chrono::Utc::now().timestamp_millis();
                    track.dup_used = false;

                    delta.packets_dropped += 1;
                    self.quality.packets_dropped += 1;
                }
            }

            track.last_cc = Some(continuity_counter);
        }

        delta
    }

    /// Get a snapshot of current quality counters.
    pub fn snapshot(&self) -> TsStreamQuality {
        self.quality
    }

    /// Get the top-N PIDs by CC error count, descending. PIDs with zero
    /// errors are omitted. Intended for periodic (≈1s) reporting only —
    /// not called from the packet hot path.
    pub fn top_loss_pids(&self, n: usize) -> Vec<(u16, u64)> {
        let mut pids: Vec<(u16, u64)> = self
            .pid_tracks
            .iter()
            .filter(|(_, t)| t.stat.cc_errors > 0)
            .map(|(pid, t)| (*pid, t.stat.cc_errors))
            .collect();
        pids.sort_by(|a, b| b.1.cmp(&a.1));
        pids.truncate(n);
        pids
    }

    /// Get the published stat for a single PID (test/debug use).
    #[cfg(test)]
    fn pid_stat(&self, pid: u16) -> Option<PidStat> {
        self.pid_tracks.get(&pid).map(|t| t.stat)
    }

    /// Number of packets counted in the PID-tracking overflow bucket.
    pub fn overflow_packets(&self) -> u64 {
        self.overflow_packets
    }

    /// Mark a known stream discontinuity (e.g. a broadcast-buffer lag gap that
    /// is already accounted for elsewhere as a separate loss source).
    ///
    /// For every tracked PID this drops the continuity-counter baseline
    /// (`last_cc = None`, `dup_used = false`) so the next packet per PID
    /// re-establishes the CC baseline WITHOUT counting a drop, exactly as the
    /// per-packet `discontinuity_indicator` handling does. Accumulated per-PID
    /// stats (`packets`, `cc_errors`) and the aggregate `quality` totals are
    /// left intact — this is not a reset, only a resync barrier so the gap's
    /// unavoidable CC break is not double-counted as `packets_dropped`.
    pub fn mark_discontinuity(&mut self) {
        for track in self.pid_tracks.values_mut() {
            track.last_cc = None;
            track.dup_used = false;
        }
    }

    /// Reset counters.
    pub fn reset(&mut self) {
        self.quality = TsStreamQuality::default();
        self.pid_tracks.clear();
        self.overflow_packets = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 188-byte TS packet with the given header fields.
    /// `adaptation_field_control`: 0b01 = payload only, 0b11 = adaptation + payload,
    /// 0b10 = adaptation only (no payload), 0b00 = reserved (no payload).
    fn make_packet(
        pid: u16,
        adaptation_field_control: u8,
        cc: u8,
        discontinuity: bool,
    ) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F; // transport_error=0, payload_unit_start=0
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = (adaptation_field_control << 4) | (cc & 0x0F);

        if adaptation_field_control & 0b10 != 0 {
            // Has adaptation field: length + flags byte.
            pkt[4] = 1; // adaptation_field_length
            pkt[5] = if discontinuity { 0x80 } else { 0x00 };
        }

        pkt
    }

    fn concat_packets(packets: &[[u8; TS_PACKET_SIZE]]) -> Vec<u8> {
        packets.iter().flat_map(|p| p.iter().copied()).collect()
    }

    #[test]
    fn normal_increment_no_error() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 0, false),
            make_packet(pid, 0b01, 1, false),
            make_packet(pid, 0b01, 2, false),
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 0);
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.packets, 3);
        assert_eq!(stat.cc_errors, 0);
    }

    #[test]
    fn no_payload_packet_does_not_advance_cc() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 0, false),
            // adaptation-field-only packet (no payload): CC must not be
            // expected to increment across it.
            make_packet(pid, 0b10, 5, false),
            make_packet(pid, 0b01, 1, false),
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(
            delta.packets_dropped, 0,
            "no-payload packet must not trigger a CC error"
        );
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.packets, 3);
        assert_eq!(stat.cc_errors, 0);
    }

    #[test]
    fn single_duplicate_is_allowed() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 0, false),
            make_packet(pid, 0b01, 0, false), // duplicate, allowed
            make_packet(pid, 0b01, 1, false),
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 0);
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.cc_errors, 0);
    }

    #[test]
    fn second_consecutive_duplicate_is_an_error() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 0, false),
            make_packet(pid, 0b01, 0, false), // duplicate #1, allowed
            make_packet(pid, 0b01, 0, false), // duplicate #2, not allowed -> error
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 1);
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.cc_errors, 1);
    }

    #[test]
    fn discontinuity_indicator_suppresses_error() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 0, false),
            // Jump from 0 to 10 with discontinuity flagged: must not error.
            make_packet(pid, 0b11, 10, true),
            make_packet(pid, 0b01, 11, false),
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 0);
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.cc_errors, 0);
    }

    #[test]
    fn cc_wraparound_15_to_0_is_normal() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 15, false),
            make_packet(pid, 0b01, 0, false),
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 0);
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.cc_errors, 0);
    }

    #[test]
    fn unexpected_jump_without_discontinuity_is_an_error() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;
        let packets = [
            make_packet(pid, 0b01, 0, false),
            make_packet(pid, 0b01, 5, false), // jumped, not flagged
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 1);
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.cc_errors, 1);
        assert!(stat.last_error_unix_ms > 0);
    }

    #[test]
    fn mark_discontinuity_suppresses_resync_drop() {
        let mut analyzer = TsPacketAnalyzer::new();
        let pid = 0x0100;

        // In-order packets: no drops, establishes a CC baseline.
        let before = analyzer.analyze(&concat_packets(&[
            make_packet(pid, 0b01, 0, false),
            make_packet(pid, 0b01, 1, false),
            make_packet(pid, 0b01, 2, false),
        ]));
        assert_eq!(before.packets_dropped, 0);

        // A known gap happened (e.g. broadcast lag). Mark it.
        analyzer.mark_discontinuity();

        // Next packet's CC is arbitrarily far from the previous one. Without
        // the mark this would count as 1 drop; after the mark it must not.
        let after = analyzer.analyze(&concat_packets(&[make_packet(pid, 0b01, 9, false)]));
        assert_eq!(
            after.packets_dropped, 0,
            "resync after a known discontinuity must not count as loss"
        );

        // Accumulated per-PID stats survive the mark (not a reset).
        let stat = analyzer.pid_stat(pid).unwrap();
        assert_eq!(stat.packets, 4);
        assert_eq!(stat.cc_errors, 0);

        // A subsequent genuine mid-stream jump (no mark) still counts as a real drop.
        let real = analyzer.analyze(&concat_packets(&[make_packet(pid, 0b01, 5, false)]));
        assert_eq!(
            real.packets_dropped, 1,
            "a genuine CC jump without a mark must still count as loss"
        );
        assert_eq!(analyzer.pid_stat(pid).unwrap().cc_errors, 1);
    }

    #[test]
    fn null_pid_excluded_from_cc_tracking() {
        let mut analyzer = TsPacketAnalyzer::new();
        let packets = [
            make_packet(NULL_PID, 0b01, 0, false),
            make_packet(NULL_PID, 0b01, 7, false), // would be an error if tracked
            make_packet(NULL_PID, 0b01, 3, false),
        ];
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert_eq!(delta.packets_dropped, 0);
        assert!(analyzer.pid_stat(NULL_PID).is_none());
        assert_eq!(analyzer.overflow_packets(), 0);
    }

    #[test]
    fn pid_tracking_cap_overflows_to_aggregate() {
        let mut analyzer = TsPacketAnalyzer::new();
        // Fill up to the cap with distinct PIDs (one packet each).
        let mut all_packets = Vec::new();
        for i in 0..MAX_TRACKED_PIDS {
            all_packets.push(make_packet(i as u16, 0b01, 0, false));
        }
        // One more distinct PID beyond the cap.
        all_packets.push(make_packet(MAX_TRACKED_PIDS as u16, 0b01, 0, false));

        let delta = analyzer.analyze(&concat_packets(&all_packets));
        assert_eq!(delta.packets_total, (MAX_TRACKED_PIDS + 1) as u64);
        assert_eq!(analyzer.pid_tracks.len(), MAX_TRACKED_PIDS);
        assert_eq!(analyzer.overflow_packets(), 1);
        // The overflowing PID must not have been inserted into the map.
        assert!(analyzer.pid_stat(MAX_TRACKED_PIDS as u16).is_none());
    }

    #[test]
    fn top_loss_pids_sorted_descending() {
        let mut analyzer = TsPacketAnalyzer::new();
        // PID A: 1 error
        let mut packets = vec![
            make_packet(0x10, 0b01, 0, false),
            make_packet(0x10, 0b01, 5, false), // error
        ];
        // PID B: 2 errors
        packets.push(make_packet(0x20, 0b01, 0, false));
        packets.push(make_packet(0x20, 0b01, 5, false)); // error
        packets.push(make_packet(0x20, 0b01, 9, false)); // error (jump from 6 expected to 9... actually after error we resync to 5, expected 6)
        let delta = analyzer.analyze(&concat_packets(&packets));
        assert!(delta.packets_dropped >= 3);

        let top = analyzer.top_loss_pids(10);
        assert_eq!(top[0].0, 0x20);
        assert!(top[0].1 >= top.get(1).map(|(_, c)| *c).unwrap_or(0));
    }
}
