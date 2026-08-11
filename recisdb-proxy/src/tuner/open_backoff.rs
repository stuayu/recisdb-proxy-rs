//! Per-DLL cooldown and log-suppression for repeated `OpenTuner` failures.
//!
//! # Why this exists
//!
//! Production logs on 2026-08-06〜07 showed
//! `OpenTuner failed - tuner may be in use, not present, or hardware error`
//! firing 2,000〜4,300 times a minute for 20 hours straight — 467,039 ERROR
//! lines (131MB) in one day. The session IDs in that window were sequential
//! 80ms apart (session 669 to 116671, ~116k sessions/day): a client was
//! reconnecting, failing to select a channel, and reconnecting again in an
//! unbroken loop, hammering the same BonDriver's `OpenTuner` roughly 13
//! times a second with nothing on the server side to slow it down.
//!
//! [`OpenFailureBackoff`] tracks consecutive open failures per `tuner_path`
//! (the DLL path, since that is the resource actually being hammered) and:
//!
//! - after a few failures in a row, makes [`crate::tuner::acquire::acquire`]
//!   refuse new attempts against that path for a short, exponentially
//!   growing cooldown instead of touching the DLL again, so a runaway
//!   reconnect loop stops reaching `OpenTuner` at all;
//! - independently of the cooldown, caps how often the failure gets logged,
//!   so a burst that *is* still within the no-cooldown grace period (or that
//!   arrives faster than the cooldown itself, from many concurrent sessions)
//!   cannot regenerate the same log flood by itself.
//!
//! Both concerns are per-`tuner_path`: a flaky DLL should not cool down or
//! silence logging for every other driver in the pool.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Consecutive failures before a cooldown kicks in at all.
///
/// A single "tuner may be in use" is routine (someone else has it open right
/// now) and must not be delayed — only a *streak* indicates the DLL itself,
/// or the device behind it, is actually unhealthy.
const FAILURE_THRESHOLD: u32 = 3;

/// Cap on the exponential backoff, so a permanently broken driver still
/// gets retried at a human-visible cadence rather than being abandoned.
const MAX_COOLDOWN: Duration = Duration::from_secs(30);

/// How often a still-failing path is allowed to log, and the window over
/// which suppressed occurrences between two log lines are counted.
const LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Per-path failure bookkeeping.
struct DriverFailures {
    consecutive: u32,
    cooldown_until: Option<Instant>,
    /// Failures observed since the last time this path logged.
    suppressed: u32,
    last_logged: Option<Instant>,
}

impl DriverFailures {
    fn new() -> Self {
        Self { consecutive: 0, cooldown_until: None, suppressed: 0, last_logged: None }
    }
}

/// What [`OpenFailureBackoff::record_failure`] learned from one more failure,
/// for the caller to act on (start a cooldown, decide whether to log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureReport {
    /// Consecutive failures on this path, including the one just recorded.
    pub consecutive: u32,
    /// Cooldown now in effect (zero if still under [`FAILURE_THRESHOLD`]).
    pub cooldown: Duration,
    /// Whether the caller should emit a log line for this failure. False
    /// while a previous log line for this path is still within
    /// [`LOG_INTERVAL`].
    pub should_log: bool,
    /// Failures suppressed (not logged) since the last time this path did
    /// log — reported once, on the failure that finally logs again, then
    /// reset to zero.
    pub suppressed: u32,
}

/// Tracks consecutive `OpenTuner` failures per DLL path to back off and
/// throttle logging (module doc above has the production incident this
/// addresses).
///
/// `std::sync::Mutex`, not `tokio::sync::Mutex`: every access here is a
/// short, non-blocking HashMap lookup/update with no `.await` in between, so
/// there is nothing gained by the async variant and this type stays usable
/// from non-async call sites (tests) without a runtime.
pub(crate) struct OpenFailureBackoff {
    state: Mutex<HashMap<String, DriverFailures>>,
}

impl OpenFailureBackoff {
    pub(crate) fn new() -> Self {
        Self { state: Mutex::new(HashMap::new()) }
    }

    /// ロックを取る。毒(poisoning)は無視して中身をそのまま使う: ここに入って
    /// いるのは失敗回数とログ時刻という診断用の数字だけで、途中でpanicして
    /// 半端になっていても選局の可否を誤らせない。逆にここで `unwrap()` して
    /// panicすると、以降すべての選局が道連れになる。
    fn lock_state(&self) -> std::sync::MutexGuard<'_, HashMap<String, DriverFailures>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remaining cooldown for `tuner_path`, or `None` if it is not
    /// currently cooling down.
    pub(crate) fn cooldown_remaining(&self, tuner_path: &str) -> Option<Duration> {
        self.cooldown_remaining_at(tuner_path, Instant::now())
    }

    pub(crate) fn cooldown_remaining_at(&self, tuner_path: &str, now: Instant) -> Option<Duration> {
        let state = self.lock_state();
        let entry = state.get(tuner_path)?;
        let until = entry.cooldown_until?;
        if until > now {
            Some(until - now)
        } else {
            None
        }
    }

    /// Record one more open failure and return what the caller should do
    /// about it.
    pub(crate) fn record_failure(&self, tuner_path: &str) -> FailureReport {
        self.record_failure_at(tuner_path, Instant::now())
    }

    pub(crate) fn record_failure_at(&self, tuner_path: &str, now: Instant) -> FailureReport {
        let mut state = self.lock_state();
        let entry = state.entry(tuner_path.to_string()).or_insert_with(DriverFailures::new);

        entry.consecutive += 1;

        let cooldown = if entry.consecutive >= FAILURE_THRESHOLD {
            let shift = entry.consecutive - FAILURE_THRESHOLD;
            let scaled = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
            Duration::from_secs(scaled).min(MAX_COOLDOWN)
        } else {
            Duration::ZERO
        };
        entry.cooldown_until = if cooldown.is_zero() { None } else { Some(now + cooldown) };

        let should_log = match entry.last_logged {
            None => true,
            Some(last) => now.duration_since(last) >= LOG_INTERVAL,
        };
        let suppressed = if should_log {
            let reported = entry.suppressed;
            entry.suppressed = 0;
            entry.last_logged = Some(now);
            reported
        } else {
            entry.suppressed += 1;
            entry.suppressed
        };

        FailureReport { consecutive: entry.consecutive, cooldown, should_log, suppressed }
    }

    /// Clear all failure state for `tuner_path` — a successful open means
    /// whatever was wrong is over, and the next failure (if any) should be
    /// judged fresh rather than continuing an old streak.
    pub(crate) fn record_success(&self, tuner_path: &str) {
        self.record_success_at(tuner_path);
    }

    pub(crate) fn record_success_at(&self, tuner_path: &str) {
        let mut state = self.lock_state();
        state.remove(tuner_path);
    }

    /// Consecutive failures currently on record for `tuner_path` (0 if
    /// none), for callers that already know a cooldown is active (via
    /// [`Self::cooldown_remaining`]) and need the count for diagnostics —
    /// e.g. [`crate::tuner::acquire::AcquireError::OpenCooldown`].
    pub(crate) fn consecutive_failures(&self, tuner_path: &str) -> u32 {
        self.lock_state().get(tuner_path).map(|e| e.consecutive).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/dev/test-tuner";

    #[test]
    fn no_cooldown_below_threshold() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();

        let r1 = backoff.record_failure_at(PATH, t0);
        assert_eq!(r1.consecutive, 1);
        assert_eq!(r1.cooldown, Duration::ZERO);
        assert_eq!(backoff.cooldown_remaining_at(PATH, t0), None);

        let r2 = backoff.record_failure_at(PATH, t0);
        assert_eq!(r2.consecutive, 2);
        assert_eq!(r2.cooldown, Duration::ZERO);
        assert_eq!(backoff.cooldown_remaining_at(PATH, t0), None);
    }

    #[test]
    fn exponential_backoff_capped_at_30s() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();

        // 3rd failure: 1s.
        for _ in 0..2 {
            backoff.record_failure_at(PATH, t0);
        }
        let r3 = backoff.record_failure_at(PATH, t0);
        assert_eq!(r3.consecutive, 3);
        assert_eq!(r3.cooldown, Duration::from_secs(1));

        // 4th: 2s.
        let r4 = backoff.record_failure_at(PATH, t0);
        assert_eq!(r4.cooldown, Duration::from_secs(2));

        // 5th: 4s.
        let r5 = backoff.record_failure_at(PATH, t0);
        assert_eq!(r5.cooldown, Duration::from_secs(4));

        // Keep failing until well past the cap.
        let mut last = r5.cooldown;
        for _ in 0..10 {
            let r = backoff.record_failure_at(PATH, t0);
            last = r.cooldown;
        }
        assert_eq!(last, MAX_COOLDOWN);
    }

    #[test]
    fn cooldown_remaining_reflects_elapsed_time() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();

        for _ in 0..3 {
            backoff.record_failure_at(PATH, t0);
        }
        // 3 consecutive failures -> 1s cooldown.
        let remaining = backoff.cooldown_remaining_at(PATH, t0);
        assert_eq!(remaining, Some(Duration::from_secs(1)));

        let mid = t0 + Duration::from_millis(500);
        assert_eq!(backoff.cooldown_remaining_at(PATH, mid), Some(Duration::from_millis(500)));

        let after = t0 + Duration::from_secs(2);
        assert_eq!(backoff.cooldown_remaining_at(PATH, after), None);
    }

    #[test]
    fn success_fully_resets_state() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();

        for _ in 0..5 {
            backoff.record_failure_at(PATH, t0);
        }
        assert!(backoff.cooldown_remaining_at(PATH, t0).is_some());

        backoff.record_success_at(PATH);
        assert_eq!(backoff.cooldown_remaining_at(PATH, t0), None);
        assert_eq!(backoff.consecutive_failures(PATH), 0);

        // Next failure starts over at 1, not continuing the old streak.
        let r = backoff.record_failure_at(PATH, t0);
        assert_eq!(r.consecutive, 1);
        assert_eq!(r.cooldown, Duration::ZERO);
        // Log suppression also resets independently: this "first" failure
        // logs again immediately.
        assert!(r.should_log);
    }

    #[test]
    fn log_suppression_window() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();

        let r1 = backoff.record_failure_at(PATH, t0);
        assert!(r1.should_log);
        assert_eq!(r1.suppressed, 0);

        let r2 = backoff.record_failure_at(PATH, t0 + Duration::from_secs(1));
        assert!(!r2.should_log);
        assert_eq!(r2.suppressed, 1);

        let r3 = backoff.record_failure_at(PATH, t0 + Duration::from_secs(2));
        assert!(!r3.should_log);
        assert_eq!(r3.suppressed, 2);

        // Just under the interval: still suppressed.
        let r4 = backoff.record_failure_at(PATH, t0 + Duration::from_secs(59));
        assert!(!r4.should_log);
        assert_eq!(r4.suppressed, 3);

        // At/after the interval: logs again, reporting what was suppressed,
        // then resets the suppressed counter.
        let r5 = backoff.record_failure_at(PATH, t0 + Duration::from_secs(61));
        assert!(r5.should_log);
        assert_eq!(r5.suppressed, 3);

        let r6 = backoff.record_failure_at(PATH, t0 + Duration::from_secs(62));
        assert!(!r6.should_log);
        assert_eq!(r6.suppressed, 1);
    }

    #[test]
    fn independent_paths_do_not_affect_each_other() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        let other = "/dev/other-tuner";

        for _ in 0..5 {
            backoff.record_failure_at(PATH, t0);
        }
        assert!(backoff.cooldown_remaining_at(PATH, t0).is_some());
        assert_eq!(backoff.cooldown_remaining_at(other, t0), None);

        let r = backoff.record_failure_at(other, t0);
        assert_eq!(r.consecutive, 1);
        assert!(r.should_log);
    }
}
