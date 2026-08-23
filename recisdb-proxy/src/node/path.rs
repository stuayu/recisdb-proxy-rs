//! Network path measurement and stream-class-aware selection.
//!
//! Reception quality and network quality are intentionally independent.  A
//! route selector first chooses a usable reception source; this module then
//! chooses how to reach that node (LAN/direct/Tailscale/Cloudflare/etc.).

use serde::{Deserialize, Serialize};

use recisdb_protocol::StreamClass;

use super::types::{EndpointKind, NodeEndpoint, TailscalePathKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathState {
    Unknown,
    Healthy,
    Degraded,
    Unreachable,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathHealth {
    pub state: PathState,
    pub connect_success_rate: f64,
    pub rtt_p50_ms: f64,
    pub rtt_p95_ms: f64,
    /// Conservative throughput estimate. p10 is used for admission rather
    /// than the average so a route with occasional deep collapses is not
    /// selected for recording merely because its mean is high.
    pub throughput_down_p10_bps: u64,
    pub throughput_down_ewma_bps: u64,
    pub jitter_ms: f64,
    pub stall_rate: f64,
    pub reconnect_rate: f64,
    pub confidence: f64,
    pub tailscale_path: Option<TailscalePathKind>,
    pub measured_at_unix_ms: i64,
}

impl Default for PathHealth {
    fn default() -> Self {
        Self {
            state: PathState::Unknown,
            connect_success_rate: 0.0,
            rtt_p50_ms: f64::INFINITY,
            rtt_p95_ms: f64::INFINITY,
            throughput_down_p10_bps: 0,
            throughput_down_ewma_bps: 0,
            jitter_ms: f64::INFINITY,
            stall_rate: 1.0,
            reconnect_rate: 1.0,
            confidence: 0.0,
            tailscale_path: None,
            measured_at_unix_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportPath {
    pub id: String,
    pub endpoint: NodeEndpoint,
    pub health: PathHealth,
}

#[derive(Debug, Clone, Copy)]
pub struct PathPolicy {
    /// Required bandwidth headroom over measured stream bitrate.
    pub view_headroom: f64,
    pub preview_headroom: f64,
    pub record_headroom: f64,
}

impl Default for PathPolicy {
    fn default() -> Self {
        Self {
            view_headroom: 1.20,
            preview_headroom: 1.25,
            record_headroom: 1.50,
        }
    }
}

impl PathPolicy {
    pub fn required_bitrate(self, stream_class: StreamClass, bitrate_bps: u64) -> u64 {
        let factor = match stream_class {
            StreamClass::View => self.view_headroom,
            StreamClass::Preview => self.preview_headroom,
            StreamClass::Record => self.record_headroom,
        };
        (bitrate_bps as f64 * factor).ceil() as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathScore {
    pub admissible: bool,
    /// Higher is better. Only meaningful when admissible=true.
    pub score: f64,
}

fn endpoint_base(kind: EndpointKind) -> f64 {
    match kind {
        EndpointKind::Lan => 1.00,
        EndpointKind::InternetDirect => 0.96,
        EndpointKind::Tailscale => 0.95,
        EndpointKind::CloudflarePrivate => 0.92,
        EndpointKind::Static => 0.90,
        // Published HTTP tunnels remain useful for bootstrap/control and
        // emergency viewing, but are deliberately behind private transports
        // for sustained TS because intermediary buffering/timeouts vary.
        EndpointKind::CloudflarePublic => 0.72,
    }
}

pub fn score_path(
    path: &TransportPath,
    stream_class: StreamClass,
    bitrate_bps: u64,
    policy: PathPolicy,
) -> PathScore {
    if !path.endpoint.enabled
        || path.health.state == PathState::Disabled
        || path.health.state == PathState::Unreachable
    {
        return PathScore { admissible: false, score: f64::NEG_INFINITY };
    }
    if stream_class == StreamClass::Record && !path.endpoint.record_allowed {
        return PathScore { admissible: false, score: f64::NEG_INFINITY };
    }

    let required = policy.required_bitrate(stream_class, bitrate_bps);
    if bitrate_bps > 0 && path.health.throughput_down_p10_bps < required {
        return PathScore { admissible: false, score: f64::NEG_INFINITY };
    }

    let throughput_ratio = if required == 0 {
        1.0
    } else {
        (path.health.throughput_down_p10_bps as f64 / required as f64).clamp(1.0, 4.0)
    };

    let rtt_penalty = (path.health.rtt_p95_ms.max(0.0) / 250.0).min(1.5);
    let jitter_penalty = (path.health.jitter_ms.max(0.0) / 100.0).min(1.0);
    let stall_penalty = path.health.stall_rate.clamp(0.0, 1.0);
    let reconnect_penalty = path.health.reconnect_rate.clamp(0.0, 1.0);
    let reliability = path.health.connect_success_rate.clamp(0.0, 1.0);
    let confidence = path.health.confidence.clamp(0.0, 1.0);

    let mut score = endpoint_base(path.endpoint.kind) * 100.0;
    score += throughput_ratio.ln_1p() * 12.0;
    score += reliability * 18.0;
    score += confidence * 4.0;
    score += path.endpoint.user_priority as f64;

    match stream_class {
        StreamClass::Record => {
            score -= rtt_penalty * 4.0;
            score -= jitter_penalty * 8.0;
            score -= stall_penalty * 70.0;
            score -= reconnect_penalty * 60.0;
        }
        StreamClass::View => {
            score -= rtt_penalty * 20.0;
            score -= jitter_penalty * 12.0;
            score -= stall_penalty * 35.0;
            score -= reconnect_penalty * 25.0;
        }
        StreamClass::Preview => {
            score -= rtt_penalty * 15.0;
            score -= jitter_penalty * 12.0;
            score -= stall_penalty * 40.0;
            score -= reconnect_penalty * 30.0;
        }
    }

    // DERP is intentionally a strong RECORD penalty. It remains admissible
    // when bandwidth is sufficient so it can be a last-resort path.
    if path.health.tailscale_path == Some(TailscalePathKind::Derp) {
        score -= if stream_class == StreamClass::Record { 45.0 } else { 18.0 };
    } else if path.health.tailscale_path == Some(TailscalePathKind::PeerRelay) {
        score -= if stream_class == StreamClass::Record { 12.0 } else { 5.0 };
    }

    if path.health.state == PathState::Degraded {
        score -= 25.0;
    }
    if path.endpoint.metered {
        score -= 8.0;
    }

    PathScore { admissible: true, score }
}

pub fn select_best_path<'a>(
    paths: &'a [TransportPath],
    stream_class: StreamClass,
    bitrate_bps: u64,
    policy: PathPolicy,
) -> Option<&'a TransportPath> {
    paths
        .iter()
        .filter_map(|path| {
            let score = score_path(path, stream_class, bitrate_bps, policy);
            score.admissible.then_some((path, score.score))
        })
        .max_by(|(a_path, a), (b_path, b)| {
            a.total_cmp(b)
                .then_with(|| b_path.id.cmp(&a_path.id)) // stable deterministic tie-break
        })
        .map(|(path, _)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(id: &str, kind: EndpointKind, mbps: u64, rtt: f64, stall: f64) -> TransportPath {
        TransportPath {
            id: id.into(),
            endpoint: NodeEndpoint {
                kind,
                address: format!("https://{id}"),
                enabled: true,
                record_allowed: true,
                metered: false,
                user_priority: 0,
            },
            health: PathHealth {
                state: PathState::Healthy,
                connect_success_rate: 1.0,
                rtt_p50_ms: rtt,
                rtt_p95_ms: rtt,
                throughput_down_p10_bps: mbps * 1_000_000,
                throughput_down_ewma_bps: mbps * 1_000_000,
                jitter_ms: 2.0,
                stall_rate: stall,
                reconnect_rate: 0.0,
                confidence: 1.0,
                tailscale_path: None,
                measured_at_unix_ms: 0,
            },
        }
    }

    #[test]
    fn record_rejects_path_without_headroom() {
        let p = path("slow", EndpointKind::Tailscale, 24, 10.0, 0.0);
        assert!(!score_path(&p, StreamClass::Record, 20_000_000, PathPolicy::default()).admissible);
    }

    #[test]
    fn record_prefers_stable_bandwidth_over_lower_rtt() {
        let low_rtt_unstable = path("a", EndpointKind::InternetDirect, 100, 8.0, 0.20);
        let stable = path("b", EndpointKind::CloudflarePrivate, 100, 35.0, 0.0);
        let candidates = vec![low_rtt_unstable, stable];
        let best = select_best_path(&candidates, StreamClass::Record, 20_000_000, PathPolicy::default()).unwrap();
        assert_eq!(best.id, "b");
    }

    /// Two peers that differ only in RTT: the near one wins for every stream
    /// class (RECORD weights RTT least, but nothing else separates them).
    #[test]
    fn identical_paths_are_separated_by_rtt_alone() {
        let near = path("near", EndpointKind::Tailscale, 100, 10.0, 0.0);
        let far = path("far", EndpointKind::Tailscale, 100, 100.0, 0.0);
        let candidates = vec![far, near];
        for class in [StreamClass::View, StreamClass::Preview, StreamClass::Record] {
            let best =
                select_best_path(&candidates, class, 20_000_000, PathPolicy::default()).unwrap();
            assert_eq!(best.id, "near", "{class:?} must take the nearer of two equal paths");
        }
    }

    #[test]
    fn view_can_prefer_low_latency() {
        let fast = path("fast", EndpointKind::InternetDirect, 100, 8.0, 0.0);
        let slower = path("slower", EndpointKind::CloudflarePrivate, 300, 80.0, 0.0);
        let candidates = vec![slower, fast];
        let best = select_best_path(&candidates, StreamClass::View, 20_000_000, PathPolicy::default()).unwrap();
        assert_eq!(best.id, "fast");
    }
}
