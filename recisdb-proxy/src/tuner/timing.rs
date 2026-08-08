//! Timing constants for the reader startup/stop sequence
//! (docs/TUNER_PIPELINE_REDESIGN.md P2a item 4).
//!
//! These are internal synchronization margins, not operator-facing tuning
//! knobs like [`crate::tuner::pool::TunerPoolConfig::set_channel_retry_timeout_ms`]
//! (which varies per BonDriver's network latency and lives in the DB-backed
//! `tuner_config` table so it can be tuned per deployment). Promoting these
//! to DB columns would need a migration plus a dashboard field for values
//! nobody should reasonably need to touch — each one is either derived from
//! another constant in this file (the reader loop's own poll interval) or a
//! fixed protocol-level margin, not something that varies by hardware/network
//! the way the retry knobs do. Kept as plain `const`s here instead.

use std::time::Duration;

/// After `SetChannel` succeeds, how long to wait before the read loop starts
/// pulling from `GetTsStream`. Newly-tuned BonDrivers (especially
/// network-backed or PCIe drivers) need a short settle time before their
/// internal buffer has anything in it; without this, the very first
/// `get_ts_stream` calls reliably return 0 bytes and get logged as spurious
/// "early startup" warnings.
pub(crate) const SET_CHANNEL_STABILIZATION_SLEEP_MS: u64 = 500;

/// Upper bound `stop_reader()` waits for the reader task to actually exit —
/// applied twice (once to acquire the `reader_handle` lock, once to join the
/// task itself). The read loop re-checks its stop flag every
/// `WAIT_TS_STREAM_POLL_MS` (100 ms), so a healthy blocking task reaches its
/// exit point within roughly 2x that; 1000 ms leaves a 5x margin for a slow
/// DLL callback caught mid-poll.
pub(crate) const STOP_READER_TIMEOUT_MS: u64 = 1000;

/// How long the reader loop's `wait_ts_stream` blocks per iteration before
/// re-checking the stop flag. Small enough that `stop_reader()` observes the
/// task exiting quickly (see `STOP_READER_TIMEOUT_MS`); not so small that it
/// busy-polls the DLL.
pub(crate) const WAIT_TS_STREAM_POLL_MS: u64 = 100;

/// Polling interval for `SharedTuner::wait_first_data`.
pub(crate) const WAIT_FIRST_DATA_POLL_MS: u64 = 50;

/// Sleep before `channel_resolve`'s single EALREADY retry, to give a
/// just-evicted idle reader's BonDriver `CloseTuner` time to actually
/// release the underlying device handle before the retry's `OpenTuner`.
pub(crate) const EALREADY_RETRY_SLEEP_MS: u64 = 300;

/// Safety margin added on top of `set_channel_retry_timeout_ms` when
/// computing how long a reader-start caller should wait for the `ready`
/// signal (docs/TUNER_PIPELINE_REDESIGN.md §2.1-1).
///
/// The blocking SetChannel-retry loop inside the reader itself runs for up
/// to `set_channel_retry_timeout_ms` before giving up. The ready-wait
/// timeout on the calling side must always be strictly longer than that
/// budget — otherwise the caller can time out and walk away (dropping the
/// `ready_rx` receiver, releasing the pool entry it owns) *before* the
/// reader has exhausted its own retries, leaving a reader that later
/// succeeds `SetChannel` with nobody left listening. This is exactly the
/// orphaned-reader bug §2.1-1 describes; see
/// [`crate::tuner::shared::SharedTuner::start_reader`] for the other half of
/// the fix (the reader checking its `ready_tx.send()` result).
///
/// 5 s covers the BonDriver open call itself (`BonDriverTuner::new`, which
/// runs *before* the retry loop and is not counted in
/// `set_channel_retry_timeout_ms` at all) plus general scheduling slack.
pub(crate) const READY_TIMEOUT_MARGIN_MS: u64 = 5_000;

/// How long a reader-start caller should wait for the `ready` signal before
/// giving up, given `set_channel_retry_timeout_ms` from the driver's
/// [`crate::tuner::pool::TunerPoolConfig`]. See [`READY_TIMEOUT_MARGIN_MS`].
pub(crate) fn reader_ready_timeout(set_channel_retry_timeout_ms: u64) -> Duration {
    Duration::from_millis(set_channel_retry_timeout_ms + READY_TIMEOUT_MARGIN_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Core invariant §2.1-1 depends on: whatever the configured retry
    /// budget, the computed ready-wait timeout must never be shorter than or
    /// equal to it.
    #[test]
    fn reader_ready_timeout_always_exceeds_set_channel_retry_budget() {
        for retry_timeout_ms in [0, 1, 10_000, 60_000] {
            let ready = reader_ready_timeout(retry_timeout_ms);
            assert!(
                ready > Duration::from_millis(retry_timeout_ms),
                "ready timeout {:?} must exceed retry budget {}ms",
                ready,
                retry_timeout_ms
            );
        }
    }

    #[test]
    fn reader_ready_timeout_matches_margin_plus_retry_budget() {
        assert_eq!(
            reader_ready_timeout(10_000),
            Duration::from_millis(10_000 + READY_TIMEOUT_MARGIN_MS)
        );
    }
}
