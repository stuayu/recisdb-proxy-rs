//! Qualification of physical reception routes.
//!
//! Seeing NID/TSID once is only discovery. A route becomes routable after it
//! can deliver a sustained, structurally-valid TS. Weak repeaters remain in
//! the database for later re-probing but are quarantined instead of deleted.

use serde::{Deserialize, Serialize};

use super::types::ReceptionRouteState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceptionObservation {
    pub observed_at_unix_ms: i64,
    pub signal_raw: Option<f64>,
    /// Driver-family-local normalization (0..=1). Raw signal is diagnostic
    /// only and must not be compared across BonDriver families/nodes.
    pub signal_normalized: Option<f64>,
    pub tune_ms: Option<u64>,
    pub first_ts_ms: Option<u64>,
    pub sample_bytes: u64,
    pub bitrate_bps: u64,
    pub tei_rate: f64,
    pub cc_error_rate: f64,
    pub sync_error_rate: f64,
    pub scramble_rate: f64,
    pub pat_ok: bool,
    pub sdt_ok: bool,
    pub nit_ok: bool,
    pub nid_matches: bool,
    pub tsid_matches: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct QualificationPolicy {
    pub min_sample_bytes: u64,
    pub min_bitrate_bps: u64,
    pub max_tei_rate: f64,
    pub max_cc_error_rate: f64,
    pub max_sync_error_rate: f64,
    pub max_scramble_rate: f64,
    pub soft_tune_ms: u64,
    pub soft_first_ts_ms: u64,
    pub good_samples_to_promote: u32,
    pub bad_samples_to_quarantine: u32,
}

impl Default for QualificationPolicy {
    fn default() -> Self {
        Self {
            // Intentionally much stronger than "NID/TSID was parsed". The
            // exact values are recisdb safety defaults, not ARIB thresholds.
            min_sample_bytes: 512 * 1024,
            min_bitrate_bps: 1_000_000,
            max_tei_rate: 0.000_1,
            max_cc_error_rate: 0.001,
            max_sync_error_rate: 0.000_1,
            max_scramble_rate: 0.20,
            soft_tune_ms: 5_000,
            soft_first_ts_ms: 5_000,
            good_samples_to_promote: 3,
            bad_samples_to_quarantine: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationResult {
    Good,
    Degraded,
    Bad,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualificationDecision {
    pub result: QualificationResult,
    pub quality_score: f64,
    pub reasons: Vec<String>,
}

pub fn qualify(
    observation: &ReceptionObservation,
    policy: QualificationPolicy,
) -> QualificationDecision {
    let mut reasons = Vec::new();
    let structural_ok = observation.pat_ok
        && observation.sdt_ok
        && observation.nid_matches
        && observation.tsid_matches;
    if !observation.pat_ok {
        reasons.push("pat_missing".into());
    }
    if !observation.sdt_ok {
        reasons.push("sdt_missing".into());
    }
    if !observation.nid_matches {
        reasons.push("nid_mismatch".into());
    }
    if !observation.tsid_matches {
        reasons.push("tsid_mismatch".into());
    }
    if observation.sample_bytes < policy.min_sample_bytes {
        reasons.push("insufficient_sample".into());
    }
    if observation.bitrate_bps < policy.min_bitrate_bps {
        reasons.push("bitrate_too_low".into());
    }
    if observation.tei_rate > policy.max_tei_rate {
        reasons.push("tei_rate".into());
    }
    if observation.cc_error_rate > policy.max_cc_error_rate {
        reasons.push("cc_error_rate".into());
    }
    if observation.sync_error_rate > policy.max_sync_error_rate {
        reasons.push("sync_error_rate".into());
    }
    if observation.scramble_rate > policy.max_scramble_rate {
        reasons.push("scramble_rate".into());
    }

    let hard_bad = !structural_ok
        || observation.sample_bytes < policy.min_sample_bytes
        || observation.bitrate_bps < policy.min_bitrate_bps
        || observation.tei_rate > policy.max_tei_rate * 5.0
        || observation.cc_error_rate > policy.max_cc_error_rate * 5.0
        || observation.sync_error_rate > policy.max_sync_error_rate * 5.0;

    let slow_tune = observation
        .tune_ms
        .is_some_and(|ms| ms > policy.soft_tune_ms);
    let slow_first = observation
        .first_ts_ms
        .is_some_and(|ms| ms > policy.soft_first_ts_ms);
    if slow_tune {
        reasons.push("slow_tune".into());
    }
    if slow_first {
        reasons.push("slow_first_ts".into());
    }

    let integrity_penalty = observation.tei_rate * 1000.0
        + observation.cc_error_rate * 300.0
        + observation.sync_error_rate * 1000.0
        + observation.scramble_rate * 0.5;
    let latency_penalty = if slow_tune { 0.08 } else { 0.0 } + if slow_first { 0.08 } else { 0.0 };
    let signal = observation.signal_normalized.unwrap_or(0.8).clamp(0.0, 1.0);
    let quality_score = (signal * 0.35 + (1.0 - integrity_penalty).clamp(0.0, 1.0) * 0.65
        - latency_penalty)
        .clamp(0.0, 1.0);

    let result = if hard_bad {
        QualificationResult::Bad
    } else if !reasons.is_empty() || quality_score < 0.65 {
        QualificationResult::Degraded
    } else {
        QualificationResult::Good
    };

    QualificationDecision {
        result,
        quality_score,
        reasons,
    }
}

#[derive(Debug, Clone)]
pub struct RouteQualifier {
    pub state: ReceptionRouteState,
    consecutive_good: u32,
    consecutive_bad: u32,
}

impl Default for RouteQualifier {
    fn default() -> Self {
        Self {
            state: ReceptionRouteState::Discovered,
            consecutive_good: 0,
            consecutive_bad: 0,
        }
    }
}

impl RouteQualifier {
    pub fn observe(
        &mut self,
        decision: &QualificationDecision,
        policy: QualificationPolicy,
    ) -> ReceptionRouteState {
        match decision.result {
            QualificationResult::Good => {
                self.consecutive_good = self.consecutive_good.saturating_add(1);
                self.consecutive_bad = 0;
                if self.consecutive_good >= policy.good_samples_to_promote {
                    self.state = ReceptionRouteState::Usable;
                } else if self.state == ReceptionRouteState::Discovered {
                    self.state = ReceptionRouteState::Validated;
                } else if matches!(
                    self.state,
                    ReceptionRouteState::Quarantined | ReceptionRouteState::Degraded
                ) {
                    // Stay conservative until the promotion threshold is met.
                }
            }
            QualificationResult::Degraded => {
                self.consecutive_good = 0;
                self.consecutive_bad = self.consecutive_bad.saturating_add(1);
                if self.state.routable() {
                    self.state = ReceptionRouteState::Degraded;
                }
            }
            QualificationResult::Bad => {
                self.consecutive_good = 0;
                self.consecutive_bad = self.consecutive_bad.saturating_add(1);
                if self.consecutive_bad >= policy.bad_samples_to_quarantine {
                    self.state = ReceptionRouteState::Quarantined;
                } else if self.state.routable() {
                    self.state = ReceptionRouteState::Degraded;
                }
            }
        }
        self.state
    }
}

/// Decide whether a challenger reception route is sufficiently better than
/// the current preferred route to avoid RF flapping. The ratio is a recisdb
/// policy parameter, not an ARIB-defined threshold.
pub fn challenger_beats_current(
    current_quality: f64,
    challenger_quality: f64,
    required_ratio: f64,
) -> bool {
    if current_quality <= 0.0 {
        return challenger_quality > current_quality;
    }
    challenger_quality >= current_quality * required_ratio.max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> ReceptionObservation {
        ReceptionObservation {
            observed_at_unix_ms: 0,
            signal_raw: Some(30.0),
            signal_normalized: Some(0.9),
            tune_ms: Some(300),
            first_ts_ms: Some(500),
            sample_bytes: 2 * 1024 * 1024,
            bitrate_bps: 18_000_000,
            tei_rate: 0.0,
            cc_error_rate: 0.0,
            sync_error_rate: 0.0,
            scramble_rate: 0.0,
            pat_ok: true,
            sdt_ok: true,
            nit_ok: true,
            nid_matches: true,
            tsid_matches: true,
        }
    }

    #[test]
    fn nid_tsid_only_is_not_usable() {
        let mut weak = good();
        weak.sample_bytes = 20_000;
        weak.bitrate_bps = 100_000;
        let d = qualify(&weak, QualificationPolicy::default());
        assert_eq!(d.result, QualificationResult::Bad);
    }

    #[test]
    fn requires_repeated_good_samples_to_promote() {
        let policy = QualificationPolicy::default();
        let d = qualify(&good(), policy);
        let mut q = RouteQualifier::default();
        assert_eq!(q.observe(&d, policy), ReceptionRouteState::Validated);
        assert_eq!(q.observe(&d, policy), ReceptionRouteState::Validated);
        assert_eq!(q.observe(&d, policy), ReceptionRouteState::Usable);
    }

    #[test]
    fn repeated_bad_samples_quarantine_but_do_not_delete() {
        let policy = QualificationPolicy::default();
        let mut bad = good();
        bad.pat_ok = false;
        let d = qualify(&bad, policy);
        let mut q = RouteQualifier {
            state: ReceptionRouteState::Usable,
            consecutive_good: 0,
            consecutive_bad: 0,
        };
        q.observe(&d, policy);
        q.observe(&d, policy);
        assert_eq!(q.state, ReceptionRouteState::Degraded);
        q.observe(&d, policy);
        assert_eq!(q.state, ReceptionRouteState::Quarantined);
    }

    #[test]
    fn hysteresis_prevents_small_quality_flapping() {
        assert!(!challenger_beats_current(0.90, 0.95, 1.15));
        assert!(challenger_beats_current(0.80, 0.95, 1.15));
    }
}
