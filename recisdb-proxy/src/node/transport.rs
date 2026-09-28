//! Dedicated recisdb-to-recisdb transport.
//!
//! This router is intentionally separate from the dashboard/Mirakurun
//! listener. Long-lived/high-bandwidth node streams must not share the same
//! HTTP connection pool or failure domain as UI/API polling.
//!
//! The listener can be served as HTTP/2 prior-knowledge (h2c) only on a
//! trusted encrypted overlay such as Tailscale/Cloudflare private routing.
//! Direct Internet exposure must terminate TLS and advertise `h2` via ALPN;
//! callers distinguish this by endpoint scheme and policy.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use recisdb_protocol::StreamClass;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::frame::{FrameFlags, NodeTsFrame};
use super::identity::{NodeCredential, NodeIdentity, PairingAcceptance, PairingCode};
use super::lease::{RemoteLeaseId, RemoteLeaseManager};
use super::serve::{LocalMuxServer, ProgramUpsertWire, RemoteEpgMetadataReply, ServeError};
use super::store::{NodeStore, StoredNode};
use super::types::{
    LogicalMuxId, NodeEndpoint, NodeId, ReceptionRouteAdvertisement, RequestContext,
};
use crate::scheduler::epg_dwell::EpgDwellConfig;
use crate::server::listener::DatabaseHandle;

pub const NODE_PROTOCOL_VERSION: u16 = 3;
pub const MAX_ACTIVE_PROBE_BYTES: usize = 16 * 1024 * 1024;

/// How long a freshly issued pairing code stays redeemable.
pub const PAIRING_CODE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Failed `/node/v3/pair` attempts tolerated inside [`PAIRING_ATTEMPT_WINDOW`]
/// before the endpoint stops answering. The code carries 64 bits of entropy,
/// so this is belt-and-braces against an attacker who can reach the node
/// listener rather than the primary defence.
const PAIRING_ATTEMPT_LIMIT: u32 = 10;
const PAIRING_ATTEMPT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapabilities {
    pub protocol_version: u16,
    pub supports_h2: bool,
    pub supports_h3: bool,
    pub supports_resume: bool,
    pub supports_replay: bool,
    pub supports_record_no_drop: bool,
    pub max_frame_payload: usize,
}

impl Default for NodeCapabilities {
    fn default() -> Self {
        Self {
            protocol_version: NODE_PROTOCOL_VERSION,
            supports_h2: true,
            supports_h3: false,
            supports_resume: true,
            supports_replay: true,
            supports_record_no_drop: true,
            max_frame_payload: super::frame::MAX_NODE_TS_PAYLOAD,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHello {
    pub identity: NodeIdentity,
    pub capabilities: NodeCapabilities,
    /// Older nodes omit this field. An empty list remains a valid response.
    #[serde(default)]
    pub endpoints: Vec<NodeEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReply {
    pub nonce: String,
    pub server_unix_ms: i64,
}

#[derive(Debug, Deserialize)]
struct ProbeQuery {
    #[serde(default)]
    nonce: String,
}

#[derive(Debug, Deserialize)]
struct DownloadProbeQuery {
    bytes: usize,
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    generation: Option<u32>,
    from_seq: Option<u64>,
}

#[derive(Debug, Serialize)]
struct LeaseReply {
    ok: bool,
}

/// What a peer sends to open a lease on one of this node's tuners.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenLeaseRequest {
    /// Carried unchanged across every hop: claim, stream class, remaining
    /// end-to-end budget, visited nodes.
    pub context: RequestContext,
    pub mux: LogicalMuxId,
    #[serde(default)]
    pub sid: Option<u16>,
    /// Milliseconds the caller already spent reaching this node. Subtracted
    /// from `context.remaining_ms`; a hop never restarts a full timeout.
    #[serde(default)]
    pub spent_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenLeaseReply {
    pub lease_id: String,
    pub generation: u32,
    pub owner_node: NodeId,
    pub route_id: String,
    pub stream_class: StreamClass,
    /// How long the lease survives without a renew. Clients must renew well
    /// inside this window; it is *not* tied to the transport connection.
    pub ttl_ms: u64,
    /// The context as this node saw it after `enter_node`, so the caller can
    /// see the remaining budget and hop count actually charged.
    pub context: RequestContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEpgMetadataRequest {
    pub context: RequestContext,
    pub mux: LogicalMuxId,
    #[serde(default)]
    pub sid: Option<u16>,
    #[serde(default = "default_epg_dwell_secs")]
    pub dwell_secs: u64,
    #[serde(default)]
    pub spent_ms: u64,
    /// None keeps the legacy fixed-dwell behavior.
    #[serde(default)]
    pub dwell: Option<RemoteEpgDwell>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEpgDwell {
    pub min_dwell_secs: u64,
    pub normal_dwell_secs: u64,
    pub max_dwell_secs: u64,
    pub idle_section_timeout_secs: u64,
    pub target_future_coverage_hours: i64,
}

impl From<EpgDwellConfig> for RemoteEpgDwell {
    fn from(value: EpgDwellConfig) -> Self {
        Self {
            min_dwell_secs: value.min_dwell.as_secs(),
            normal_dwell_secs: value.normal_dwell.as_secs(),
            max_dwell_secs: value.max_dwell.as_secs(),
            idle_section_timeout_secs: value.idle_section_timeout.as_secs(),
            target_future_coverage_hours: value.target_future_coverage_hours,
        }
    }
}

impl From<RemoteEpgDwell> for EpgDwellConfig {
    fn from(value: RemoteEpgDwell) -> Self {
        Self {
            min_dwell: Duration::from_secs(value.min_dwell_secs),
            normal_dwell: Duration::from_secs(value.normal_dwell_secs),
            max_dwell: Duration::from_secs(value.max_dwell_secs),
            idle_section_timeout: Duration::from_secs(value.idle_section_timeout_secs),
            target_future_coverage_hours: value.target_future_coverage_hours,
        }
    }
}

fn default_epg_dwell_secs() -> u64 {
    30
}

/// What the redeeming node sends to `/node/v3/pair`.
#[derive(Debug, Deserialize)]
pub struct PairingRequest {
    /// One-time code the operator copied from the issuing node's dashboard.
    pub code: String,
    /// Who is asking. Becomes a `remote_nodes` row on success.
    pub identity: NodeIdentity,
    /// How the issuing node can reach the caller back.
    #[serde(default)]
    pub endpoints: Vec<NodeEndpoint>,
}

/// Simple fixed-window limiter for the one unauthenticated endpoint.
#[derive(Debug)]
struct PairingAttempts {
    window_started: std::time::Instant,
    failures: u32,
}

impl Default for PairingAttempts {
    fn default() -> Self {
        Self {
            window_started: std::time::Instant::now(),
            failures: 0,
        }
    }
}

impl PairingAttempts {
    fn blocked(&mut self) -> bool {
        if self.window_started.elapsed() > PAIRING_ATTEMPT_WINDOW {
            *self = Self::default();
        }
        self.failures >= PAIRING_ATTEMPT_LIMIT
    }

    fn record_failure(&mut self) {
        if self.window_started.elapsed() > PAIRING_ATTEMPT_WINDOW {
            *self = Self::default();
        }
        self.failures += 1;
    }

    /// A code was actually redeemed, so the failures before it were an
    /// operator mistyping rather than a scan: clear the failure count instead
    /// of leaving the next legitimate pairing locked out.
    ///
    /// The *window* is deliberately left where it was. Restarting it would
    /// mean an attacker who obtains one valid code (or watches a legitimate
    /// pairing happen) gets a fresh full budget of guesses on top of the one
    /// already running.
    fn record_success(&mut self) {
        self.failures = 0;
    }
}

#[derive(Clone)]
pub struct NodeTransportState {
    pub identity: NodeIdentity,
    /// Mutable display name. The node id is immutable for the lifetime of a
    /// process, while the dashboard may rename a node without restarting it.
    pub display_name: Arc<RwLock<String>>,
    pub capabilities: NodeCapabilities,
    pub endpoints: Arc<RwLock<Vec<NodeEndpoint>>>,
    /// Listener address used to derive fresh peer-reachable advertisements.
    /// `None` is retained for unit-test states that do not serve a listener.
    pub node_listen_addr: Option<SocketAddr>,
    pub routes: Arc<RwLock<Vec<ReceptionRouteAdvertisement>>>,
    pub peers: Arc<RwLock<HashMap<NodeId, NodeCredential>>>,
    pub leases: Arc<RemoteLeaseManager>,
    /// Turns peer lease requests into local tuner acquisitions. `None` on a
    /// node that only consumes remote tuners (or in unit tests), in which
    /// case `POST /node/v3/lease` answers 503.
    pub mux_server: Option<Arc<LocalMuxServer>>,
    /// Needed to redeem pairing codes and persist the resulting peer. When
    /// absent (unit tests), `/node/v3/pair` answers 503 instead of pairing
    /// against nothing.
    pub database: Option<DatabaseHandle>,
    pairing_attempts: Arc<std::sync::Mutex<PairingAttempts>>,
}

impl NodeTransportState {
    pub fn new(identity: NodeIdentity, leases: Arc<RemoteLeaseManager>) -> Self {
        Self {
            display_name: Arc::new(RwLock::new(identity.display_name.clone())),
            identity,
            capabilities: NodeCapabilities::default(),
            endpoints: Arc::new(RwLock::new(Vec::new())),
            node_listen_addr: None,
            routes: Arc::new(RwLock::new(Vec::new())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            leases,
            mux_server: None,
            database: None,
            pairing_attempts: Arc::new(std::sync::Mutex::new(PairingAttempts::default())),
        }
    }

    /// Return the current identity advertised to peers.
    pub async fn current_identity(&self) -> NodeIdentity {
        NodeIdentity {
            node_id: self.identity.node_id.clone(),
            display_name: self.display_name.read().await.clone(),
        }
    }

    /// Update the display name advertised by this process immediately.
    pub async fn set_display_name(&self, display_name: String) {
        *self.display_name.write().await = display_name;
    }

    /// Attach the database so this node can accept pairing requests.
    pub fn with_database(mut self, database: DatabaseHandle) -> Self {
        self.database = Some(database);
        self
    }

    /// Attach the derived listener address so hello responses can advertise
    /// actual interface addresses instead of a wildcard bind address.
    pub fn with_node_listen_addr(mut self, node_listen_addr: SocketAddr) -> Self {
        self.node_listen_addr = Some(node_listen_addr);
        self
    }

    pub async fn advertised_endpoints(&self) -> Vec<NodeEndpoint> {
        if let Some(listen_addr) = self.node_listen_addr {
            return super::discovery::discover_advertised_endpoints(listen_addr).await;
        }
        self.endpoints.read().await.clone()
    }

    /// Offer this node's own tuners to peers.
    pub fn with_mux_server(mut self, mux_server: Arc<LocalMuxServer>) -> Self {
        self.mux_server = Some(mux_server);
        self
    }

    /// Load every already-paired peer's credential into the in-memory
    /// authorization map. Called at startup and after a successful pairing.
    pub async fn reload_peers(&self) -> Result<usize, crate::database::DatabaseError> {
        let Some(database) = self.database.as_ref() else {
            return Ok(0);
        };
        let pairs = {
            let db = database.lock().await;
            let store = NodeStore::new(&db)?;
            let mut pairs = Vec::new();
            for node in store.list_nodes()? {
                if !node.enabled {
                    continue;
                }
                if let Some(credential) = store.credential_for(&node.node_id)? {
                    pairs.push((node.node_id, credential));
                }
            }
            pairs
        };
        let count = pairs.len();
        let mut peers = self.peers.write().await;
        peers.clear();
        for (node_id, credential) in pairs {
            peers.insert(node_id, credential);
        }
        Ok(count)
    }

    pub async fn trust_peer(&self, node_id: NodeId, credential: NodeCredential) {
        self.peers.write().await.insert(node_id, credential);
    }

    async fn authorize(&self, headers: &HeaderMap) -> Result<NodeId, StatusCode> {
        let node = headers
            .get("x-recisdb-node-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| NodeId::new(v).ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let peers = self.peers.read().await;
        let credential = peers.get(&node).ok_or(StatusCode::UNAUTHORIZED)?;
        if !credential.matches(token) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(node)
    }
}

pub fn router(state: Arc<NodeTransportState>) -> Router {
    Router::new()
        .route("/node/v3/pair", post(pair))
        .route("/node/v3/peer", delete(unpair_peer))
        .route("/node/v3/hello", get(hello))
        .route("/node/v3/routes", get(routes))
        .route("/node/v3/probe/ping", get(probe_ping))
        .route("/node/v3/probe/download", get(probe_download))
        .route("/node/v3/lease", post(open_lease))
        .route("/node/v3/lease/:id/renew", post(renew_lease))
        .route("/node/v3/lease/:id", delete(release_lease))
        .route("/node/v3/stream/:id", get(stream_lease))
        .route("/node/v3/epg/metadata", post(epg_metadata))
        .with_state(state)
}

/// Remove the authenticated caller from this node's persisted peer list.
/// Pairing rollback and normal node deletion use this to clean up both sides.
async fn unpair_peer(State(state): State<Arc<NodeTransportState>>, headers: HeaderMap) -> Response {
    let Ok(node_id) = state.authorize(&headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(database) = state.database.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let result = {
        let db = database.lock().await;
        NodeStore::new(&db).and_then(|store| store.delete_node(&node_id))
    };
    match result {
        Ok(()) => {
            state.peers.write().await.remove(&node_id);
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(error) => {
            log::error!("Node unpair failed for {node_id}: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Serve the node router on a dedicated TCP listener. Hyper/axum accepts the
/// HTTP/2 prior-knowledge preface on cleartext connections. This mode is for
/// encrypted overlays only; InternetDirect endpoints must use a TLS wrapper.
pub async fn serve_h2c(addr: SocketAddr, state: Arc<NodeTransportState>) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!(
        "Node transport listening on {} (HTTP/2 prior-knowledge / trusted overlay)",
        addr
    );
    axum::serve(listener, router(state)).await
}

/// Redeem a one-time pairing code.
///
/// This is the only node endpoint that is not authenticated by a
/// [`NodeCredential`] — it is how the first credential is established. Being
/// reachable over Tailscale/LAN is deliberately *not* sufficient: without a
/// live, unexpired, unused code the request is rejected.
async fn pair(
    State(state): State<Arc<NodeTransportState>>,
    Json(payload): Json<PairingRequest>,
) -> Response {
    let Some(database) = state.database.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "pairing is not configured on this node",
        )
            .into_response();
    };
    if state.pairing_attempts.lock().unwrap().blocked() {
        // Never say whether the code was wrong or the limiter tripped.
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }

    let Ok(code) = PairingCode::parse(&payload.code) else {
        state.pairing_attempts.lock().unwrap().record_failure();
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if payload.identity.node_id == state.identity.node_id {
        // Costs budget like any other rejected attempt: a branch that answers
        // before the code is checked and *without* counting would be a free
        // way to keep the limiter's window from ever mattering.
        state.pairing_attempts.lock().unwrap().record_failure();
        return (StatusCode::BAD_REQUEST, "a node cannot pair with itself").into_response();
    }

    let credential = NodeCredential::random();
    let peer = StoredNode {
        node_id: payload.identity.node_id.clone(),
        display_name: payload.identity.display_name.clone(),
        site_name: None,
        enabled: true,
        allow_transit: false,
        auto_connect: true,
        last_seen_unix_ms: Some(chrono::Utc::now().timestamp_millis()),
    };

    let stored = {
        let db = database.lock().await;
        let store = match NodeStore::new(&db) {
            Ok(store) => store,
            Err(e) => {
                log::error!("Node pairing: store unavailable: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        match store.consume_pending_pairing(&code) {
            Ok(true) => {}
            Ok(false) => {
                state.pairing_attempts.lock().unwrap().record_failure();
                log::warn!(
                    "Node pairing rejected for {}: no valid pending code",
                    payload.identity.node_id
                );
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Err(e) => {
                log::error!("Node pairing: failed to redeem code: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
        store
            .upsert_node(&peer, Some(&credential))
            .and_then(|()| store.replace_endpoints(&peer.node_id, &payload.endpoints))
    };
    if let Err(e) = stored {
        log::error!("Node pairing: failed to persist peer {}: {e}", peer.node_id);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    state.pairing_attempts.lock().unwrap().record_success();
    state
        .trust_peer(peer.node_id.clone(), credential.clone())
        .await;
    // The credential itself is never logged.
    log::info!(
        "Node pairing accepted: {} ({})",
        peer.node_id,
        peer.display_name
    );

    Json(PairingAcceptance {
        identity: state.current_identity().await,
        credential,
    })
    .into_response()
}

async fn hello(State(state): State<Arc<NodeTransportState>>, headers: HeaderMap) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(NodeHello {
        identity: state.current_identity().await,
        capabilities: state.capabilities.clone(),
        endpoints: state.advertised_endpoints().await,
    })
    .into_response()
}

async fn routes(State(state): State<Arc<NodeTransportState>>, headers: HeaderMap) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.routes.read().await.clone()).into_response()
}

async fn probe_ping(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Query(query): Query<ProbeQuery>,
) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(ProbeReply {
        nonce: query.nonce,
        server_unix_ms: chrono::Utc::now().timestamp_millis(),
    })
    .into_response()
}

async fn probe_download(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Query(query): Query<DownloadProbeQuery>,
) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let len = query.bytes.min(MAX_ACTIVE_PROBE_BYTES);
    let mut response = Response::new(Body::from(vec![0xA5; len]));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    response
}

/// Open a lease on one of this node's tuners for a peer.
///
/// The request goes through the ordinary local arbitration path, carrying the
/// peer's claim verbatim — a remote recording contends with local viewers
/// under the same policy, and priority is never reinterpreted per hop.
async fn open_lease(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Json(payload): Json<OpenLeaseRequest>,
) -> Response {
    let Ok(peer) = state.authorize(&headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(server) = state.mux_server.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this node does not offer local tuners",
        )
            .into_response();
    };

    let mut context = payload.context;
    match server
        .open_lease(&mut context, payload.mux, payload.sid, payload.spent_ms)
        .await
    {
        Ok(lease) => Json(OpenLeaseReply {
            lease_id: lease.id.as_str().to_owned(),
            generation: lease.generation,
            owner_node: lease.owner_node.clone(),
            route_id: lease.route_id.clone(),
            stream_class: lease.stream_class,
            ttl_ms: state.leases.policy().ttl(lease.stream_class).as_millis() as u64,
            context,
        })
        .into_response(),
        // Loop/hop/deadline problems are the caller's request being wrong,
        // not this node failing — and they must never be retried blindly.
        Err(e @ ServeError::Hop(_)) => {
            log::warn!("[node] lease request from {peer} rejected: {e}");
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e @ ServeError::NoRoute(_)) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
        Err(e @ ServeError::MuxLeaseUnavailable(_)) => {
            (StatusCode::CONFLICT, e.to_string()).into_response()
        }
        Err(e @ ServeError::Unavailable(_)) => {
            log::info!("[node] lease request from {peer} could not be served: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response()
        }
        Err(e @ ServeError::Database(_)) => {
            log::error!("[node] lease request from {peer} failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn epg_metadata(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Json(payload): Json<RemoteEpgMetadataRequest>,
) -> Response {
    let Ok(peer) = state.authorize(&headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(server) = state.mux_server.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let mut context = payload.context;
    match server
        .collect_epg_metadata(
            &mut context,
            payload.mux,
            payload.sid,
            payload.spent_ms,
            payload.dwell_secs,
            payload.dwell,
        )
        .await
    {
        Ok(outcome) => Json(RemoteEpgMetadataReply {
            programs: outcome
                .programs
                .into_iter()
                .map(ProgramUpsertWire::from)
                .collect(),
            status: Some(outcome.status.as_str().to_owned()),
            elapsed_secs: Some(outcome.elapsed_secs),
            sections_seen: Some(outcome.sections_seen),
            services_total: Some(outcome.services_total),
            services_complete: Some(outcome.services_complete),
            coverage_until: outcome.coverage_until,
        })
        .into_response(),
        Err(e @ ServeError::MuxLeaseUnavailable(_)) => {
            log::info!("[node] EPG metadata request from {peer} deferred: {e}");
            (StatusCode::CONFLICT, e.to_string()).into_response()
        }
        Err(e @ ServeError::NoRoute(_)) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
        Err(e @ ServeError::Hop(_)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(e) => {
            log::warn!("[node] EPG metadata request from {peer} failed: {e}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn renew_lease(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(id) = RemoteLeaseId::parse(id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let ok = state.leases.renew(&id).await;
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        },
        Json(LeaseReply { ok }),
    )
        .into_response()
}

async fn release_lease(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(id) = RemoteLeaseId::parse(id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let ok = state.leases.release(&id).await.is_some();
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        },
        Json(LeaseReply { ok }),
    )
        .into_response()
}

async fn stream_lease(
    State(state): State<Arc<NodeTransportState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(id) = RemoteLeaseId::parse(id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(lease) = state.leases.get(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let live = lease.subscribe_live();
    let mut replay_frames = Vec::new();
    let mut last_sequence = query.from_seq.map(|seq| seq.saturating_sub(1));

    if let Some(from_seq) = query.from_seq {
        let generation = query.generation.unwrap_or(lease.generation);
        match lease.replay.lock().await.replay_from(generation, from_seq) {
            Ok(frames) => {
                for mut frame in frames {
                    frame.flags = FrameFlags::new(frame.flags.bits() | FrameFlags::REPLAY);
                    last_sequence = Some(frame.sequence);
                    replay_frames.push(frame);
                }
            }
            Err(err) => {
                log::warn!("Node replay unavailable for lease {}: {}", id.as_str(), err);
                return StatusCode::GONE.into_response();
            }
        }
    }

    let class = lease.stream_class;
    let initial = stream::iter(replay_frames.into_iter().map(|frame| {
        frame
            .encode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }));

    struct LiveState {
        rx: tokio::sync::broadcast::Receiver<NodeTsFrame>,
        last_sequence: Option<u64>,
        class: StreamClass,
        discontinuity_pending: bool,
    }

    let live_stream = stream::unfold(
        LiveState {
            rx: live,
            last_sequence,
            class,
            discontinuity_pending: false,
        },
        |mut state| async move {
            loop {
                match state.rx.recv().await {
                    Ok(mut frame) => {
                        if state
                            .last_sequence
                            .is_some_and(|last| frame.sequence <= last)
                        {
                            continue;
                        }
                        if state.discontinuity_pending {
                            frame.flags =
                                FrameFlags::new(frame.flags.bits() | FrameFlags::DISCONTINUITY);
                            state.discontinuity_pending = false;
                        }
                        state.last_sequence = Some(frame.sequence);
                        let encoded = frame
                            .encode()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                        return Some((encoded, state));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        if state.class == StreamClass::Record {
                            let err = io::Error::new(
                                io::ErrorKind::Other,
                                format!("record node stream lagged by {skipped} frame(s)"),
                            );
                            return Some((Err(err), state));
                        }
                        state.discontinuity_pending = true;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    let body_stream = initial.chain(live_stream);
    let mut response = Response::new(Body::from_stream(body_stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/vnd.recisdb.ts-frames"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// Why a lease stream could not be opened. `ReplayGap` and `LeaseGone` are
/// terminal for a RECORD consumer: there is no way to continue without a hole.
#[derive(Debug, thiserror::Error)]
pub enum LeaseStreamError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("replay history no longer covers the requested sequence")]
    ReplayGap,
    #[error("the peer no longer holds this lease")]
    LeaseGone,
    #[error("unexpected status {0}")]
    Status(u16),
}

/// Minimal client used by discovery/path-probe code. HTTPS endpoints use
/// ordinary ALPN negotiation; `http://` endpoints use HTTP/2 prior knowledge
/// and are only valid for trusted encrypted overlays.
pub struct NodeTransportClient {
    identity: NodeId,
    credential: NodeCredential,
    https: reqwest::Client,
    h2c: reqwest::Client,
}

/// TCP connect bound for node-to-node requests (see `NodeTransportClient::new`).
const NODE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl NodeTransportClient {
    pub fn new(identity: NodeId, credential: NodeCredential) -> Result<Self, reqwest::Error> {
        // Without a connect timeout an unreachable endpoint (e.g. an IPv6
        // Tailscale address on a peer listening on 0.0.0.0) blocks each probe
        // request for the OS SYN timeout (~21s on Windows); a 5-sample probe
        // then took ~100s. Streams are long-lived, so only connect is bounded.
        let https = reqwest::Client::builder()
            .connect_timeout(NODE_CONNECT_TIMEOUT)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .build()?;
        let h2c = reqwest::Client::builder()
            .http2_prior_knowledge()
            .connect_timeout(NODE_CONNECT_TIMEOUT)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .build()?;
        Ok(Self {
            identity,
            credential,
            https,
            h2c,
        })
    }

    fn client_for(&self, base: &str) -> &reqwest::Client {
        if base.starts_with("http://") {
            &self.h2c
        } else {
            &self.https
        }
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.client_for(&url)
            .request(method, url)
            .header("x-recisdb-node-id", self.identity.as_str())
            .bearer_auth(self.credential.expose())
    }

    /// Redeem a pairing code at `base`, using an unauthenticated client (no
    /// credential exists yet, by definition).
    pub async fn redeem_pairing_code(
        base: &str,
        code: &PairingCode,
        identity: &NodeIdentity,
        endpoints: &[NodeEndpoint],
    ) -> Result<PairingAcceptance, reqwest::Error> {
        let base = base.trim_end_matches('/');
        let client = if base.starts_with("http://") {
            reqwest::Client::builder()
                .http2_prior_knowledge()
                .connect_timeout(NODE_CONNECT_TIMEOUT)
                .build()?
        } else {
            reqwest::Client::builder()
                .connect_timeout(NODE_CONNECT_TIMEOUT)
                .build()?
        };
        client
            .post(format!("{base}/node/v3/pair"))
            .json(&serde_json::json!({
                "code": code.as_str(),
                "identity": identity,
                "endpoints": endpoints,
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    /// Open a lease on `base`'s tuners.
    pub async fn open_lease(
        &self,
        base: &str,
        request: &OpenLeaseRequest,
    ) -> Result<OpenLeaseReply, reqwest::Error> {
        self.request(
            reqwest::Method::POST,
            format!("{}/node/v3/lease", base.trim_end_matches('/')),
        )
        .json(request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
    }

    /// Ask a paired node to parse EIT locally. Only metadata rows are returned;
    /// no TS endpoint is involved.
    pub async fn collect_epg_metadata(
        &self,
        base: &str,
        request: &RemoteEpgMetadataRequest,
    ) -> Result<RemoteEpgMetadataReply, reqwest::Error> {
        self.request(
            reqwest::Method::POST,
            format!("{}/node/v3/epg/metadata", base.trim_end_matches('/')),
        )
        .json(request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
    }

    /// Keep a lease alive. Returns `false` when the peer no longer has it —
    /// the lease is gone and a RECORD consumer must treat that as failure,
    /// not as something to keep retrying against.
    pub async fn renew_lease(&self, base: &str, lease_id: &str) -> Result<bool, reqwest::Error> {
        let response = self
            .request(
                reqwest::Method::POST,
                format!(
                    "{}/node/v3/lease/{}/renew",
                    base.trim_end_matches('/'),
                    lease_id
                ),
            )
            .send()
            .await?;
        Ok(response.status().is_success())
    }

    pub async fn release_lease(&self, base: &str, lease_id: &str) -> Result<(), reqwest::Error> {
        self.request(
            reqwest::Method::DELETE,
            format!("{}/node/v3/lease/{}", base.trim_end_matches('/'), lease_id),
        )
        .send()
        .await?;
        Ok(())
    }

    /// Remove this node from the peer's persisted pairing records.
    pub async fn unpair_peer(&self, base: &str) -> Result<(), reqwest::Error> {
        self.request(
            reqwest::Method::DELETE,
            format!("{}/node/v3/peer", base.trim_end_matches('/')),
        )
        .send()
        .await?
        .error_for_status()?;
        Ok(())
    }

    /// Open the frame stream for a lease.
    ///
    /// `from_seq` requests lossless resume from the peer's replay buffer.
    /// The peer answers `410 Gone` when that history is no longer available;
    /// callers must surface that rather than silently restarting from live
    /// (`docs/DISTRIBUTED_TUNER_FABRIC.md` §7).
    pub async fn open_lease_stream(
        &self,
        base: &str,
        lease_id: &str,
        generation: Option<u32>,
        from_seq: Option<u64>,
    ) -> Result<reqwest::Response, LeaseStreamError> {
        let mut url = format!("{}/node/v3/stream/{}", base.trim_end_matches('/'), lease_id);
        if let Some(from_seq) = from_seq {
            url.push_str(&format!("?from_seq={from_seq}"));
            if let Some(generation) = generation {
                url.push_str(&format!("&generation={generation}"));
            }
        }
        let response = self
            .request(reqwest::Method::GET, url)
            .send()
            .await
            .map_err(LeaseStreamError::Transport)?;
        // `reqwest` (http 0.2) and `axum` (http 1.x) have distinct
        // `StatusCode` types, so compare numerically rather than importing
        // both under one name.
        match response.status().as_u16() {
            200 => Ok(response),
            410 => Err(LeaseStreamError::ReplayGap),
            404 => Err(LeaseStreamError::LeaseGone),
            other => Err(LeaseStreamError::Status(other)),
        }
    }

    pub async fn hello(&self, base: &str) -> Result<NodeHello, reqwest::Error> {
        self.request(
            reqwest::Method::GET,
            format!("{}/node/v3/hello", base.trim_end_matches('/')),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
    }

    pub async fn routes(
        &self,
        base: &str,
    ) -> Result<Vec<ReceptionRouteAdvertisement>, reqwest::Error> {
        self.request(
            reqwest::Method::GET,
            format!("{}/node/v3/routes", base.trim_end_matches('/')),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
    }

    pub async fn ping(&self, base: &str, nonce: &str) -> Result<ProbeReply, reqwest::Error> {
        let url = format!(
            "{}/node/v3/probe/ping?nonce={}",
            base.trim_end_matches('/'),
            nonce
        );
        self.request(reqwest::Method::GET, url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn probe_download(
        &self,
        base: &str,
        bytes: usize,
    ) -> Result<(usize, std::time::Duration), reqwest::Error> {
        let bytes = bytes.min(MAX_ACTIVE_PROBE_BYTES);
        let url = format!(
            "{}/node/v3/probe/download?bytes={bytes}",
            base.trim_end_matches('/')
        );
        let started = std::time::Instant::now();
        let body = self
            .request(reqwest::Method::GET, url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok((body.len(), started.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_epg_dwell_round_trips_through_epg_dwell_config() {
        let config = EpgDwellConfig {
            min_dwell: Duration::from_secs(7),
            normal_dwell: Duration::from_secs(19),
            max_dwell: Duration::from_secs(41),
            idle_section_timeout: Duration::from_secs(5),
            target_future_coverage_hours: 36,
        };
        let round_trip: EpgDwellConfig = RemoteEpgDwell::from(config).into();
        assert_eq!(round_trip, config);
    }

    #[test]
    fn remote_epg_metadata_request_without_dwell_deserializes() {
        let json = serde_json::json!({
            "context": {
                "request_id": "r", "trace_id": "t", "stream_class": "View",
                "claim": {"priority": -1000, "exclusive": false},
                "remaining_ms": 1000, "origin_node": "node-a"
            },
            "mux": {"nid": 1, "tsid": 2}
        });
        let request: RemoteEpgMetadataRequest = serde_json::from_value(json).unwrap();
        assert_eq!(request.dwell, None);
        assert_eq!(request.dwell_secs, 30);
    }
    use crate::node::{LeasePolicy, LogicalMuxId};
    use crate::tuner::EffectiveClaim;

    #[tokio::test]
    async fn auth_rejects_wrong_peer_token() {
        let state = Arc::new(NodeTransportState::new(
            NodeIdentity {
                node_id: NodeId::new("site-b").unwrap(),
                display_name: "拠点B".into(),
            },
            Arc::new(RemoteLeaseManager::new(LeasePolicy::default())),
        ));
        let peer = NodeId::new("site-a").unwrap();
        state
            .trust_peer(peer.clone(), NodeCredential::random())
            .await;

        let mut headers = HeaderMap::new();
        headers.insert("x-recisdb-node-id", peer.as_str().parse().unwrap());
        headers.insert(
            header::AUTHORIZATION,
            "Bearer definitely-wrong".parse().unwrap(),
        );
        assert_eq!(
            state.authorize(&headers).await,
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    /// The client speaks HTTP/2 prior-knowledge; the listener must accept it
    /// over a real socket. Without axum's `http2` feature the server only
    /// spoke HTTP/1.1 and every node request was reset (in-process router
    /// tests never noticed).
    #[tokio::test]
    async fn listener_accepts_http2_prior_knowledge_over_tcp() {
        let state = Arc::new(NodeTransportState::new(
            NodeIdentity {
                node_id: NodeId::new("site-b").unwrap(),
                display_name: "拠点B".into(),
            },
            Arc::new(RemoteLeaseManager::new(LeasePolicy::default())),
        ));
        let addr = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap()
        };
        let server = tokio::spawn(serve_h2c(addr, state));
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let url = format!("http://{addr}/node/v3/hello");
        let mut status = None;
        for _ in 0..50 {
            match client.get(&url).send().await {
                Ok(resp) => {
                    status = Some(resp.status().as_u16());
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
        server.abort();
        // Unauthenticated hello: the point is that an h2c request got an
        // HTTP answer at all instead of a connection reset.
        assert_eq!(status, Some(StatusCode::UNAUTHORIZED.as_u16()));
    }

    fn test_state_with_db() -> (Arc<NodeTransportState>, DatabaseHandle) {
        let db = crate::database::Database::open_in_memory().unwrap();
        let database: DatabaseHandle = Arc::new(tokio::sync::Mutex::new(db));
        let state = Arc::new(
            NodeTransportState::new(
                NodeIdentity {
                    node_id: NodeId::new("site-a").unwrap(),
                    display_name: "拠点A".into(),
                },
                Arc::new(RemoteLeaseManager::new(LeasePolicy::default())),
            )
            .with_database(database.clone()),
        );
        (state, database)
    }

    fn peer_request(code: &str) -> PairingRequest {
        PairingRequest {
            code: code.to_string(),
            identity: NodeIdentity {
                node_id: NodeId::new("site-c").unwrap(),
                display_name: "拠点C".into(),
            },
            endpoints: vec![NodeEndpoint::direct("http://site-c.example.com:20773")],
        }
    }

    #[test]
    fn old_hello_without_endpoints_deserializes() {
        let value = serde_json::json!({
            "identity": {
                "node_id": "old-node",
                "display_name": "旧ノード"
            },
            "capabilities": NodeCapabilities::default(),
        });
        let hello: NodeHello = serde_json::from_value(value).unwrap();
        assert!(hello.endpoints.is_empty());
    }

    /// Reachability is not authentication: without a live code the request is
    /// rejected even though it arrived on the trusted overlay listener.
    #[tokio::test]
    async fn pairing_without_a_pending_code_is_rejected() {
        let (state, _db) = test_state_with_db();
        let code = PairingCode::random();

        let response = pair(State(Arc::clone(&state)), Json(peer_request(code.as_str()))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(state.peers.read().await.is_empty());
    }

    #[tokio::test]
    async fn pairing_redeems_once_and_trusts_the_peer() {
        let (state, database) = test_state_with_db();
        let code = PairingCode::random();
        {
            let db = database.lock().await;
            NodeStore::new(&db)
                .unwrap()
                .create_pending_pairing(
                    &code,
                    None,
                    chrono::Utc::now().timestamp_millis() + 600_000,
                )
                .unwrap();
        }

        let response = pair(State(Arc::clone(&state)), Json(peer_request(code.as_str()))).await;
        assert_eq!(response.status(), StatusCode::OK);

        let peer = NodeId::new("site-c").unwrap();
        let credential = state
            .peers
            .read()
            .await
            .get(&peer)
            .cloned()
            .expect("paired peer must be trusted immediately, without a restart");
        {
            let db = database.lock().await;
            let store = NodeStore::new(&db).unwrap();
            assert_eq!(store.credential_for(&peer).unwrap().unwrap(), credential);
            assert_eq!(store.endpoints(&peer).unwrap().len(), 1);
            assert_eq!(
                store.endpoints(&peer).unwrap()[0].address,
                "http://site-c.example.com:20773"
            );
        }

        // Replaying the same code must not pair anything else.
        let replay = pair(State(Arc::clone(&state)), Json(peer_request(code.as_str()))).await;
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    }

    /// `/node/v3/pair` is the one unauthenticated endpoint, so guessing the
    /// code is the attack it has to survive. The fixed window caps how many
    /// guesses a scanner gets, and a wrong guess is answered exactly like a
    /// stale one (never "the code was wrong" vs "you are rate limited").
    #[tokio::test]
    async fn pairing_locks_out_after_repeated_wrong_codes() {
        let (state, _database) = test_state_with_db();

        for _ in 0..PAIRING_ATTEMPT_LIMIT {
            let guess = PairingCode::random();
            assert_eq!(
                pair(
                    State(Arc::clone(&state)),
                    Json(peer_request(guess.as_str()))
                )
                .await
                .status(),
                StatusCode::UNAUTHORIZED
            );
        }

        let guess = PairingCode::random();
        assert_eq!(
            pair(
                State(Arc::clone(&state)),
                Json(peer_request(guess.as_str()))
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS,
            "a scanner must be cut off, not merely told 'wrong' forever"
        );
    }

    /// Malformed codes must count against the same budget: otherwise the
    /// limiter is trivially bypassed by sending garbage in between guesses.
    #[tokio::test]
    async fn malformed_pairing_codes_count_towards_the_lockout() {
        let (state, _database) = test_state_with_db();

        for _ in 0..PAIRING_ATTEMPT_LIMIT {
            assert_eq!(
                pair(State(Arc::clone(&state)), Json(peer_request("not-a-code")))
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            pair(State(Arc::clone(&state)), Json(peer_request("not-a-code")))
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    /// Failures before a *successful* pairing were an operator mistyping the
    /// code, not a scan: pairing a second node right after must not be locked
    /// out by them.
    #[tokio::test]
    async fn a_successful_pairing_clears_earlier_failures() {
        let (state, database) = test_state_with_db();

        for _ in 0..(PAIRING_ATTEMPT_LIMIT - 1) {
            let guess = PairingCode::random();
            let _ = pair(
                State(Arc::clone(&state)),
                Json(peer_request(guess.as_str())),
            )
            .await;
        }

        let code = PairingCode::random();
        {
            let db = database.lock().await;
            NodeStore::new(&db)
                .unwrap()
                .create_pending_pairing(
                    &code,
                    None,
                    chrono::Utc::now().timestamp_millis() + 600_000,
                )
                .unwrap();
        }
        assert_eq!(
            pair(State(Arc::clone(&state)), Json(peer_request(code.as_str())))
                .await
                .status(),
            StatusCode::OK
        );

        // The budget is back: the next wrong code is a plain 401, not a 429.
        let guess = PairingCode::random();
        assert_eq!(
            pair(
                State(Arc::clone(&state)),
                Json(peer_request(guess.as_str()))
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn reload_peers_restores_credentials_after_a_restart() {
        let (state, database) = test_state_with_db();
        let code = PairingCode::random();
        {
            let db = database.lock().await;
            NodeStore::new(&db)
                .unwrap()
                .create_pending_pairing(
                    &code,
                    None,
                    chrono::Utc::now().timestamp_millis() + 600_000,
                )
                .unwrap();
        }
        assert_eq!(
            pair(State(Arc::clone(&state)), Json(peer_request(code.as_str())))
                .await
                .status(),
            StatusCode::OK
        );

        // A "restarted" node sharing the same database.
        let restarted = Arc::new(
            NodeTransportState::new(
                state.identity.clone(),
                Arc::new(RemoteLeaseManager::new(LeasePolicy::default())),
            )
            .with_database(database.clone()),
        );
        assert_eq!(restarted.reload_peers().await.unwrap(), 1);

        let peer = NodeId::new("site-c").unwrap();
        let credential = restarted.peers.read().await.get(&peer).cloned().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-recisdb-node-id", peer.as_str().parse().unwrap());
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", credential.expose()).parse().unwrap(),
        );
        assert_eq!(restarted.authorize(&headers).await, Ok(peer));
    }

    #[tokio::test]
    async fn reload_peers_does_not_restore_disabled_nodes() {
        let (state, database) = test_state_with_db();
        let code = PairingCode::random();
        {
            let db = database.lock().await;
            NodeStore::new(&db)
                .unwrap()
                .create_pending_pairing(
                    &code,
                    None,
                    chrono::Utc::now().timestamp_millis() + 600_000,
                )
                .unwrap();
        }
        assert_eq!(
            pair(State(Arc::clone(&state)), Json(peer_request(code.as_str())))
                .await
                .status(),
            StatusCode::OK
        );
        {
            let db = database.lock().await;
            let store = NodeStore::new(&db).unwrap();
            store
                .set_node_enabled(&NodeId::new("site-c").unwrap(), false)
                .unwrap();
        }
        let restarted = Arc::new(
            NodeTransportState::new(
                state.identity.clone(),
                Arc::new(RemoteLeaseManager::new(LeasePolicy::default())),
            )
            .with_database(database),
        );
        assert_eq!(restarted.reload_peers().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn record_live_lag_contract_is_replay_capable() {
        let leases = Arc::new(RemoteLeaseManager::new(LeasePolicy::default()));
        let lease = leases
            .create(
                NodeId::new("site-a").unwrap(),
                "r".into(),
                LogicalMuxId { nid: 1, tsid: 1 },
                None,
                StreamClass::Record,
                EffectiveClaim::new(2, false),
                1,
            )
            .await;
        assert_eq!(lease.stream_class, StreamClass::Record);
        assert!(lease
            .replay
            .lock()
            .await
            .replay_from(1, 0)
            .unwrap()
            .is_empty());
    }
}
