//! Pure ordering for remote reception advertisements.

use crate::tuner::EffectiveClaim;

use super::store::StoredRemoteRoute;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RouteCapacityClass {
    Running,
    Available,
    Evictable,
    Blocked,
}

#[derive(Debug, Clone)]
pub struct RemoteRouteCandidate {
    pub route: StoredRemoteRoute,
    /// Supplied by the caller; ranking itself performs no I/O.
    pub transport_quality: f64,
}

pub fn capacity_class(route: &StoredRemoteRoute, claim: EffectiveClaim) -> RouteCapacityClass {
    if route.running {
        return RouteCapacityClass::Running;
    }
    if !route.capacity_info_known {
        // Legacy peers omit live capacity fields. Keep them compatible by
        // trying them as ordinary candidates.
        return RouteCapacityClass::Available;
    }
    if route.free_slots > 0 || route.background_slots > 0 || route.available_slots > 0 {
        return RouteCapacityClass::Available;
    }
    if route
        .lowest_client_priority
        .is_some_and(|priority| claim.priority > priority)
    {
        return RouteCapacityClass::Evictable;
    }
    RouteCapacityClass::Blocked
}

/// Sort candidates in requester-side lease-attempt order.
pub fn rank_remote_routes(candidates: &mut [RemoteRouteCandidate], claim: EffectiveClaim) {
    candidates.sort_by(|a, b| {
        capacity_class(&a.route, claim)
            .cmp(&capacity_class(&b.route, claim))
            .then_with(|| b.route.configured_priority.cmp(&a.route.configured_priority))
            .then_with(|| b.route.confidence.total_cmp(&a.route.confidence))
            .then_with(|| b.route.source_quality.total_cmp(&a.route.source_quality))
            .then_with(|| b.transport_quality.total_cmp(&a.transport_quality))
            .then_with(|| a.route.route_id.cmp(&b.route.route_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::types::{
        DeliveryType, LogicalBroadcastType, LogicalMuxId, NodeId, ReceptionRouteState,
    };

    fn route(id: &str) -> StoredRemoteRoute {
        StoredRemoteRoute {
            route_id: id.into(),
            node_id: NodeId::new("site-a").unwrap(),
            mux: LogicalMuxId { nid: 1, tsid: 1 },
            logical_broadcast: LogicalBroadcastType::Terrestrial,
            ingress_delivery: DeliveryType::IsdbTDirect,
            ultimate_delivery: DeliveryType::IsdbTDirect,
            state: ReceptionRouteState::Usable,
            configured_priority: 0,
            source_quality: 0.5,
            confidence: 0.5,
            running: false,
            available_slots: 0,
            total_slots: 1,
            free_slots: 0,
            background_slots: 0,
            lowest_client_priority: None,
            locked_slots: 0,
            capacity_info_known: true,
            last_seen_unix_ms: None,
        }
    }

    #[test]
    fn running_mux_beats_everything_else() {
        let mut running = route("running");
        running.running = true;
        let mut free = route("free");
        free.free_slots = 1;
        let mut candidates = vec![
            RemoteRouteCandidate { route: free, transport_quality: 100.0 },
            RemoteRouteCandidate { route: running, transport_quality: 0.0 },
        ];
        rank_remote_routes(&mut candidates, EffectiveClaim::new(0, false));
        assert_eq!(candidates[0].route.route_id, "running");
    }

    #[test]
    fn higher_priority_claim_can_evict_but_lock_cannot() {
        let mut evictable = route("evictable");
        evictable.lowest_client_priority = Some(5);
        let mut locked = route("locked");
        locked.locked_slots = 1;
        let mut candidates = vec![
            RemoteRouteCandidate { route: locked, transport_quality: 100.0 },
            RemoteRouteCandidate { route: evictable, transport_quality: 0.0 },
        ];
        rank_remote_routes(&mut candidates, EffectiveClaim::new(10, false));
        assert_eq!(candidates[0].route.route_id, "evictable");
        assert_eq!(candidates[1].route.route_id, "locked");
    }

    #[test]
    fn legacy_capacity_is_treated_as_available() {
        let mut legacy = route("legacy");
        legacy.capacity_info_known = false;
        assert_eq!(
            capacity_class(&legacy, EffectiveClaim::new(0, false)),
            RouteCapacityClass::Available
        );
    }
}
