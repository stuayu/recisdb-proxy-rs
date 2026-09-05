//! Distributed node/fabric management API.
//!
//! These endpoints are dashboard-facing and therefore live under the normal
//! authenticated `/api/*` namespace.  The actual inter-node transport uses a
//! separate HTTP/2 listener/namespace (`node::transport`) and never shares the
//! dashboard bearer token.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
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
    probe_endpoint, select_best_path, NodeCredential, NodeEndpoint, NodeId, NodeStore,
    NodeTransportClient, PairingCode, PathPolicy, ProbeConfig, StoredNode, PAIRING_CODE_TTL,
};
use crate::web::state::WebState;

use super::error::ApiError;

fn probe_cache() -> &'static Mutex<HashMap<String, Vec<serde_json::Value>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<serde_json::Value>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

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
pub struct IssuePairingRequest {
    /// Free-form note shown next to the pending code ("東京の受信機" etc.).
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLocalNodeRequest {
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateNodeStateRequest {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct RedeemPairingRequest {
    /// Base URL of the *issuing* node's transport listener, e.g.
    /// `http://tokyo.tailnet.ts.net:20773`.
    pub base_url: String,
    /// The one-time code shown on that node's dashboard.
    pub code: String,
    /// Endpoints this node advertises back, so the peer can reach us.
    #[serde(default)]
    pub endpoints: Vec<NodeEndpoint>,
}

#[derive(Debug, Deserialize)]
pub struct RouteGroupMemberRequest {
    pub name: String,
    pub node_id: String,
    pub weight: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct RouteGroupRequest {
    pub id: Option<i64>,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoveRouteGroupMemberRequest {
    pub group_id: i64,
    pub node_id: String,
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
    for node in nodes.iter().cloned() {
        let endpoints = store.endpoints(&node.node_id)?;
        // Both numbers: "12 routes, 0 usable" and "no routes at all" are
        // different problems and the operator has to tell them apart.
        let (routable_routes, total_routes) = store.remote_route_counts(&node.node_id)?;
        entries.push(json!({
            "node": node,
            "endpoints": endpoints,
            // Never expose the shared credential back to the browser.
            "paired": store.credential_for(&node.node_id)?.is_some(),
            "routable_routes": routable_routes,
            "total_routes": total_routes,
        }));
    }
    let route_groups = store
        .list_route_groups()?
        .into_iter()
        .map(|group| {
            let members = store
                .route_group_members(group.id)?
                .into_iter()
                .map(|(node_id, weight)| json!({ "node_id": node_id, "weight": weight }))
                .collect::<Vec<_>>();
            Ok(json!({ "id": group.id, "name": group.name, "members": members }))
        })
        .collect::<Result<Vec<_>, crate::database::DatabaseError>>()?;
    let setup_status = vec![
        json!({ "id": "local", "label": "このPCの設定", "state": "done", "action": null }),
        json!({ "id": "pairing", "label": "相手PCとの接続", "state": if entries.iter().all(|entry| entry["paired"] == true) && !entries.is_empty() { "done" } else { "todo" }, "action": "wizard" }),
        json!({ "id": "probe", "label": "通信テスト", "state": "todo", "action": "probe" }),
        json!({ "id": "record", "label": "録画利用設定", "state": if entries.iter().any(|entry| entry["paired"] == true) { "done" } else { "todo" }, "action": "settings" }),
        json!({ "id": "area", "label": "受信エリア設定", "state": if route_groups.is_empty() { "todo" } else { "done" }, "action": "area" }),
    ];
    let local_node_id = local.node_id.clone();
    let topology_paths = entries
        .iter()
        .flat_map(|entry| {
            let node = entry["node"]["node_id"].clone();
            let node_key = node.as_str().unwrap_or_default().to_owned();
            let paired = entry["paired"] == true;
            entry["endpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map({
                    let from = local_node_id.clone();
                    let measured_paths = probe_cache()
                        .lock()
                        .ok()
                        .and_then(|cache| cache.get(&node_key).cloned())
                        .unwrap_or_default();
                    move |(index, endpoint)| {
                        let measured = measured_paths.get(index).cloned();
                        let status = measured
                            .as_ref()
                            .and_then(|path| path["health"]["state"].as_str())
                            .map(|state| match state {
                                "healthy" => "online",
                                "degraded" => "degraded",
                                _ => "unavailable",
                            })
                            .unwrap_or(if paired { "unmeasured" } else { "unavailable" });
                        json!({
                            "from": from.clone(),
                            "to": node.clone(),
                            "kind": endpoint["kind"],
                            "status": status,
                            "role": if index == 0 { "primary" } else { "fallback" },
                            "record_allowed": endpoint["record_allowed"],
                            "measured": measured.is_some(),
                            "health": measured.map(|path| path["health"].clone()),
                        })
                    }
                })
        })
        .collect::<Vec<_>>();
    // Only the expiry is knowable: the code itself was stored as a digest.
    let pending_pairings = store.pending_pairings()?;

    let topology_local = local.clone();
    Ok(Json(json!({
        "success": true,
        "local": local,
        "nodes": entries,
        "route_groups": route_groups,
        "setup_status": setup_status,
        "topology": { "local": topology_local, "nodes": entries.iter().map(|entry| entry["node"].clone()).collect::<Vec<_>>(), "paths": topology_paths },
        "pending_pairings": pending_pairings,
    })))
}

/// Update this node's peer-visible display name without editing TOML or
/// restarting the process.
pub async fn update_local_node(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateLocalNodeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let display_name = payload.display_name.trim();
    if display_name.is_empty() {
        return Err(ApiError::bad_request("display_name must not be empty"));
    }
    let identity = {
        let db = web_state.database.lock().await;
        let store = NodeStore::new(&db)?;
        let mut identity = store.local_identity()?;
        identity.display_name = display_name.to_owned();
        store.update_local_identity(&identity, web_state.node_listen_addr.as_deref())?;
        identity
    };
    if let Some(state) = web_state.node_transport.as_ref() {
        state.set_display_name(identity.display_name.clone()).await;
    }
    Ok(Json(json!({ "success": true, "local": identity })))
}

/// Enable or suspend a paired node while retaining its route and credential
/// records for later use.
pub async fn update_node_state(
    State(web_state): State<Arc<WebState>>,
    Path(node_id): Path<String>,
    Json(payload): Json<UpdateNodeStateRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let node_id = NodeId::new(node_id).map_err(ApiError::bad_request)?;
    let credential = {
        let db = web_state.database.lock().await;
        let store = NodeStore::new(&db)?;
        if !store
            .list_nodes()?
            .iter()
            .any(|node| node.node_id == node_id)
        {
            return Err(ApiError::not_found(format!("node {node_id} not found")));
        }
        store.set_node_enabled(&node_id, payload.enabled)?;
        if payload.enabled {
            store.credential_for(&node_id)?
        } else {
            None
        }
    };
    // `enabled` is an admission control switch, not just a display flag:
    // remove disabled peers from the live authorization map so inbound lease
    // requests stop immediately. Reinsert the persisted credential on enable.
    if let Some(state) = web_state.node_transport.as_ref() {
        if let Some(credential) = credential {
            state.trust_peer(node_id.clone(), credential).await;
        } else if !payload.enabled {
            state.peers.write().await.remove(&node_id);
        }
    }
    Ok(Json(
        json!({ "success": true, "node_id": node_id, "enabled": payload.enabled }),
    ))
}

/// Remove a remote node, its credentials, endpoints and cached route data.
pub async fn delete_node(
    State(web_state): State<Arc<WebState>>,
    Path(node_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let node_id = NodeId::new(node_id).map_err(ApiError::bad_request)?;
    let (local, credential, endpoints) = {
        let db = web_state.database.lock().await;
        let store = NodeStore::new(&db)?;
        if !store
            .list_nodes()?
            .iter()
            .any(|node| node.node_id == node_id)
        {
            return Err(ApiError::not_found(format!("node {node_id} not found")));
        }
        (
            store.local_identity()?,
            store.credential_for(&node_id)?,
            store.endpoints(&node_id)?,
        )
    };
    let mut reciprocal_removed = false;
    if let Some(credential) = credential {
        if let Ok(client) = NodeTransportClient::new(local.node_id, credential) {
            for endpoint in endpoints.iter().filter(|endpoint| endpoint.enabled) {
                if client.unpair_peer(&endpoint.address).await.is_ok() {
                    reciprocal_removed = true;
                    break;
                }
            }
        }
    }
    {
        let db = web_state.database.lock().await;
        NodeStore::new(&db)?.delete_node(&node_id)?;
    }
    if let Some(state) = web_state.node_transport.as_ref() {
        state.peers.write().await.remove(&node_id);
    }
    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "reciprocal_removed": reciprocal_removed,
        "warning": if reciprocal_removed { serde_json::Value::Null } else { json!("相手側の設定を確認してください") },
    })))
}

/// Issue a one-time pairing code for another node to redeem.
///
/// The plaintext code is returned **once, here**. Only its SHA-256 is stored,
/// so it cannot be shown again — reissue instead.
pub async fn issue_pairing_code(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<IssuePairingRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let label = payload
        .label
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty());
    let code = PairingCode::random();
    let expires_at_unix_ms =
        chrono::Utc::now().timestamp_millis() + PAIRING_CODE_TTL.as_millis() as i64;

    let (local, listen_hint) = {
        let db = web_state.database.lock().await;
        let store = NodeStore::new(&db)?;
        store.create_pending_pairing(&code, label.as_deref(), expires_at_unix_ms)?;
        (store.local_identity()?, web_state.node_listen_addr.clone())
    };

    Ok(Json(json!({
        "success": true,
        // Shown once; the server keeps only the digest.
        "code": code.as_str(),
        "expires_at_unix_ms": expires_at_unix_ms,
        "ttl_secs": PAIRING_CODE_TTL.as_secs(),
        "label": label,
        "local": local,
        "node_listen_addr": listen_hint,
    })))
}

/// Redeem a code issued by another node, establishing the shared credential.
///
/// The credential the peer returns is stored but never echoed back to the
/// browser (`GET /api/nodes` reports only `paired: true/false`).
pub async fn redeem_pairing_code(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<RedeemPairingRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let base_url = payload.base_url.trim().trim_end_matches('/').to_owned();
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://",
        ));
    }
    let code = PairingCode::parse(&payload.code).map_err(ApiError::bad_request)?;
    for endpoint in &payload.endpoints {
        if endpoint.address.trim().is_empty() {
            return Err(ApiError::bad_request("endpoint address must not be empty"));
        }
    }

    let local = {
        let db = web_state.database.lock().await;
        NodeStore::new(&db)?.local_identity()?
    };

    let acceptance =
        NodeTransportClient::redeem_pairing_code(&base_url, &code, &local, &payload.endpoints)
            .await
            .map_err(|e| {
                // `e` never contains the code (it is sent in the JSON body).
                ApiError::bad_gateway(format!("pairing request to {base_url} failed: {e}"))
            })?;

    if acceptance.identity.node_id == local.node_id {
        return Err(ApiError::bad_request("that endpoint is this node itself"));
    }

    let peer = StoredNode {
        node_id: acceptance.identity.node_id.clone(),
        display_name: acceptance.identity.display_name.clone(),
        site_name: None,
        enabled: true,
        allow_transit: false,
        auto_connect: true,
        last_seen_unix_ms: Some(chrono::Utc::now().timestamp_millis()),
    };
    let endpoint = NodeEndpoint::direct(base_url.clone());
    {
        let db = web_state.database.lock().await;
        let store = NodeStore::new(&db)?;
        store.upsert_node(&peer, Some(&acceptance.credential))?;
        store.replace_endpoints(&peer.node_id, &[endpoint.clone()])?;
    }
    if let Some(node_state) = web_state.node_transport.as_ref() {
        node_state
            .trust_peer(peer.node_id.clone(), acceptance.credential.clone())
            .await;
    }

    log::info!("Paired with node {} ({})", peer.node_id, peer.display_name);
    Ok(Json(json!({
        "success": true,
        // Deliberately no credential in this response.
        "node": peer,
        "endpoint": endpoint,
    })))
}

/// Add or edit one remote node and atomically replace its endpoint list.
pub async fn upsert_node(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpsertNodeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let node_id = NodeId::new(payload.node_id).map_err(ApiError::bad_request)?;
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
        let credential = store.credential_for(&node_id)?.ok_or_else(|| {
            ApiError::coded_conflict(
                "node_not_paired",
                "node is not paired; no application credential is stored",
            )
        })?;
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
    let selected_view =
        select_best_path(&paths, StreamClass::View, bitrate, policy).map(|p| p.id.clone());
    let selected_preview =
        select_best_path(&paths, StreamClass::Preview, bitrate, policy).map(|p| p.id.clone());
    let selected_record =
        select_best_path(&paths, StreamClass::Record, bitrate, policy).map(|p| p.id.clone());

    if let Ok(mut cache) = probe_cache().lock() {
        cache.insert(
            node_id.to_string(),
            paths
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| ApiError::internal(format!("failed to encode probe result: {e}")))?,
        );
    }

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

pub async fn update_route_group(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<RouteGroupRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let name = payload.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("route area name must not be empty"));
    }
    let db = web_state.database.lock().await;
    let store = NodeStore::new(&db)?;
    // `route_groups.name` is UNIQUE. Resolve the collision here so the user
    // gets a readable 409 instead of the raw SQLite constraint error as a 500.
    let existing = store.route_group_id_by_name(name)?;
    let id = match payload.id {
        Some(id) => {
            if !store.route_group_exists(id)? {
                return Err(ApiError::not_found(format!("route area {id} not found")));
            }
            if matches!(existing, Some(other) if other != id) {
                return Err(ApiError::coded_conflict(
                    "route_group_name_taken",
                    format!("受信エリア「{name}」はすでにあります。別の名前を入力してください。"),
                ));
            }
            id
        }
        None => existing.unwrap_or(store.ensure_route_group(name)?),
    };
    store.rename_route_group(id, name)?;
    Ok(Json(json!({ "success": true, "id": id, "name": name })))
}

pub async fn delete_route_group(
    State(web_state): State<Arc<WebState>>,
    Path(group_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let db = web_state.database.lock().await;
    NodeStore::new(&db)?.delete_route_group(group_id)?;
    Ok(Json(json!({ "success": true, "id": group_id })))
}

pub async fn remove_route_group_member(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<RemoveRouteGroupMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let node_id = NodeId::new(payload.node_id).map_err(ApiError::bad_request)?;
    let db = web_state.database.lock().await;
    NodeStore::new(&db)?.remove_group_member(payload.group_id, &node_id)?;
    Ok(Json(json!({ "success": true })))
}
