use serde::{Deserialize, Serialize};
use std::fmt;

use crate::tuner::EffectiveClaim;
use recisdb_protocol::StreamClass;

/// Stable identity of one recisdb-proxy node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() || value.len() > 128 {
            return Err("node id must be 1..=128 characters");
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err("node id contains unsupported characters");
        }
        Ok(Self(value.to_owned()))
    }

    /// Generate an opaque local id without adding a UUID dependency.
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("OS RNG unavailable for node identity");
        let mut out = String::with_capacity(32);
        for byte in bytes {
            use std::fmt::Write;
            let _ = write!(&mut out, "{byte:02x}");
        }
        Self(out)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Logical identity of a transport stream, independent of RF frequency,
/// BonDriver space/channel and remote-node path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogicalMuxId {
    pub nid: u16,
    pub tsid: u16,
}

/// Logical broadcast family advertised to clients and used for channel lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalBroadcastType {
    Terrestrial,
    Bs,
    Cs110,
    Bs4k,
    Cs4k,
    CatvOriginal,
    Sky,
    Unknown,
}

/// How a mux physically reaches this node.  This is intentionally separate
/// from [`LogicalBroadcastType`]: a BS mux can arrive through CATV
/// transmodulation while remaining logically BS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryType {
    IsdbTDirect,
    IsdbSDirect,
    CatvPassThrough,
    CatvRemux,
    CatvTsmf,
    CatvTransmodulation,
    RemoteProxy,
    Unknown,
}

impl DeliveryType {
    /// Default routing tier. Lower is preferred after usability has been
    /// established. A healthy fallback in a later tier still beats an
    /// unusable direct-RF route.
    pub const fn preference_tier(self) -> u8 {
        match self {
            DeliveryType::IsdbTDirect | DeliveryType::IsdbSDirect => 1,
            DeliveryType::RemoteProxy => 2,
            DeliveryType::CatvPassThrough => 3,
            DeliveryType::CatvRemux
            | DeliveryType::CatvTsmf
            | DeliveryType::CatvTransmodulation => 4,
            DeliveryType::Unknown => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceptionRouteState {
    Discovered,
    Validated,
    Usable,
    Preferred,
    Degraded,
    Quarantined,
    Disabled,
}

impl ReceptionRouteState {
    pub const fn routable(self) -> bool {
        matches!(
            self,
            ReceptionRouteState::Usable
                | ReceptionRouteState::Preferred
                | ReceptionRouteState::Degraded
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Lan,
    InternetDirect,
    Tailscale,
    CloudflarePrivate,
    CloudflarePublic,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailscalePathKind {
    Direct,
    PeerRelay,
    Derp,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEndpoint {
    pub kind: EndpointKind,
    pub address: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub record_allowed: bool,
    #[serde(default)]
    pub metered: bool,
    #[serde(default)]
    pub user_priority: i32,
}

impl NodeEndpoint {
    /// An operator-supplied base URL with no discovery behind it. Recording
    /// over it is allowed by default: it is an address the user typed for
    /// this specific peer, unlike a discovered Cloudflare/DERP fallback.
    pub fn direct(address: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::Static,
            address: address.into(),
            enabled: true,
            record_allowed: true,
            metered: false,
            user_priority: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Request metadata that must remain consistent across every proxy hop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestContext {
    pub request_id: String,
    pub trace_id: String,
    pub stream_class: StreamClass,
    pub claim: EffectiveClaim,
    /// End-to-end remaining budget. Each hop subtracts time already spent;
    /// hops must not start a fresh full timeout.
    pub remaining_ms: u64,
    pub origin_node: NodeId,
    #[serde(default)]
    pub visited_nodes: Vec<NodeId>,
    #[serde(default)]
    pub hop_count: u8,
    #[serde(default = "default_max_hops")]
    pub max_hops: u8,
}

const fn default_max_hops() -> u8 {
    3
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HopError {
    #[error("route loop detected through node {0}")]
    Loop(NodeId),
    #[error("maximum proxy hop count exceeded ({0})")]
    MaxHops(u8),
    #[error("end-to-end request deadline exhausted")]
    Deadline,
}

impl RequestContext {
    pub fn enter_node(&mut self, node: &NodeId, spent_ms: u64) -> Result<(), HopError> {
        self.remaining_ms = self.remaining_ms.saturating_sub(spent_ms);
        if self.remaining_ms == 0 {
            return Err(HopError::Deadline);
        }
        if self.visited_nodes.iter().any(|seen| seen == node) {
            return Err(HopError::Loop(node.clone()));
        }
        if self.hop_count >= self.max_hops {
            return Err(HopError::MaxHops(self.max_hops));
        }
        self.visited_nodes.push(node.clone());
        self.hop_count = self.hop_count.saturating_add(1);
        Ok(())
    }
}

/// One physical/local-or-remote way of receiving a logical mux.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceptionRouteAdvertisement {
    pub route_id: String,
    pub origin_node: NodeId,
    pub mux: LogicalMuxId,
    pub logical_broadcast: LogicalBroadcastType,
    pub ingress_delivery: DeliveryType,
    /// Ultimate physical delivery is preserved through RemoteProxy hops.
    pub ultimate_delivery: DeliveryType,
    #[serde(default)]
    pub path: Vec<NodeId>,
    pub state: ReceptionRouteState,
    pub available_slots: u32,
    pub total_slots: u32,
    pub predicted_ready_ms: u64,
    /// Normalized 0..=1 source quality. Never compare raw BonDriver signal
    /// values across driver families/nodes.
    pub source_quality: f64,
    pub confidence: f64,
    pub generation: u64,
    pub observed_at_unix_ms: i64,
}

impl ReceptionRouteAdvertisement {
    pub fn validate_for(&self, receiver: &NodeId) -> Result<(), HopError> {
        if self.path.iter().any(|node| node == receiver) || &self.origin_node == receiver {
            return Err(HopError::Loop(receiver.clone()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_context_rejects_loop_and_preserves_budget() {
        let a = NodeId::new("a").unwrap();
        let b = NodeId::new("b").unwrap();
        let mut ctx = RequestContext {
            request_id: "r".into(),
            trace_id: "t".into(),
            stream_class: StreamClass::Record,
            claim: EffectiveClaim::new(2, false),
            remaining_ms: 12_000,
            origin_node: a.clone(),
            visited_nodes: vec![a.clone()],
            hop_count: 1,
            max_hops: 3,
        };
        ctx.enter_node(&b, 250).unwrap();
        assert_eq!(ctx.remaining_ms, 11_750);
        assert_eq!(ctx.enter_node(&a, 1), Err(HopError::Loop(a)));
    }

    #[test]
    fn catv_is_a_delivery_axis_not_logical_band() {
        let logical = LogicalBroadcastType::Bs;
        let delivery = DeliveryType::CatvTsmf;
        assert_eq!(logical, LogicalBroadcastType::Bs);
        assert!(delivery.preference_tier() > DeliveryType::IsdbSDirect.preference_tier());
    }
}
