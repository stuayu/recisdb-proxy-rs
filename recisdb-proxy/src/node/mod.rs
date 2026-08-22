//! Distributed recisdb-proxy tuner fabric.
//!
//! The node layer separates three concerns that used to be collapsed into a
//! BonDriver/channel row:
//! - logical broadcast identity (`NID/TSID/SID`),
//! - physical/local-or-remote reception routes,
//! - network transport paths used to reach a remote node.
//!
//! This makes RF quality, tuner health and WAN/VPN/tunnel health independent
//! failure domains and is the basis for stable multi-site recording.

pub mod discovery;
pub mod frame;
pub mod identity;
pub mod lease;
pub mod path;
pub mod qualification;
pub mod replay;
pub mod route;
pub mod store;
pub mod transport;
pub mod types;

pub use discovery::{
    classify_tailscale_ping, discover_tailscale_endpoint, inspect_tailscale_path,
    probe_endpoint, ProbeConfig,
};
pub use frame::{FrameFlags, NodeTsFrame};
pub use identity::{NodeCredential, NodeIdentity, PairingAcceptance, PairingCode};
pub use lease::{LeasePolicy, RemoteLeaseId, RemoteLeaseManager, RemoteMuxLease};
pub use path::{
    score_path, select_best_path, PathHealth, PathPolicy, PathScore, PathState, TransportPath,
};
pub use qualification::{
    challenger_beats_current, qualify, QualificationDecision, QualificationPolicy,
    QualificationResult, ReceptionObservation, RouteQualifier,
};
pub use replay::{ReplayBudget, ReplayBuffer, ReplayError};
pub use route::{select_route, ReceptionCandidate, RouteDecision};
pub use store::{NodeStore, RouteGroup, StoredNode};
pub use transport::{NodeCapabilities, NodeHello, NodeTransportClient, NodeTransportState};
pub use types::*;
