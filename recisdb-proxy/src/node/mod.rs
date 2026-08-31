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

pub mod advertise;
pub mod consume;
pub mod discovery;
pub mod frame;
pub mod identity;
pub mod lease;
pub mod path;
pub mod qualification;
pub mod replay;
pub mod route;
pub mod serve;
pub mod store;
pub mod sync;
pub mod transport;
pub mod types;

pub use consume::{ConsumeError, RemoteMuxStream};
pub use discovery::{
    classify_tailscale_ping, discover_tailscale_endpoint, inspect_tailscale_path, probe_endpoint,
    ProbeConfig,
};
pub use frame::{FrameFlags, NodeTsFrame};
pub use identity::{NodeCredential, NodeIdentity, PairingAcceptance, PairingCode};
pub use lease::{
    LeasePolicy, MuxLeaseGuard, MuxLeaseManager, RemoteLeaseId, RemoteLeaseManager, RemoteMuxLease,
};
pub use path::{
    score_path, select_best_path, PathHealth, PathPolicy, PathScore, PathState, TransportPath,
};
pub use qualification::{
    challenger_beats_current, qualify, QualificationDecision, QualificationPolicy,
    QualificationResult, ReceptionObservation, RouteQualifier,
};
pub use replay::{ReplayBudget, ReplayBuffer, ReplayError};
pub use route::{select_route, ReceptionCandidate, RouteDecision};
pub use serve::{LocalMuxServer, ServeError};
pub use store::{NodeStore, PendingPairing, RouteGroup, StoredNode, StoredRemoteRoute};
pub use sync::{RouteSync, DEFAULT_SYNC_INTERVAL};
pub use transport::{
    serve_h2c, LeaseStreamError, NodeCapabilities, NodeHello, NodeTransportClient,
    NodeTransportState, OpenLeaseReply, OpenLeaseRequest, RemoteEpgMetadataRequest,
    PAIRING_CODE_TTL,
};
pub use types::*;
