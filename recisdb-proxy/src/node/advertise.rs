//! What this node tells peers it can receive, and what it learns back.
//!
//! A [`ReceptionRouteAdvertisement`] is deliberately *not* a channel row. It
//! describes one physical way of receiving one logical mux, keeping three
//! things apart (`docs/DISTRIBUTED_TUNER_FABRIC.md` §1):
//!
//! - the **logical broadcast** (`BS`, `GR`, …), which is what a client asks for;
//! - the **delivery** it physically arrived by (direct RF, CATV
//!   transmodulation, another proxy), which is what quality and preference
//!   depend on;
//! - the **transport path** to the node, which is a separate failure domain
//!   handled by `node::path`.
//!
//! A BS mux received over CATV stays logically BS. Collapsing the two is what
//! makes a fabric pick a bad route and call it the right one.

use std::sync::Arc;

use recisdb_protocol::BandType;

use crate::database::Database;
use crate::server::listener::DatabaseHandle;
use crate::tuner::TunerPool;

use super::store::NodeStore;
use super::types::{
    DeliveryType, LogicalBroadcastType, LogicalMuxId, NodeId, ReceptionRouteAdvertisement,
    ReceptionRouteState,
};

/// One local physical route, straight out of the channel/driver tables.
struct LocalRoute {
    mux: LogicalMuxId,
    band_type: Option<u8>,
    dll_path: String,
    bon_space: u32,
    bon_channel: u32,
    max_instances: i32,
}

/// Enumerate the distinct (mux, driver, tuning) triples this node can receive.
///
/// Enabled rows only: a disabled channel is not something to offer a peer.
fn local_routes(db: &Database) -> Result<Vec<LocalRoute>, crate::database::DatabaseError> {
    let conn = db.connection();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT c.nid, c.tsid, c.band_type,
                bd.dll_path, c.bon_space, c.bon_channel, bd.max_instances
         FROM channels c
         JOIN bon_drivers bd ON c.bon_driver_id = bd.id
         WHERE c.is_enabled = 1
           AND c.bon_space IS NOT NULL
           AND c.bon_channel IS NOT NULL
         ORDER BY c.nid, c.tsid, bd.dll_path",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(LocalRoute {
                mux: LogicalMuxId {
                    nid: row.get::<_, i64>(0)? as u16,
                    tsid: row.get::<_, i64>(1)? as u16,
                },
                band_type: row.get::<_, Option<i64>>(2)?.map(|b| b as u8),
                dll_path: row.get(3)?,
                bon_space: row.get::<_, i64>(4)? as u32,
                bon_channel: row.get::<_, i64>(5)? as u32,
                max_instances: row.get::<_, i64>(6)? as i32,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Logical family of a scanned mux.
///
/// `BandType::CATV` is the one case where the logical answer is genuinely
/// unknown from the band alone: a CATV feed may carry re-muxed terrestrial,
/// BS, or the operator's own channels. It is reported as `CatvOriginal` only
/// when the NID says so; otherwise the NID classification wins, because a BS
/// mux delivered over CATV is still logically BS.
pub fn logical_broadcast_for(mux: LogicalMuxId, band_type: Option<u8>) -> LogicalBroadcastType {
    use recisdb_protocol::broadcast_region::classify_nid;
    use recisdb_protocol::BroadcastType;

    // NID first: `classify_nid` is the single source of truth for 4K
    // (0x000B/0x000C), and getting that wrong is what makes B25 run over an
    // already-descrambled 4K stream (CLAUDE.md, docs/FOURK_SETUP.md).
    if matches!(classify_nid(mux.nid).0, BroadcastType::FourK) {
        return LogicalBroadcastType::Bs4k;
    }
    match band_type.and_then(band_from_u8) {
        Some(BandType::Terrestrial) => LogicalBroadcastType::Terrestrial,
        Some(BandType::BS) => LogicalBroadcastType::Bs,
        Some(BandType::CS) => LogicalBroadcastType::Cs110,
        Some(BandType::FourK) => LogicalBroadcastType::Bs4k,
        Some(BandType::CATV) => LogicalBroadcastType::CatvOriginal,
        _ => LogicalBroadcastType::Unknown,
    }
}

/// How the mux physically arrived. Without a CATV-specific scan this node can
/// only distinguish "direct RF, terrestrial tuner" from "direct RF, satellite
/// tuner" and "the operator called it CATV".
pub fn delivery_for(band_type: Option<u8>) -> DeliveryType {
    match band_type.and_then(band_from_u8) {
        Some(BandType::Terrestrial) => DeliveryType::IsdbTDirect,
        Some(BandType::BS) | Some(BandType::CS) | Some(BandType::FourK) => {
            DeliveryType::IsdbSDirect
        }
        Some(BandType::CATV) => DeliveryType::CatvPassThrough,
        _ => DeliveryType::Unknown,
    }
}

fn band_from_u8(value: u8) -> Option<BandType> {
    match value {
        0 => Some(BandType::Terrestrial),
        1 => Some(BandType::BS),
        2 => Some(BandType::CS),
        3 => Some(BandType::FourK),
        4 => Some(BandType::Other),
        5 => Some(BandType::CATV),
        _ => None,
    }
}

/// Route id shared with `node::serve::route_id_for` — the same physical target
/// must have the same id whether it is being advertised or leased.
pub fn route_id(dll_path: &str, space: u32, channel: u32) -> String {
    format!("{dll_path}#{space}:{channel}")
}

/// Build the advertisement list this node serves from `GET /node/v3/routes`.
///
/// `tuner_pool` is used only for live slot occupancy; the advertisement is
/// still produced (with `available_slots = 0`) when a driver is full, because
/// "busy" and "cannot receive this" are different answers and a peer needs to
/// tell them apart.
pub async fn build_local_advertisements(
    database: &DatabaseHandle,
    tuner_pool: &Arc<TunerPool>,
    local_node: &NodeId,
) -> Result<Vec<ReceptionRouteAdvertisement>, crate::database::DatabaseError> {
    let routes = {
        let db = database.lock().await;
        local_routes(&db)?
    };

    let now = chrono::Utc::now().timestamp_millis();
    let mut out = Vec::with_capacity(routes.len());
    for route in routes {
        let running = crate::server::session_capacity::count_running_instances_on_driver(
            tuner_pool,
            &route.dll_path,
            None,
        )
        .await;
        let total_slots = route.max_instances.max(0) as u32;
        let available_slots = total_slots.saturating_sub(running.max(0) as u32);

        out.push(ReceptionRouteAdvertisement {
            route_id: route_id(&route.dll_path, route.bon_space, route.bon_channel),
            origin_node: local_node.clone(),
            mux: route.mux,
            logical_broadcast: logical_broadcast_for(route.mux, route.band_type),
            ingress_delivery: delivery_for(route.band_type),
            ultimate_delivery: delivery_for(route.band_type),
            path: Vec::new(),
            // Scanned and enabled means "we have received this"; promotion to
            // Preferred/Degraded is `node::qualification`'s job once real
            // observations exist.
            state: ReceptionRouteState::Usable,
            available_slots,
            total_slots,
            // Nothing measures tune latency per route yet; 0 means "unknown",
            // not "instant" — path scoring treats it as no information.
            predicted_ready_ms: 0,
            source_quality: 0.0,
            confidence: 0.0,
            generation: now as u64,
            observed_at_unix_ms: now,
        });
    }
    Ok(out)
}

/// Persist what a peer told us it can receive.
///
/// Advertisements that would route back through this node are dropped:
/// `validate_for` is loop detection, and a route that passes through us is not
/// a way for us to reach anything.
pub fn store_peer_advertisements(
    db: &Database,
    local_node: &NodeId,
    peer: &NodeId,
    advertisements: &[ReceptionRouteAdvertisement],
) -> Result<usize, crate::database::DatabaseError> {
    let store = NodeStore::new(db)?;
    let mut accepted = Vec::with_capacity(advertisements.len());
    for advertisement in advertisements {
        match advertisement.validate_for(local_node) {
            Ok(()) => accepted.push(advertisement.clone()),
            Err(e) => log::debug!(
                "[node] dropping route {} from {}: {}",
                advertisement.route_id,
                peer,
                e
            ),
        }
    }
    store.replace_remote_routes(peer, &accepted)?;
    Ok(accepted.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catv_delivered_bs_stays_logically_bs() {
        // NID 0x0004 is BS. Even when the row was scanned on a CATV tuner the
        // logical family must not become "CATV original" — that is what makes
        // a client's BS request resolvable through a CATV feed.
        let bs = LogicalMuxId { nid: 0x0004, tsid: 0x4010 };
        assert_eq!(
            logical_broadcast_for(bs, Some(BandType::BS as u8)),
            LogicalBroadcastType::Bs
        );
        // ... while the delivery it arrived by is recorded separately.
        assert_eq!(delivery_for(Some(BandType::CATV as u8)), DeliveryType::CatvPassThrough);
        assert_eq!(delivery_for(Some(BandType::BS as u8)), DeliveryType::IsdbSDirect);
    }

    #[test]
    fn four_k_is_classified_by_nid_not_only_by_band() {
        // A 4K mux scanned before band classification existed still has to be
        // reported as 4K: nothing downstream may run B25 over it.
        let four_k = LogicalMuxId { nid: 0x000B, tsid: 0x0001 };
        assert_eq!(logical_broadcast_for(four_k, None), LogicalBroadcastType::Bs4k);
        assert_eq!(
            logical_broadcast_for(four_k, Some(BandType::BS as u8)),
            LogicalBroadcastType::Bs4k
        );
    }

    #[test]
    fn route_ids_match_the_lease_side() {
        assert_eq!(route_id("/dev/px4video0", 0, 27), "/dev/px4video0#0:27");
    }
}
