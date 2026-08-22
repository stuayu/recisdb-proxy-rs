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
use super::identity::{NodeCredential, NodeIdentity};
use super::lease::{RemoteLeaseId, RemoteLeaseManager};
use super::types::{NodeEndpoint, NodeId, ReceptionRouteAdvertisement};

pub const NODE_PROTOCOL_VERSION: u16 = 3;
pub const MAX_ACTIVE_PROBE_BYTES: usize = 16 * 1024 * 1024;

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

#[derive(Clone)]
pub struct NodeTransportState {
    pub identity: NodeIdentity,
    pub capabilities: NodeCapabilities,
    pub endpoints: Arc<RwLock<Vec<NodeEndpoint>>>,
    pub routes: Arc<RwLock<Vec<ReceptionRouteAdvertisement>>>,
    pub peers: Arc<RwLock<HashMap<NodeId, NodeCredential>>>,
    pub leases: Arc<RemoteLeaseManager>,
}

impl NodeTransportState {
    pub fn new(identity: NodeIdentity, leases: Arc<RemoteLeaseManager>) -> Self {
        Self {
            identity,
            capabilities: NodeCapabilities::default(),
            endpoints: Arc::new(RwLock::new(Vec::new())),
            routes: Arc::new(RwLock::new(Vec::new())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            leases,
        }
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
        .route("/node/v3/hello", get(hello))
        .route("/node/v3/routes", get(routes))
        .route("/node/v3/probe/ping", get(probe_ping))
        .route("/node/v3/probe/download", get(probe_download))
        .route("/node/v3/lease/:id/renew", post(renew_lease))
        .route("/node/v3/lease/:id", delete(release_lease))
        .route("/node/v3/stream/:id", get(stream_lease))
        .with_state(state)
}

/// Serve the node router on a dedicated TCP listener. Hyper/axum accepts the
/// HTTP/2 prior-knowledge preface on cleartext connections. This mode is for
/// encrypted overlays only; InternetDirect endpoints must use a TLS wrapper.
pub async fn serve_h2c(addr: SocketAddr, state: Arc<NodeTransportState>) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Node transport listening on {} (HTTP/2 prior-knowledge / trusted overlay)", addr);
    axum::serve(listener, router(state)).await
}

async fn hello(State(state): State<Arc<NodeTransportState>>, headers: HeaderMap) -> Response {
    if state.authorize(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(NodeHello {
        identity: state.identity.clone(),
        capabilities: state.capabilities.clone(),
        endpoints: state.endpoints.read().await.clone(),
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
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/octet-stream"));
    response
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
    (if ok { StatusCode::OK } else { StatusCode::NOT_FOUND }, Json(LeaseReply { ok })).into_response()
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
    (if ok { StatusCode::OK } else { StatusCode::NOT_FOUND }, Json(LeaseReply { ok })).into_response()
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
                        if state.last_sequence.is_some_and(|last| frame.sequence <= last) {
                            continue;
                        }
                        if state.discontinuity_pending {
                            frame.flags = FrameFlags::new(frame.flags.bits() | FrameFlags::DISCONTINUITY);
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

/// Minimal client used by discovery/path-probe code. HTTPS endpoints use
/// ordinary ALPN negotiation; `http://` endpoints use HTTP/2 prior knowledge
/// and are only valid for trusted encrypted overlays.
pub struct NodeTransportClient {
    identity: NodeId,
    credential: NodeCredential,
    https: reqwest::Client,
    h2c: reqwest::Client,
}

impl NodeTransportClient {
    pub fn new(identity: NodeId, credential: NodeCredential) -> Result<Self, reqwest::Error> {
        let https = reqwest::Client::builder()
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .build()?;
        let h2c = reqwest::Client::builder()
            .http2_prior_knowledge()
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

    pub async fn hello(&self, base: &str) -> Result<NodeHello, reqwest::Error> {
        self.request(reqwest::Method::GET, format!("{}/node/v3/hello", base.trim_end_matches('/')))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn routes(&self, base: &str) -> Result<Vec<ReceptionRouteAdvertisement>, reqwest::Error> {
        self.request(reqwest::Method::GET, format!("{}/node/v3/routes", base.trim_end_matches('/')))
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

    pub async fn probe_download(&self, base: &str, bytes: usize) -> Result<(usize, std::time::Duration), reqwest::Error> {
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
    use crate::node::{LeasePolicy, LogicalMuxId};
    use crate::tuner::EffectiveClaim;

    #[tokio::test]
    async fn auth_rejects_wrong_peer_token() {
        let state = Arc::new(NodeTransportState::new(
            NodeIdentity {
                node_id: NodeId::new("gunma").unwrap(),
                display_name: "群馬".into(),
            },
            Arc::new(RemoteLeaseManager::new(LeasePolicy::default())),
        ));
        let peer = NodeId::new("fukushima").unwrap();
        state.trust_peer(peer.clone(), NodeCredential::random()).await;

        let mut headers = HeaderMap::new();
        headers.insert("x-recisdb-node-id", peer.as_str().parse().unwrap());
        headers.insert(header::AUTHORIZATION, "Bearer definitely-wrong".parse().unwrap());
        assert_eq!(state.authorize(&headers).await, Err(StatusCode::UNAUTHORIZED));
    }

    #[tokio::test]
    async fn record_live_lag_contract_is_replay_capable() {
        let leases = Arc::new(RemoteLeaseManager::new(LeasePolicy::default()));
        let lease = leases
            .create(
                NodeId::new("fukushima").unwrap(),
                "r".into(),
                LogicalMuxId { nid: 1, tsid: 1 },
                None,
                StreamClass::Record,
                EffectiveClaim::new(2, false),
                1,
            )
            .await;
        assert_eq!(lease.stream_class, StreamClass::Record);
        assert!(lease.replay.lock().await.replay_from(1, 0).unwrap().is_empty());
    }
}
