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

/// Failure threshold once a path is [`BreakerState::Degraded`]. A driver that
/// is already answering far too slowly gets less rope before it is cut off.
const DEGRADED_FAILURE_THRESHOLD: u32 = 2;

/// An open that succeeds but takes longer than this is a *soft* failure. The
/// hard deadline is the caller's own timeout; this is the "works, but nobody
/// wants to wait for it" line, and repeatedly crossing it is what moves a path
/// to [`BreakerState::Degraded`] (`docs/DISTRIBUTED_TUNER_FABRIC.md` §9).
const SLOW_OPEN_MS: u64 = 5_000;

/// Consecutive slow opens before a path is considered degraded.
const SLOW_OPEN_THRESHOLD: u32 = 3;

/// How long a half-open trial may be outstanding before it is assumed lost.
///
/// The caller that was admitted may never report back — it could fail to get a
/// slot permit, or its task could be dropped. Without this, one lost trial
/// would keep the path closed forever.
const HALF_OPEN_TRIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the exponential backoff, so a permanently broken driver still
/// gets retried at a human-visible cadence rather than being abandoned.
const MAX_COOLDOWN: Duration = Duration::from_secs(30);

/// How often a still-failing path is allowed to log, and the window over
/// which suppressed occurrences between two log lines are counted.
const LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Circuit state of one DLL path.
///
/// ```text
/// Healthy ──repeated slow opens──▶ Degraded
///    │                                │
///    └────repeated failures───────────┴──▶ Open
///                                          │ cooldown elapsed
///                                          ▼
///                                       HalfOpen ──success──▶ Healthy
///                                          │ failure
///                                          └──────────────▶ Open
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreakerState {
    /// Normal. Every request is admitted.
    Healthy,
    /// Answering, but too slowly. Still admitted — a slow tuner beats no
    /// tuner — but it trips to `Open` after fewer failures, and
    /// `tuner::policy` already ranks it below its healthy siblings through
    /// the driver quality score.
    Degraded,
    /// Refusing requests until the cooldown elapses.
    Open,
    /// Cooldown elapsed; exactly one trial request is admitted to find out
    /// whether the driver recovered. Everyone else keeps waiting, so a queue
    /// of clients does not all hit a just-maybe-recovered DLL at once.
    HalfOpen,
}

/// What a caller may do with a path right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Proceed normally.
    Allow,
    /// Proceed as the single half-open trial. The caller **must** report the
    /// outcome via `record_success*`/`record_failure`, or the trial is only
    /// released after [`HALF_OPEN_TRIAL_TIMEOUT`].
    Trial,
    /// Do not touch this driver yet.
    Reject { retry_in: Duration },
}

/// Per-path failure bookkeeping.
struct DriverFailures {
    consecutive: u32,
    cooldown_until: Option<Instant>,
    /// Failures observed since the last time this path logged.
    suppressed: u32,
    last_logged: Option<Instant>,
    /// Consecutive successful-but-slow opens.
    consecutive_slow: u32,
    degraded: bool,
    /// When the outstanding half-open trial was admitted, if any.
    trial_started: Option<Instant>,
}

impl DriverFailures {
    fn new() -> Self {
        Self {
            consecutive: 0,
            cooldown_until: None,
            suppressed: 0,
            last_logged: None,
            consecutive_slow: 0,
            degraded: false,
            trial_started: None,
        }
    }

    fn failure_threshold(&self) -> u32 {
        if self.degraded {
            DEGRADED_FAILURE_THRESHOLD
        } else {
            FAILURE_THRESHOLD
        }
    }

    fn state_at(&self, now: Instant) -> BreakerState {
        match self.cooldown_until {
            Some(until) if until > now => BreakerState::Open,
            Some(_) => BreakerState::HalfOpen,
            None if self.degraded => BreakerState::Degraded,
            None => BreakerState::Healthy,
        }
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

    /// Circuit state of `tuner_path` right now, for diagnostics and the
    /// dashboard. Does not admit anything — use [`Self::try_admit`].
    pub(crate) fn state(&self, tuner_path: &str) -> BreakerState {
        self.state_at(tuner_path, Instant::now())
    }

    pub(crate) fn state_at(&self, tuner_path: &str, now: Instant) -> BreakerState {
        self.lock_state()
            .get(tuner_path)
            .map(|e| e.state_at(now))
            .unwrap_or(BreakerState::Healthy)
    }

    /// Decide whether a caller may touch `tuner_path` now.
    ///
    /// This *takes* the half-open trial slot when it returns [`Admission::Trial`],
    /// so it must be called once per attempt, not speculatively.
    pub(crate) fn try_admit(&self, tuner_path: &str) -> Admission {
        self.try_admit_at(tuner_path, Instant::now())
    }

    pub(crate) fn try_admit_at(&self, tuner_path: &str, now: Instant) -> Admission {
        let mut state = self.lock_state();
        let Some(entry) = state.get_mut(tuner_path) else {
            return Admission::Allow;
        };
        match entry.state_at(now) {
            BreakerState::Healthy | BreakerState::Degraded => Admission::Allow,
            BreakerState::Open => Admission::Reject {
                retry_in: entry
                    .cooldown_until
                    .map(|until| until.saturating_duration_since(now))
                    .unwrap_or_default(),
            },
            BreakerState::HalfOpen => {
                let trial_free = match entry.trial_started {
                    None => true,
                    // A trial nobody ever reported back on must not keep the
                    // path closed forever.
                    Some(started) => now.duration_since(started) >= HALF_OPEN_TRIAL_TIMEOUT,
                };
                if trial_free {
                    entry.trial_started = Some(now);
                    Admission::Trial
                } else {
                    Admission::Reject {
                        retry_in: HALF_OPEN_TRIAL_TIMEOUT,
                    }
                }
            }
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
        // A failing open is not a slow one; the slow streak restarts.
        entry.consecutive_slow = 0;
        // Whatever the half-open trial was, it has now reported back.
        entry.trial_started = None;

        let threshold = entry.failure_threshold();
        let cooldown = if entry.consecutive >= threshold {
            let shift = entry.consecutive - threshold;
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

    /// A successful open, with how long it took.
    ///
    /// Fast enough closes the breaker completely. Repeatedly slow keeps the
    /// path admitted — a slow tuner still beats no tuner — but marks it
    /// [`BreakerState::Degraded`], which halves the rope it gets before the
    /// next failure streak opens the circuit.
    pub(crate) fn record_success_with_latency(&self, tuner_path: &str, open_ms: u64) {
        let mut state = self.lock_state();
        if open_ms < SLOW_OPEN_MS {
            state.remove(tuner_path);
            return;
        }
        let entry = state
            .entry(tuner_path.to_string())
            .or_insert_with(DriverFailures::new);
        entry.consecutive = 0;
        entry.cooldown_until = None;
        entry.trial_started = None;
        entry.consecutive_slow = entry.consecutive_slow.saturating_add(1);
        entry.degraded = entry.consecutive_slow >= SLOW_OPEN_THRESHOLD;
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
mod breaker_tests {
    use super::*;

    const PATH: &str = "/dev/test-tuner";

    fn open_the_circuit(backoff: &OpenFailureBackoff, t0: Instant) -> Duration {
        let mut cooldown = Duration::ZERO;
        for _ in 0..FAILURE_THRESHOLD {
            cooldown = backoff.record_failure_at(PATH, t0).cooldown;
        }
        assert!(!cooldown.is_zero(), "the circuit should be open by now");
        cooldown
    }

    #[test]
    fn an_unknown_path_is_healthy_and_always_admitted() {
        let backoff = OpenFailureBackoff::new();
        assert_eq!(backoff.state(PATH), BreakerState::Healthy);
        assert_eq!(backoff.try_admit(PATH), Admission::Allow);
    }

    #[test]
    fn an_open_circuit_rejects_until_the_cooldown_elapses() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        let cooldown = open_the_circuit(&backoff, t0);

        assert_eq!(backoff.state_at(PATH, t0), BreakerState::Open);
        assert!(matches!(
            backoff.try_admit_at(PATH, t0),
            Admission::Reject { .. }
        ));

        // Still open just before the cooldown expires.
        let almost = t0 + cooldown - Duration::from_millis(1);
        assert!(matches!(
            backoff.try_admit_at(PATH, almost),
            Admission::Reject { .. }
        ));
    }

    /// The point of half-open: a queue of waiting clients must not all hit a
    /// just-maybe-recovered DLL at once.
    #[test]
    fn only_one_caller_is_admitted_when_the_cooldown_elapses() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        let cooldown = open_the_circuit(&backoff, t0);
        let after = t0 + cooldown + Duration::from_millis(1);

        assert_eq!(backoff.state_at(PATH, after), BreakerState::HalfOpen);
        assert_eq!(backoff.try_admit_at(PATH, after), Admission::Trial);
        for _ in 0..5 {
            assert!(
                matches!(backoff.try_admit_at(PATH, after), Admission::Reject { .. }),
                "a second caller must wait for the trial's outcome"
            );
        }
    }

    #[test]
    fn a_successful_trial_closes_the_circuit() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        let cooldown = open_the_circuit(&backoff, t0);
        let after = t0 + cooldown + Duration::from_millis(1);
        assert_eq!(backoff.try_admit_at(PATH, after), Admission::Trial);

        backoff.record_success_with_latency(PATH, 200);
        assert_eq!(backoff.state_at(PATH, after), BreakerState::Healthy);
        assert_eq!(backoff.try_admit_at(PATH, after), Admission::Allow);
    }

    #[test]
    fn a_failed_trial_reopens_the_circuit_for_longer() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        let first_cooldown = open_the_circuit(&backoff, t0);
        let after = t0 + first_cooldown + Duration::from_millis(1);
        assert_eq!(backoff.try_admit_at(PATH, after), Admission::Trial);

        let second_cooldown = backoff.record_failure_at(PATH, after).cooldown;
        assert!(
            second_cooldown > first_cooldown,
            "backoff must grow: {second_cooldown:?} vs {first_cooldown:?}"
        );
        assert_eq!(backoff.state_at(PATH, after), BreakerState::Open);
        assert!(matches!(
            backoff.try_admit_at(PATH, after),
            Admission::Reject { .. }
        ));
    }

    /// A caller that never reports back must not close the path forever.
    #[test]
    fn a_lost_trial_is_released_after_the_timeout() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        let cooldown = open_the_circuit(&backoff, t0);
        let after = t0 + cooldown + Duration::from_millis(1);
        assert_eq!(backoff.try_admit_at(PATH, after), Admission::Trial);

        let much_later = after + HALF_OPEN_TRIAL_TIMEOUT;
        assert_eq!(backoff.try_admit_at(PATH, much_later), Admission::Trial);
    }

    /// "Works, but nobody wants to wait for it" has to be visible. It stays
    /// admitted — a slow tuner beats no tuner — but gets less rope.
    #[test]
    fn repeated_slow_opens_degrade_a_path_without_blocking_it() {
        let backoff = OpenFailureBackoff::new();
        for _ in 0..SLOW_OPEN_THRESHOLD {
            backoff.record_success_with_latency(PATH, SLOW_OPEN_MS + 1);
        }
        assert_eq!(backoff.state(PATH), BreakerState::Degraded);
        assert_eq!(
            backoff.try_admit(PATH),
            Admission::Allow,
            "degraded is still usable"
        );
    }

    #[test]
    fn a_degraded_path_opens_after_fewer_failures() {
        let backoff = OpenFailureBackoff::new();
        let t0 = Instant::now();
        for _ in 0..SLOW_OPEN_THRESHOLD {
            backoff.record_success_with_latency(PATH, SLOW_OPEN_MS + 1);
        }
        assert_eq!(backoff.state(PATH), BreakerState::Degraded);

        for _ in 0..DEGRADED_FAILURE_THRESHOLD {
            backoff.record_failure_at(PATH, t0);
        }
        assert_eq!(
            backoff.state_at(PATH, t0),
            BreakerState::Open,
            "a degraded path must trip before a healthy one would"
        );
        // A healthy path survives the same number of failures.
        let healthy = OpenFailureBackoff::new();
        for _ in 0..DEGRADED_FAILURE_THRESHOLD {
            healthy.record_failure_at(PATH, t0);
        }
        assert_eq!(healthy.state_at(PATH, t0), BreakerState::Healthy);
    }

    #[test]
    fn a_fast_open_clears_a_previous_slow_streak() {
        let backoff = OpenFailureBackoff::new();
        for _ in 0..SLOW_OPEN_THRESHOLD {
            backoff.record_success_with_latency(PATH, SLOW_OPEN_MS + 1);
        }
        assert_eq!(backoff.state(PATH), BreakerState::Degraded);
        backoff.record_success_with_latency(PATH, 100);
        assert_eq!(backoff.state(PATH), BreakerState::Healthy);
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
