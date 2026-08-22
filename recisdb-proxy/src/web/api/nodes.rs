//! Distributed node/fabric management API.
//!
//! These endpoints are dashboard-facing and therefore live under the normal
//! authenticated `/api/*` namespace.  The actual inter-node transport uses a
//! separate HTTP/2 listener/namespace (`node::transport`) and never shares the
//! dashboard bearer token.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use recisdb_protocol::StreamClass;
use serde::Deserialize;
use serde_json::json;

use crate::node::{
    probe_endpoint, select_best_path, NodeCredential, NodeEndpoint, NodeId,
    NodeStore, NodeTransportClient, PathPolicy, ProbeConfig, StoredNode,
};
use crate::web::state::WebState;

use super::error::ApiError;

#[derive(Debug, Deserialize)]
pub struct UpsertNodeRequest {
    pub node_id: String,
    pub display_name: String,
    pub site_name: Option<String>,
    pub enabled: Option<bool>,
    pub allow_transit: Option<bool>,
    pub auto_connect: Option<bool>,
    /// Advanced/manual pairing only.  Normal users should use the one-time
    /// pairing-code flow added by the NodeTransport pairing endpoint.
    pub credential: Option<String>,
    #[serde(default)]
    pub endpoints: Vec<NodeEndpoint>,
}

#[derive(Debug, Deserialize)]
pub struct ProbeNodeRequest {
    /// Approximate payload bitrate used for path admission. 20 Mbps is a
    /// conservative HD/full-TS default when the UI does not know the stream.
    pub bitrate_bps: Option<u64>,
    /// Active throughput probe size. Capped again in NodeTransport.
    pub download_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct RouteGroupMemberRequest {
    pub name: String,
    pub node_id: String,
    pub weight: Option<i32>,
}

/// Full fabric snapshot used by the Nodes dashboard tab.
pub async fn get_nodes(
    State(web_state): State<Arc<WebState>>,
) -> Result<impl IntoResponse, ApiError> {
    let db = web_state.database.lock().await;
    let store = NodeStore::new(&db)?;
    let local = store.local_identity()?;
    let nodes = store.list_nodes()?;
    let mut entries = Vec::with_capacity(nodes.len());
    for node in nodes {
        let endpoints = store.endpoints(&node.node_id)?;
        entries.push(json!({
            "node": node,
            "endpoints": endpoints,
            // Never expose the shared credential back to the browser.
            "paired": store.credential_for(&node.node_id)?.is_some(),
        }));
    }
    let route_groups = store.list_route_groups()?;

    Ok(Json(json!({
        "success": true,
        "local": local,
        "nodes": entries,
        "route_groups": route_groups,
    })))
}

/// Add or edit one remote node and atomically replace its endpoint list.
pub async fn upsert_node(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpsertNodeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let node_id = NodeId::new(payload.node_id)
        .map_err(ApiError::bad_request)?;
    let display_name = payload.display_name.trim();
    if display_name.is_empty() {
        return Err(ApiError::bad_request("display_name must not be empty"));
    }
    for endpoint in &payload.endpoints {
        if endpoint.address.trim().is_empty() {
            return Err(ApiError::bad_request("endpoint address must not be empty"));
        }
        if !endpoint.address.starts_with("http://") && !endpoint.address.starts_with("https://") {
            return Err(ApiError::bad_request(
                "endpoint address must start with http:// or https://",
            ));
        }
    }

    let credential = payload
        .credential
        .map(NodeCredential::parse)
        .transpose()
        .map_err(ApiError::bad_request)?;
    let node = StoredNode {
        node_id: node_id.clone(),
        display_name: display_name.to_owned(),
        site_name: payload.site_name.filter(|s| !s.trim().is_empty()),
        enabled: payload.enabled.unwrap_or(true),
        allow_transit: payload.allow_transit.unwrap_or(false),
        auto_connect: payload.auto_connect.unwrap_or(true),
        last_seen_unix_ms: None,
    };

    let db = web_state.database.lock().await;
    let store = NodeStore::new(&db)?;
    store.upsert_node(&node, credential.as_ref())?;
    store.replace_endpoints(&node_id, &payload.endpoints)?;

    Ok(Json(json!({ "success": true, "node": node })))
}

/// Actively test every enabled transport path for one node.  The probe result
/// reports both VIEW and RECORD choices because their scoring differs by
/// design: live viewing values latency, recording values stalls/reconnects and
/// conservative p10 bandwidth much more heavily.
pub async fn probe_node(
    State(web_state): State<Arc<WebState>>,
    Path(node_id): Path<String>,
    Json(payload): Json<ProbeNodeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let node_id = NodeId::new(node_id).map_err(ApiError::bad_request)?;
    let (local, credential, endpoints) = {
        let db = web_state.database.lock().await;
        let store = NodeStore::new(&db)?;
        let exists = store.list_nodes()?.iter().any(|n| n.node_id == node_id);
        if !exists {
            return Err(ApiError::not_found(format!("node {node_id} not found")));
        }
        let local = store.local_identity()?;
        let credential = store
            .credential_for(&node_id)?
            .ok_or_else(|| ApiError::conflict("node is not paired; no application credential is stored"))?;
        let endpoints = store.endpoints(&node_id)?;
        (local, credential, endpoints)
    };

    let client = NodeTransportClient::new(local.node_id, credential)
        .map_err(|e| ApiError::internal(format!("failed to create node client: {e}")))?;
    let config = ProbeConfig {
        ping_samples: 3,
        download_samples: 2,
        download_bytes: payload.download_bytes.unwrap_or(1024 * 1024),
        command_timeout: Duration::from_secs(3),
    };

    let mut paths = Vec::new();
    for endpoint in endpoints.into_iter().filter(|e| e.enabled) {
        paths.push(probe_endpoint(&client, endpoint, config.clone()).await);
    }

    let bitrate = payload.bitrate_bps.unwrap_or(20_000_000);
    let policy = PathPolicy::default();
    let selected_view = select_best_path(&paths, StreamClass::View, bitrate, policy)
        .map(|p| p.id.clone());
    let selected_preview = select_best_path(&paths, StreamClass::Preview, bitrate, policy)
        .map(|p| p.id.clone());
    let selected_record = select_best_path(&paths, StreamClass::Record, bitrate, policy)
        .map(|p| p.id.clone());

    Ok(Json(json!({
        "success": true,
        "bitrate_bps": bitrate,
        "paths": paths,
        "selected": {
            "view": selected_view,
            "preview": selected_preview,
            "record": selected_record,
        }
    })))
}

/// Create a route group if needed and add/update one member. This intentionally
/// makes the common "関東グループにこのノードを追加" operation one request.
pub async fn set_route_group_member(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<RouteGroupMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let name = payload.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("route group name must not be empty"));
    }
    let node_id = NodeId::new(payload.node_id).map_err(ApiError::bad_request)?;
    let weight = payload.weight.unwrap_or(100).clamp(1, 10_000);

    let db = web_state.database.lock().await;
    let store = NodeStore::new(&db)?;
    if !store.list_nodes()?.iter().any(|n| n.node_id == node_id) {
        return Err(ApiError::not_found(format!("node {node_id} not found")));
    }
    let group_id = store.ensure_route_group(name)?;
    store.set_group_member(group_id, &node_id, weight)?;

    Ok(Json(json!({
        "success": true,
        "group_id": group_id,
        "name": name,
        "node_id": node_id,
        "weight": weight,
    })))
}
