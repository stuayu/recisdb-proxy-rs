//! End-to-end route selection.
//!
//! Selection is deliberately two-stage:
//! 1. choose a usable reception source for the requested NID/TSID;
//! 2. for remote sources, choose a transport path to that node.
//!
//! Raw RF signal values are never compared across nodes. `source_quality` is
//! a locally-normalized score derived from TS integrity, tune stability and
//! driver-specific signal calibration.

use recisdb_protocol::StreamClass;

use super::path::{score_path, PathPolicy, TransportPath};
use super::types::{DeliveryType, ReceptionRouteAdvertisement, ReceptionRouteState};

#[derive(Debug, Clone)]
pub struct ReceptionCandidate {
    pub advertisement: ReceptionRouteAdvertisement,
    /// True when this exact logical mux is already flowing on the candidate;
    /// joining it costs no retune and is preferred when healthy.
    pub mux_already_running: bool,
    /// 0.0 = idle, 1.0 = fully occupied. Values above 1 are clamped.
    pub load_ratio: f64,
    /// Network paths are empty for a local reception route.
    pub transport_paths: Vec<TransportPath>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteDecision {
    pub route_id: String,
    pub transport_path_id: Option<String>,
    pub score: f64,
}

fn state_rank(state: ReceptionRouteState) -> Option<u8> {
    match state {
        ReceptionRouteState::Preferred => Some(0),
        ReceptionRouteState::Usable => Some(1),
        ReceptionRouteState::Degraded => Some(2),
        ReceptionRouteState::Discovered
        | ReceptionRouteState::Validated
        | ReceptionRouteState::Quarantined
        | ReceptionRouteState::Disabled => None,
    }
}

fn delivery_rank(delivery: DeliveryType) -> u8 {
    delivery.preference_tier()
}

pub fn select_route(
    candidates: &[ReceptionCandidate],
    stream_class: StreamClass,
    bitrate_bps: u64,
    path_policy: PathPolicy,
) -> Option<RouteDecision> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let state_rank = state_rank(candidate.advertisement.state)?;
            if candidate.advertisement.total_slots == 0 {
                return None;
            }

            // A route with zero currently-free slots remains eligible only
            // when the requested mux is already running and can be shared.
            if candidate.advertisement.available_slots == 0 && !candidate.mux_already_running {
                return None;
            }

            let chosen_path =
                if candidate.advertisement.ingress_delivery == DeliveryType::RemoteProxy {
                    candidate
                        .transport_paths
                        .iter()
                        .filter_map(|path| {
                            let score = score_path(path, stream_class, bitrate_bps, path_policy);
                            score.admissible.then_some((path, score.score))
                        })
                        .max_by(|(a_path, a), (b_path, b)| {
                            a.total_cmp(b).then_with(|| b_path.id.cmp(&a_path.id))
                        })
                } else {
                    None
                };

            if candidate.advertisement.ingress_delivery == DeliveryType::RemoteProxy
                && chosen_path.is_none()
            {
                return None;
            }

            // Use lexicographic priorities as large score bands instead of a
            // single blended weight. This preserves invariants such as
            // "healthy direct reception beats healthy CATV transmod" while
            // still letting source quality/load break ties within the band.
            let mut score = 10_000.0;
            score -= state_rank as f64 * 2_000.0;
            score -= delivery_rank(candidate.advertisement.ultimate_delivery) as f64 * 500.0;
            if candidate.mux_already_running {
                score += 1_200.0;
            }

            score += candidate.advertisement.source_quality.clamp(0.0, 1.0) * 250.0;
            score += candidate.advertisement.confidence.clamp(0.0, 1.0) * 50.0;
            score -= candidate.load_ratio.clamp(0.0, 1.0) * 120.0;
            score -= (candidate.advertisement.predicted_ready_ms as f64 / 1000.0).min(30.0) * 8.0;

            let transport_path_id = chosen_path.as_ref().map(|(path, _)| path.id.clone());
            if let Some((_, transport_score)) = chosen_path {
                // Transport is important but cannot turn an unusable source
                // into a valid one (hard-gated above).
                score += transport_score * 0.25;
            }

            Some(RouteDecision {
                route_id: candidate.advertisement.route_id.clone(),
                transport_path_id,
                score,
            })
        })
        .max_by(|a, b| {
            a.score
                .total_cmp(&b.score)
                .then_with(|| b.route_id.cmp(&a.route_id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::path::{PathHealth, PathState};
    use crate::node::types::{
        EndpointKind, LogicalBroadcastType, LogicalMuxId, NodeEndpoint, NodeId, TailscalePathKind,
    };

    fn ad(
        id: &str,
        delivery: DeliveryType,
        state: ReceptionRouteState,
        q: f64,
    ) -> ReceptionRouteAdvertisement {
        ReceptionRouteAdvertisement {
            route_id: id.into(),
            origin_node: NodeId::new(id).unwrap(),
            mux: LogicalMuxId { nid: 1, tsid: 1 },
            logical_broadcast: LogicalBroadcastType::Terrestrial,
            ingress_delivery: delivery,
            ultimate_delivery: delivery,
            path: vec![],
            state,
            available_slots: 1,
            total_slots: 1,
            predicted_ready_ms: 500,
            source_quality: q,
            confidence: 1.0,
            generation: 1,
            observed_at_unix_ms: 0,
        }
    }

    #[test]
    fn quarantined_weak_repeater_is_never_selected() {
        let good = ReceptionCandidate {
            advertisement: ad(
                "good",
                DeliveryType::IsdbTDirect,
                ReceptionRouteState::Usable,
                0.90,
            ),
            mux_already_running: false,
            load_ratio: 0.5,
            transport_paths: vec![],
        };
        let weak = ReceptionCandidate {
            advertisement: ad(
                "weak",
                DeliveryType::IsdbTDirect,
                ReceptionRouteState::Quarantined,
                0.20,
            ),
            mux_already_running: true,
            load_ratio: 0.0,
            transport_paths: vec![],
        };
        assert_eq!(
            select_route(
                &[weak, good],
                StreamClass::View,
                18_000_000,
                PathPolicy::default()
            )
            .unwrap()
            .route_id,
            "good"
        );
    }

    #[test]
    fn healthy_direct_rf_is_preferred_over_catv_when_both_are_usable() {
        let rf = ReceptionCandidate {
            advertisement: ad(
                "rf",
                DeliveryType::IsdbSDirect,
                ReceptionRouteState::Usable,
                0.85,
            ),
            mux_already_running: false,
            load_ratio: 0.3,
            transport_paths: vec![],
        };
        let catv = ReceptionCandidate {
            advertisement: ad(
                "catv",
                DeliveryType::CatvTsmf,
                ReceptionRouteState::Preferred,
                1.0,
            ),
            mux_already_running: false,
            load_ratio: 0.0,
            transport_paths: vec![],
        };
        // Preferred state is intentionally stronger than delivery tier. This
        // represents an operator/qualification decision that CATV is the
        // currently known-good route. With both merely Usable, direct wins.
        let mut catv_usable = catv.clone();
        catv_usable.advertisement.state = ReceptionRouteState::Usable;
        assert_eq!(
            select_route(
                &[catv_usable, rf],
                StreamClass::Record,
                20_000_000,
                PathPolicy::default()
            )
            .unwrap()
            .route_id,
            "rf"
        );
    }

    #[test]
    fn remote_route_requires_admissible_transport() {
        let mut remote_ad = ad(
            "remote",
            DeliveryType::RemoteProxy,
            ReceptionRouteState::Usable,
            1.0,
        );
        remote_ad.ultimate_delivery = DeliveryType::IsdbTDirect;
        let remote = ReceptionCandidate {
            advertisement: remote_ad,
            mux_already_running: false,
            load_ratio: 0.0,
            transport_paths: vec![TransportPath {
                id: "derp".into(),
                endpoint: NodeEndpoint {
                    kind: EndpointKind::Tailscale,
                    address: "http://remote".into(),
                    enabled: true,
                    record_allowed: true,
                    metered: false,
                    user_priority: 0,
                },
                health: PathHealth {
                    state: PathState::Healthy,
                    connect_success_rate: 1.0,
                    rtt_p50_ms: 50.0,
                    rtt_p95_ms: 80.0,
                    throughput_down_p10_bps: 10_000_000,
                    throughput_down_ewma_bps: 20_000_000,
                    jitter_ms: 3.0,
                    stall_rate: 0.0,
                    reconnect_rate: 0.0,
                    confidence: 1.0,
                    tailscale_path: Some(TailscalePathKind::Derp),
                    measured_at_unix_ms: 0,
                },
            }],
        };
        assert!(select_route(
            &[remote],
            StreamClass::Record,
            20_000_000,
            PathPolicy::default()
        )
        .is_none());
    }
}
