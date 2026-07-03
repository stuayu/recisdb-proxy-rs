//! Fixed-duration prefill / jitter buffer (STREAMING_DESIGN.md §4, P3).
//!
//! Sits between the per-session TS alignment step and the class-specific
//! send policy (`send_ts_frame` in `session.rs`). While `Filling`, wire
//! frames are queued (not sent) until `target_bytes` worth has accumulated;
//! the whole queue is then released at once and the buffer switches to
//! `Passthrough`, where every subsequent frame is handed straight through.
//!
//! This is deliberately a free-standing state machine (no `Session`
//! dependency) so it can be unit tested without spinning up sessions/tokio.

use std::collections::VecDeque;

use bytes::Bytes;
use log::warn;

use recisdb_protocol::BandType;

/// STREAMING_DESIGN.md §4.2: per-band default bitrate assumption (bits per
/// second), used to size the prefill buffer when no better estimate is
/// available.
///
/// NOTE: this is a static default, not a measured rate. `SessionRegistry`
/// already tracks a live `current_bitrate_mbps` per session (updated once a
/// second in `send_ts_data`), which would be a natural future input for
/// dynamic sizing. STREAMING_DESIGN.md §4.2 explicitly calls dynamic
/// bitrate-based correction out as future work, not a P3 requirement, so it
/// is intentionally not wired in here.
pub fn default_bitrate_bps(band: Option<BandType>) -> u64 {
    match band {
        Some(BandType::Terrestrial) => 18_000_000,
        Some(BandType::BS) => 24_000_000,
        Some(BandType::CS) => 24_000_000,
        Some(BandType::FourK) => 33_000_000,
        // Other/CATV/SKY/unknown: STREAMING_DESIGN.md §4.2 "不明 18Mbps".
        _ => 18_000_000,
    }
}

/// STREAMING_DESIGN.md §4.2:
/// `target_bytes = bitrate_bps / 8 * prefill_ms / 1000 * safety_factor`.
///
/// Returns `0` (bypass) when `prefill_ms` or `bitrate_bps` is `0`.
pub fn prefill_target_bytes(bitrate_bps: u64, prefill_ms: u64, safety_factor: f64) -> usize {
    if prefill_ms == 0 || bitrate_bps == 0 {
        return 0;
    }
    let bytes = (bitrate_bps as f64 / 8.0) * (prefill_ms as f64 / 1000.0) * safety_factor.max(0.0);
    if !bytes.is_finite() || bytes <= 0.0 {
        0
    } else {
        bytes.round() as usize
    }
}

/// Internal state of a [`PrefillBuffer`].
enum PrefillState {
    /// Accumulating frames until `queued_bytes` reaches `target_bytes`.
    Filling {
        queued: VecDeque<Bytes>,
        queued_bytes: usize,
        target_bytes: usize,
    },
    /// Target reached (or bypass): frames are handed straight through.
    Passthrough,
}

/// Fixed-duration prefill / jitter buffer state machine
/// (STREAMING_DESIGN.md §4.3).
pub struct PrefillBuffer {
    state: PrefillState,
}

impl PrefillBuffer {
    /// Creates a buffer that starts in `Passthrough` (bypass). Sessions call
    /// [`PrefillBuffer::reset`] explicitly on `StartStream` / mid-stream
    /// channel switch (STREAMING_DESIGN.md §4.3), so there is no meaningful
    /// "filling" state before a stream has actually started.
    pub fn new() -> Self {
        Self {
            state: PrefillState::Passthrough,
        }
    }

    /// Whether the buffer is currently accumulating (not yet released).
    pub fn is_filling(&self) -> bool {
        matches!(self.state, PrefillState::Filling { .. })
    }

    /// Push one wire frame.
    ///
    /// - While filling: queues the frame and returns `None`, unless this
    ///   push reaches (or exceeds) `target_bytes`, in which case the entire
    ///   queue (this frame included) is returned and the buffer switches to
    ///   `Passthrough`.
    /// - While in `Passthrough`: returns `Some(vec![frame])` immediately.
    ///
    /// Memory safety valve: if a single push causes `queued_bytes` to jump
    /// to at least 2x `target_bytes`, a warning is logged in addition to the
    /// normal flush. This indicates the configured target is too small
    /// relative to the incoming chunk size (e.g. a misconfigured near-zero
    /// prefill setting), not an ongoing leak — the target-reached check
    /// below fires on every push, so queued bytes can never silently grow
    /// past `target_bytes` across multiple pushes.
    pub fn push(&mut self, frame: Bytes) -> Option<Vec<Bytes>> {
        let PrefillState::Filling {
            queued,
            queued_bytes,
            target_bytes,
        } = &mut self.state
        else {
            return Some(vec![frame]);
        };

        let frame_len = frame.len();
        queued.push_back(frame);
        *queued_bytes += frame_len;

        if *queued_bytes < *target_bytes {
            return None;
        }

        if *queued_bytes >= target_bytes.saturating_mul(2) {
            warn!(
                "PrefillBuffer: queued {} bytes in a single push, >= 2x target ({} bytes); \
                 force-flushing. Check prefill_*_ms / jitter_safety_factor vs. chunk size.",
                *queued_bytes, *target_bytes
            );
        }

        let drained: Vec<Bytes> = queued.drain(..).collect();
        self.state = PrefillState::Passthrough;
        Some(drained)
    }

    /// Start (or restart) filling toward `target_bytes`. `target_bytes == 0`
    /// bypasses prefill entirely (`Passthrough`) — STREAMING_DESIGN.md §4.3:
    /// "prefill_ms = 0 なら完全バイパス".
    pub fn reset(&mut self, target_bytes: usize) {
        self.state = if target_bytes == 0 {
            PrefillState::Passthrough
        } else {
            PrefillState::Filling {
                queued: VecDeque::new(),
                queued_bytes: 0,
                target_bytes,
            }
        };
    }

    /// Discard any queued frames (STREAMING_DESIGN.md §4.3: `PurgeStream`).
    /// Does not change whether the buffer is filling or passthrough.
    pub fn clear(&mut self) {
        if let PrefillState::Filling {
            queued,
            queued_bytes,
            ..
        } = &mut self.state
        {
            queued.clear();
            *queued_bytes = 0;
        }
    }
}

impl Default for PrefillBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize) -> Bytes {
        Bytes::from(vec![0u8; len])
    }

    #[test]
    fn fills_then_flushes_then_passes_through() {
        let mut buf = PrefillBuffer::new();
        buf.reset(300);

        assert!(buf.push(frame(100)).is_none());
        assert!(buf.is_filling());
        assert!(buf.push(frame(100)).is_none());

        // Third frame reaches the 300-byte target: all three flush at once.
        let flushed = buf.push(frame(100)).expect("target reached");
        assert_eq!(flushed.len(), 3);
        assert!(!buf.is_filling());

        // Subsequent frames pass straight through, one at a time.
        let passed = buf.push(frame(50)).expect("passthrough");
        assert_eq!(passed, vec![frame(50)]);
    }

    #[test]
    fn reset_returns_to_filling() {
        let mut buf = PrefillBuffer::new();
        buf.reset(10);
        let _ = buf.push(frame(20)); // flush -> Passthrough
        assert!(!buf.is_filling());

        buf.reset(500);
        assert!(buf.is_filling());
        assert!(buf.push(frame(10)).is_none());
    }

    #[test]
    fn clear_discards_queue_without_leaving_filling_state() {
        let mut buf = PrefillBuffer::new();
        buf.reset(1000);
        assert!(buf.push(frame(900)).is_none());

        buf.clear();
        assert!(buf.is_filling());

        // Queue was actually discarded: another 900 bytes should not reach
        // the 1000-byte target on its own.
        assert!(buf.push(frame(900)).is_none());
        // But together with a further push it should.
        let flushed = buf.push(frame(200)).expect("target reached after clear");
        assert_eq!(flushed.len(), 2);
    }

    #[test]
    fn clear_on_passthrough_stays_passthrough() {
        let mut buf = PrefillBuffer::new(); // Passthrough by default
        assert!(!buf.is_filling());
        buf.clear();
        assert!(!buf.is_filling());
        assert_eq!(buf.push(frame(10)), Some(vec![frame(10)]));
    }

    #[test]
    fn zero_target_bypasses_immediately() {
        let mut buf = PrefillBuffer::new();
        buf.reset(0);
        assert!(!buf.is_filling());
        let out = buf.push(frame(123)).expect("bypass");
        assert_eq!(out, vec![frame(123)]);
    }

    #[test]
    fn oversized_single_push_force_flushes_past_2x_target() {
        let mut buf = PrefillBuffer::new();
        buf.reset(100);
        // Single frame far exceeds 2x the target in one push.
        let flushed = buf.push(frame(500)).expect("force-flush");
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].len(), 500);
        assert!(!buf.is_filling());
    }

    #[test]
    fn default_bitrate_by_band() {
        assert_eq!(default_bitrate_bps(Some(BandType::Terrestrial)), 18_000_000);
        assert_eq!(default_bitrate_bps(Some(BandType::BS)), 24_000_000);
        assert_eq!(default_bitrate_bps(Some(BandType::CS)), 24_000_000);
        assert_eq!(default_bitrate_bps(Some(BandType::FourK)), 33_000_000);
        assert_eq!(default_bitrate_bps(Some(BandType::Other)), 18_000_000);
        assert_eq!(default_bitrate_bps(None), 18_000_000);
    }

    #[test]
    fn target_bytes_boundaries() {
        assert_eq!(prefill_target_bytes(18_000_000, 0, 1.5), 0);
        assert_eq!(prefill_target_bytes(18_000_000, 1000, 1.0), 2_250_000);
        assert_eq!(prefill_target_bytes(18_000_000, 1000, 1.5), 3_375_000);
        assert_eq!(prefill_target_bytes(24_000_000, 2000, 1.5), 9_000_000);
        assert_eq!(prefill_target_bytes(0, 1000, 1.5), 0);
    }
}
