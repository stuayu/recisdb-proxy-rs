//! Web API endpoints for monitoring and configuration.

use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;
use crate::tuner::TunerPoolConfig;
use crate::database::NewBonDriver;

/// Get channel logo image file.
pub async fn get_logo(
    Path(file): Path<String>,
) -> impl IntoResponse {
    // Accept only safe filename patterns: <nid>_<sid>.png
    if !file
        .chars()
        .all(|c| c.is_ascii_digit() || c == '_' || c == '.')
        || !file.ends_with(".png")
    {
        return (StatusCode::BAD_REQUEST, "invalid logo file").into_response();
    }

    let path = std::path::PathBuf::from("logos").join(&file);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match tokio::fs::read(path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "image/png")],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read logo").into_response(),
    }
}

// ============================================================================
// Data structures
// ============================================================================

/// Server statistics.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerStats {
    pub total_sessions: u64,
    pub active_sessions: u64,
    pub total_tuners: usize,
    pub active_tuners: usize,
    pub uptime_seconds: u64,
    pub total_sessions_db: u64,
}

/// Full BonDriver information for API.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BonDriverInfo {
    pub id: i64,
    pub dll_path: String,
    pub driver_name: Option<String>,
    pub version: Option<String>,
    pub group_name: Option<String>,
    pub auto_scan_enabled: bool,
    pub scan_interval_hours: i32,
    pub scan_priority: i32,
    pub last_scan: Option<i64>,
    pub next_scan_at: Option<i64>,
    pub passive_scan_enabled: bool,
    pub max_instances: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Channel information for API.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelInfoApi {
    pub id: i64,
    pub bon_driver_id: i64,
    pub bon_driver_path: Option<String>,
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub manual_sheet: Option<u16>,
    pub raw_name: Option<String>,
    pub channel_name: Option<String>,
    pub physical_ch: Option<u8>,
    pub remote_control_key: Option<u8>,
    pub service_type: Option<u8>,
    pub network_name: Option<String>,
    pub bon_space: Option<u32>,
    pub bon_channel: Option<u32>,
    // Band and region classification
    pub band_type: Option<u8>,
    pub region_id: Option<u8>,
    pub terrestrial_region: Option<String>,
    pub is_enabled: bool,
    pub priority: i32,
    pub failure_count: i32,
    pub scan_time: Option<i64>,
    pub last_seen: Option<i64>,
    // Grouped channel info (only when group_logical=true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuner_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuner_names: Option<Vec<String>>,
}

/// Scan history record for API.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScanHistoryInfo {
    pub id: i64,
    pub bon_driver_id: i64,
    pub scan_time: i64,
    pub channel_count: Option<i32>,
    pub success: bool,
    pub error_message: Option<String>,
}

/// Session history query.
#[derive(Debug, Deserialize)]
pub struct SessionHistoryQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    pub client_address: Option<String>,
}

/// Alert rule create/update request.
#[derive(Debug, Deserialize)]
pub struct AlertRuleRequest {
    pub name: String,
    pub metric: String,
    pub condition: String,
    pub threshold: f64,
    pub severity: Option<String>,
    pub is_enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub webhook_format: Option<String>,
}

/// Client control override request.
#[derive(Debug, Deserialize)]
pub struct ClientControlOverrideRequest {
    pub override_priority: Option<Option<i32>>,
    pub override_exclusive: Option<Option<bool>>,
}

// ============================================================================
// Client/Session endpoints
// ============================================================================

/// Get all connected clients.
pub async fn get_clients(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let sessions = web_state.session_registry.get_all().await;

    let clients: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let effective_priority = s.override_priority.or(s.client_priority);
            let effective_exclusive = s.override_exclusive.unwrap_or(s.client_exclusive);
            json!({
                "session_id": s.id,
                "address": s.addr,
                "host": s.host,
                "tuner_path": s.tuner_path,
                "channel_info": s.channel_info,
                "channel_name": s.channel_name,
                "is_streaming": s.is_streaming,
                "connected_seconds": s.connected_seconds(),
                "signal_level": format!("{:.1}", s.signal_level),
                "packets_sent": s.packets_sent,
                "packets_dropped": s.packets_dropped,
                "packets_scrambled": s.packets_scrambled,
                "packets_error": s.packets_error,
                "current_bitrate_mbps": format!("{:.2}", s.current_bitrate_mbps),
                "client_priority": s.client_priority,
                "client_exclusive": s.client_exclusive,
                "override_priority": s.override_priority,
                "override_exclusive": s.override_exclusive,
                "effective_priority": effective_priority,
                "effective_exclusive": effective_exclusive
            })
        })
        .collect();

    let count = clients.len();

    Json(json!({
        "success": true,
        "clients": clients,
        "count": count
    }))
}

/// Get server statistics.
pub async fn get_stats(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let active_sessions = web_state.session_registry.count().await;
    let tuner_keys = web_state.tuner_pool.keys().await;
    let total_tuners = tuner_keys.len();

    let mut active_tuners = 0;
    for key in tuner_keys.iter() {
        if let Some(tuner) = web_state.tuner_pool.get(key).await {
            if tuner.is_running() {
                active_tuners += 1;
            }
        }
    }

    let total_sessions_db = {
        let db = web_state.database.lock().await;
        db.get_total_session_count().unwrap_or(0)
    };

    let stats = ServerStats {
        total_sessions: active_sessions as u64,
        active_sessions: active_sessions as u64,
        total_tuners,
        active_tuners,
        uptime_seconds: 0,
        total_sessions_db,
    };

    Json(json!({
        "success": true,
        "stats": stats
    }))
}

// ============================================================================
// BonDriver endpoints
// ============================================================================

/// Get all BonDrivers with full details.
pub async fn get_bondrivers(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.get_all_bon_drivers() {
        Ok(drivers) => {
            let bondrivers: Vec<BonDriverInfo> = drivers
                .iter()
                .map(|d| BonDriverInfo {
                    id: d.id,
                    dll_path: d.dll_path.clone(),
                    driver_name: d.driver_name.clone(),
                    version: d.version.clone(),
                    group_name: d.group_name.clone(),
                    auto_scan_enabled: d.auto_scan_enabled,
                    scan_interval_hours: d.scan_interval_hours,
                    scan_priority: d.scan_priority,
                    last_scan: d.last_scan,
                    next_scan_at: d.next_scan_at,
                    passive_scan_enabled: d.passive_scan_enabled,
                    max_instances: d.max_instances,
                    created_at: d.created_at,
                    updated_at: d.updated_at,
                })
                .collect();

            Json(json!({
                "success": true,
                "bondrivers": bondrivers,
                "count": bondrivers.len()
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Get single BonDriver.
pub async fn get_bondriver(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.get_bon_driver(id) {
        Ok(Some(d)) => {
            Json(json!({
                "success": true,
                "bondriver": BonDriverInfo {
                    id: d.id,
                    dll_path: d.dll_path.clone(),
                    driver_name: d.driver_name.clone(),
                    version: d.version.clone(),
                    group_name: d.group_name.clone(),
                    auto_scan_enabled: d.auto_scan_enabled,
                    scan_interval_hours: d.scan_interval_hours,
                    scan_priority: d.scan_priority,
                    last_scan: d.last_scan,
                    next_scan_at: d.next_scan_at,
                    passive_scan_enabled: d.passive_scan_enabled,
                    max_instances: d.max_instances,
                    created_at: d.created_at,
                    updated_at: d.updated_at,
                }
            }))
        }
        Ok(None) => {
            Json(json!({
                "success": false,
                "error": "BonDriver not found"
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Update BonDriver request.
#[derive(Debug, Deserialize)]
pub struct UpdateBonDriverRequest {
    pub dll_path: Option<String>,
    pub driver_name: Option<String>,
    pub group_name: Option<String>,
    pub max_instances: Option<i32>,
    pub auto_scan_enabled: Option<bool>,
    pub scan_interval_hours: Option<i32>,
    pub scan_priority: Option<i32>,
    pub passive_scan_enabled: Option<bool>,
}

/// Create BonDriver request.
#[derive(Debug, Deserialize)]
pub struct CreateBonDriverRequest {
    pub dll_path: String,
    pub driver_name: Option<String>,
    pub group_name: Option<String>,
    pub max_instances: Option<i32>,
    pub auto_scan_enabled: Option<bool>,
    pub scan_interval_hours: Option<i32>,
    pub scan_priority: Option<i32>,
    pub passive_scan_enabled: Option<bool>,
}

/// Create BonDriver.
pub async fn create_bondriver(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<CreateBonDriverRequest>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    let dll_path = payload.dll_path.trim();
    if dll_path.is_empty() {
        return Json(json!({
            "success": false,
            "error": "dll_path is required"
        }));
    }

    match db.get_bon_driver_by_path(dll_path) {
        Ok(Some(_)) => {
            return Json(json!({
                "success": false,
                "error": "BonDriver already exists"
            }));
        }
        Ok(None) => {}
        Err(e) => {
            return Json(json!({
                "success": false,
                "error": e.to_string()
            }));
        }
    }

    let mut new_driver = NewBonDriver::new(dll_path.to_string());
    if let Some(name) = payload.driver_name.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        new_driver.driver_name = Some(name.to_string());
    }
    if let Some(max_instances) = payload.max_instances {
        if max_instances > 0 {
            new_driver.max_instances = Some(max_instances);
        }
    }

    let id = match db.insert_bon_driver(&new_driver) {
        Ok(id) => id,
        Err(e) => {
            return Json(json!({
                "success": false,
                "error": e.to_string()
            }));
        }
    };

    if let Some(group) = payload.group_name.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Err(e) = db.set_group_name(id, Some(group)) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to set group_name: {}", e)
            }));
        }
    }

    if payload.auto_scan_enabled.is_some()
        || payload.scan_interval_hours.is_some()
        || payload.scan_priority.is_some()
        || payload.passive_scan_enabled.is_some()
    {
        let auto_scan = payload.auto_scan_enabled.unwrap_or(false);
        let interval = payload.scan_interval_hours.unwrap_or(24);
        let priority = payload.scan_priority.unwrap_or(0);
        let passive = payload.passive_scan_enabled.unwrap_or(false);

        if let Err(e) = db.update_scan_config(id, Some(auto_scan), Some(interval), Some(priority), Some(passive)) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update scan config: {}", e)
            }));
        }
    }

    Json(json!({
        "success": true,
        "id": id,
        "message": "BonDriver created successfully"
    }))
}

/// Update BonDriver.
pub async fn update_bondriver(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateBonDriverRequest>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    if let Some(path) = payload.dll_path.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Err(e) = db.update_bon_driver_path(id, path) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update dll_path: {}", e)
            }));
        }
    }

    // Update individual fields
    if let Some(max_instances) = payload.max_instances {
        if let Err(e) = db.update_bon_driver_max_instances(id, max_instances) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update max_instances: {}", e)
            }));
        }
    }

    if let Some(name) = &payload.driver_name {
        if let Err(e) = db.update_bon_driver_display_name(id, name) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update driver_name: {}", e)
            }));
        }
    }

    if let Some(group) = &payload.group_name {
        if let Err(e) = db.set_group_name(id, Some(group.as_str())) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update group_name: {}", e)
            }));
        }
    }

    // Update scan config if any scan-related fields are provided
    if payload.auto_scan_enabled.is_some()
        || payload.scan_interval_hours.is_some()
        || payload.scan_priority.is_some()
        || payload.passive_scan_enabled.is_some()
    {
        // Get current values first
        let current = match db.get_bon_driver(id) {
            Ok(Some(d)) => d,
            _ => return Json(json!({
                "success": false,
                "error": "BonDriver not found"
            })),
        };

        let auto_scan = payload.auto_scan_enabled.unwrap_or(current.auto_scan_enabled);
        let interval = payload.scan_interval_hours.unwrap_or(current.scan_interval_hours);
        let priority = payload.scan_priority.unwrap_or(current.scan_priority);
        let passive = payload.passive_scan_enabled.unwrap_or(current.passive_scan_enabled);

        if let Err(e) = db.update_scan_config(id, Some(auto_scan), Some(interval), Some(priority), Some(passive)) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update scan config: {}", e)
            }));
        }
    }

    Json(json!({
        "success": true,
        "message": "BonDriver updated successfully"
    }))
}

/// Delete BonDriver.
pub async fn delete_bondriver(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.delete_bon_driver(id) {
        Ok(_) => {
            Json(json!({
                "success": true,
                "message": "BonDriver deleted successfully"
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Trigger immediate scan for a BonDriver.
pub async fn trigger_scan(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.enable_immediate_scan(id) {
        Ok(_) => {
            Json(json!({
                "success": true,
                "message": "Scan scheduled"
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

// ============================================================================
// Channel endpoints
// ============================================================================

/// Query parameters for channel list.
#[derive(Debug, Deserialize)]
pub struct ChannelQuery {
    pub bondriver_id: Option<i64>,
    pub enabled_only: Option<bool>,
    pub group_logical: Option<bool>,
}

/// Get all channels.
pub async fn get_channels(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<ChannelQuery>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    let enabled_only = query.enabled_only.unwrap_or(false);

    // Get channels based on query
    let channel_infos: Result<Vec<ChannelInfoApi>, String> = if let Some(bondriver_id) = query.bondriver_id {
        // Get channels for specific BonDriver
        db.get_channels_by_bon_driver(bondriver_id)
            .map(|channels| {
                channels
                    .into_iter()
                    .filter(|c| !enabled_only || c.is_enabled)
                    .map(|c| ChannelInfoApi {
                        id: c.id,
                        bon_driver_id: c.bon_driver_id,
                        bon_driver_path: None,
                        nid: c.nid,
                        sid: c.sid,
                        tsid: c.tsid,
                        manual_sheet: c.manual_sheet,
                        raw_name: c.raw_name,
                        channel_name: c.channel_name,
                        physical_ch: c.physical_ch,
                        remote_control_key: c.remote_control_key,
                        service_type: c.service_type,
                        network_name: c.network_name,
                        bon_space: c.bon_space,
                        bon_channel: c.bon_channel,
                        band_type: c.band_type,
                        region_id: c.region_id,
                        terrestrial_region: c.terrestrial_region,
                        is_enabled: c.is_enabled,
                        priority: c.priority,
                        failure_count: c.failure_count,
                        scan_time: c.scan_time,
                        last_seen: c.last_seen,
                        tuner_count: None,
                        tuner_names: None,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    } else if query.group_logical.unwrap_or(false) {
        // Get all channels grouped by logical identity (NID-SID-TSID)
        db.get_all_bon_drivers()
            .map(|all_drivers| {
                let mut channel_map: std::collections::HashMap<(u16, u16, u16), ChannelInfoApi> = std::collections::HashMap::new();

                for driver in &all_drivers {
                    if let Ok(channels) = db.get_channels_by_bon_driver(driver.id) {
                        for c in channels {
                            if enabled_only && !c.is_enabled {
                                continue;
                            }
                            let key = (c.nid, c.sid, c.tsid);
                            let driver_name = driver.driver_name.clone()
                                .unwrap_or_else(|| std::path::Path::new(&driver.dll_path)
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("Unknown")
                                    .to_string());

                            channel_map.entry(key)
                                .and_modify(|existing| {
                                    if let Some(ref mut count) = existing.tuner_count {
                                        *count += 1;
                                    }
                                    if let Some(ref mut names) = existing.tuner_names {
                                        if !names.contains(&driver_name) {
                                            names.push(driver_name.clone());
                                        }
                                    }
                                    // Use higher priority
                                    if c.priority > existing.priority {
                                        existing.priority = c.priority;
                                    }
                                })
                                .or_insert_with(|| ChannelInfoApi {
                                    id: c.id,
                                    bon_driver_id: c.bon_driver_id,
                                    bon_driver_path: Some(driver.dll_path.clone()),
                                    nid: c.nid,
                                    sid: c.sid,
                                    tsid: c.tsid,
                                    manual_sheet: c.manual_sheet,
                                    raw_name: c.raw_name.clone(),
                                    channel_name: c.channel_name.clone(),
                                    physical_ch: c.physical_ch,
                                    remote_control_key: c.remote_control_key,
                                    service_type: c.service_type,
                                    network_name: c.network_name.clone(),
                                    bon_space: c.bon_space,
                                    bon_channel: c.bon_channel,
                                    band_type: c.band_type,
                                    region_id: c.region_id,
                                    terrestrial_region: c.terrestrial_region.clone(),
                                    is_enabled: c.is_enabled,
                                    priority: c.priority,
                                    failure_count: c.failure_count,
                                    scan_time: c.scan_time,
                                    last_seen: c.last_seen,
                                    tuner_count: Some(1),
                                    tuner_names: Some(vec![driver_name]),
                                });
                        }
                    }
                }

                let mut channels: Vec<ChannelInfoApi> = channel_map.into_values().collect();
                channels.sort_by(|a, b| {
                    a.nid.cmp(&b.nid)
                        .then_with(|| a.tsid.cmp(&b.tsid))
                        .then_with(|| a.sid.cmp(&b.sid))
                });
                channels
            })
            .map_err(|e| e.to_string())
    } else {
        // Get all channels with driver info
        db.get_all_channels_with_drivers()
            .map(|channels| {
                channels
                    .into_iter()
                    .filter(|(c, _)| !enabled_only || c.is_enabled)
                    .map(|(c, bd)| ChannelInfoApi {
                        id: c.id,
                        bon_driver_id: c.bon_driver_id,
                        bon_driver_path: bd.map(|d| d.dll_path),
                        nid: c.nid as u16,
                        sid: c.sid as u16,
                        tsid: c.tsid as u16,
                        manual_sheet: None,
                        raw_name: None,
                        channel_name: c.service_name,
                        physical_ch: None,
                        remote_control_key: c.remote_control_key.map(|v| v as u8),
                        service_type: c.service_type.map(|v| v as u8),
                        network_name: c.ts_name,
                        bon_space: Some(c.space),
                        bon_channel: Some(c.channel),
                        band_type: None,
                        region_id: None,
                        terrestrial_region: None,
                        is_enabled: c.is_enabled,
                        priority: c.priority,
                        failure_count: 0,
                        scan_time: None,
                        last_seen: None,
                        tuner_count: None,
                        tuner_names: None,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    };

    match channel_infos {
        Ok(infos) => {
            Json(json!({
                "success": true,
                "channels": infos,
                "count": infos.len()
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e
            }))
        }
    }
}

/// Update channel request.
#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub channel_name: Option<String>,
    pub priority: Option<i32>,
    pub is_enabled: Option<bool>,
}

/// Update channel.
pub async fn update_channel(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateChannelRequest>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    if payload.channel_name.is_none() && payload.priority.is_none() && payload.is_enabled.is_none() {
        return Json(json!({
            "success": false,
            "error": "No fields to update"
        }));
    }

    match db.update_channel_fields(
        id,
        payload.channel_name.as_deref(),
        payload.priority,
        payload.is_enabled,
    ) {
        Ok(_) => {
            Json(json!({
                "success": true,
                "message": "Channel updated successfully"
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Enable/disable channel.
pub async fn toggle_channel(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    let enabled = payload.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

    let result = if enabled {
        db.enable_channel(id)
    } else {
        db.disable_channel(id)
    };

    match result {
        Ok(_) => {
            Json(json!({
                "success": true,
                "message": if enabled { "Channel enabled" } else { "Channel disabled" }
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Delete channel.
pub async fn delete_channel(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.delete_channel(id) {
        Ok(_) => {
            Json(json!({
                "success": true,
                "message": "Channel deleted successfully"
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

// ============================================================================
// Scan history endpoints
// ============================================================================

/// Get scan history.
pub async fn get_scan_history(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<ChannelQuery>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    let bondriver_id = query.bondriver_id.unwrap_or(0);

    // Get all scan history if bondriver_id is 0
    let result = if bondriver_id > 0 {
        db.get_scan_history(bondriver_id, 100)
    } else {
        // Get scan history for all bondrivers
        let mut all_history = Vec::new();
        if let Ok(drivers) = db.get_all_bon_drivers() {
            for driver in drivers {
                if let Ok(history) = db.get_scan_history(driver.id, 50) {
                    all_history.extend(history);
                }
            }
        }
        // Sort by scan_time descending
        all_history.sort_by(|a, b| b.scan_time.cmp(&a.scan_time));
        Ok(all_history.into_iter().take(100).collect())
    };

    match result {
        Ok(history) => {
            let history_infos: Vec<ScanHistoryInfo> = history
                .iter()
                .map(|h| ScanHistoryInfo {
                    id: h.id,
                    bon_driver_id: h.bon_driver_id,
                    scan_time: h.scan_time,
                    channel_count: h.channel_count,
                    success: h.success,
                    error_message: h.error_message.clone(),
                })
                .collect();

            Json(json!({
                "success": true,
                "history": history_infos,
                "count": history_infos.len()
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

// ============================================================================
// Legacy endpoints (for backwards compatibility)
// ============================================================================

/// Legacy: Get all active tuners (alias for get_bondrivers).
pub async fn get_tuners(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.get_all_bon_drivers() {
        Ok(drivers) => {
            let tuners: Vec<serde_json::Value> = drivers
                .iter()
                .map(|d| json!({
                    "id": d.id,
                    "dll_path": d.dll_path,
                    "display_name": d.driver_name,
                    "group_name": d.group_name,
                    "max_instances": d.max_instances
                }))
                .collect();

            Json(json!({
                "success": true,
                "tuners": tuners,
                "count": tuners.len()
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Legacy: Get server configuration.
pub async fn get_config(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    get_bondrivers(State(web_state)).await
}

/// Legacy: Update configuration.
#[derive(Debug, Deserialize)]
pub struct LegacyBonDriverConfig {
    pub id: i64,
    pub dll_path: String,
    pub display_name: Option<String>,
    pub group_name: Option<String>,
    pub max_instances: i32,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConfigRequest {
    pub bon_drivers: Vec<LegacyBonDriverConfig>,
}

pub async fn update_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateConfigRequest>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    for driver_config in payload.bon_drivers {
        if let Err(e) = db.update_bon_driver_max_instances(driver_config.id, driver_config.max_instances) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to update {}: {}", driver_config.dll_path, e)
            }));
        }

        // Update group_name if provided
        if let Some(group) = driver_config.group_name {
            if let Err(e) = db.set_group_name(driver_config.id, Some(&group)) {
                return Json(json!({
                    "success": false,
                    "error": format!("Failed to update group_name for {}: {}", driver_config.dll_path, e)
                }));
            }
        }
    }

    Json(json!({
        "success": true,
        "message": "Configuration updated successfully"
    }))
}

// ============================================================================
// Scan scheduler configuration endpoints
// ============================================================================

/// Get tuner optimization configuration.
pub async fn get_tuner_config(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    match db.get_tuner_config() {
        Ok((
            keep_alive,
            prewarm_enabled,
            prewarm_timeout,
            set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms,
            signal_poll_interval_ms,
            signal_wait_timeout_ms,
        )) => Json(json!({
            "success": true,
            "config": {
                "keep_alive_secs": keep_alive,
                "prewarm_enabled": prewarm_enabled,
                "prewarm_timeout_secs": prewarm_timeout,
                "set_channel_retry_interval_ms": set_channel_retry_interval_ms,
                "set_channel_retry_timeout_ms": set_channel_retry_timeout_ms,
                "signal_poll_interval_ms": signal_poll_interval_ms,
                "signal_wait_timeout_ms": signal_wait_timeout_ms,
            }
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

/// Update tuner optimization configuration request.
#[derive(Debug, Deserialize)]
pub struct UpdateTunerConfigRequest {
    pub keep_alive_secs: Option<u64>,
    pub prewarm_enabled: Option<bool>,
    pub prewarm_timeout_secs: Option<u64>,
    pub set_channel_retry_interval_ms: Option<u64>,
    pub set_channel_retry_timeout_ms: Option<u64>,
    pub signal_poll_interval_ms: Option<u64>,
    pub signal_wait_timeout_ms: Option<u64>,
}

/// Update tuner optimization configuration.
pub async fn update_tuner_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateTunerConfigRequest>,
) -> impl IntoResponse {
    let (
        keep_alive,
        prewarm_enabled,
        prewarm_timeout,
        set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms,
        signal_poll_interval_ms,
        signal_wait_timeout_ms,
    ) = {
        let db = web_state.database.lock().await;

        let (
            mut keep_alive,
            mut prewarm_enabled,
            mut prewarm_timeout,
            mut set_channel_retry_interval_ms,
            mut set_channel_retry_timeout_ms,
            mut signal_poll_interval_ms,
            mut signal_wait_timeout_ms,
        ) =
            match db.get_tuner_config() {
                Ok(config) => config,
                Err(_) => (60, true, 30, 500, 10_000, 500, 10_000),
            };

        if let Some(val) = payload.keep_alive_secs {
            if val > 0 {
                keep_alive = val;
            }
        }
        if let Some(val) = payload.prewarm_enabled {
            prewarm_enabled = val;
        }
        if let Some(val) = payload.prewarm_timeout_secs {
            if val > 0 {
                prewarm_timeout = val;
            }
        }

        if let Some(val) = payload.set_channel_retry_interval_ms {
            if val > 0 {
                set_channel_retry_interval_ms = val;
            }
        }
        if let Some(val) = payload.set_channel_retry_timeout_ms {
            if val > 0 {
                set_channel_retry_timeout_ms = val;
            }
        }
        if let Some(val) = payload.signal_poll_interval_ms {
            if val > 0 {
                signal_poll_interval_ms = val;
            }
        }
        if let Some(val) = payload.signal_wait_timeout_ms {
            if val > 0 {
                signal_wait_timeout_ms = val;
            }
        }

        if let Err(e) = db.update_tuner_config(
            keep_alive,
            prewarm_enabled,
            prewarm_timeout,
            set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms,
            signal_poll_interval_ms,
            signal_wait_timeout_ms,
        ) {
            return Json(json!({
                "success": false,
                "error": format!("Failed to save configuration: {}", e)
            }));
        }

        (
            keep_alive,
            prewarm_enabled,
            prewarm_timeout,
            set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms,
            signal_poll_interval_ms,
            signal_wait_timeout_ms,
        )
    };

    let config = crate::web::state::TunerConfigInfo {
        keep_alive_secs: keep_alive,
        prewarm_enabled,
        prewarm_timeout_secs: prewarm_timeout,
        set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms,
        signal_poll_interval_ms,
        signal_wait_timeout_ms,
    };
    web_state.update_tuner_config(config.clone()).await;

    let pool_config = TunerPoolConfig {
        keep_alive_secs: keep_alive,
        prewarm_enabled,
        prewarm_timeout_secs: prewarm_timeout,
        set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms,
        signal_poll_interval_ms,
        signal_wait_timeout_ms,
    };
    web_state.tuner_pool.update_config(pool_config).await;

    Json(json!({
        "success": true,
        "message": "Tuner configuration saved successfully",
        "config": {
            "keep_alive_secs": config.keep_alive_secs,
            "prewarm_enabled": config.prewarm_enabled,
            "prewarm_timeout_secs": config.prewarm_timeout_secs,
            "set_channel_retry_interval_ms": config.set_channel_retry_interval_ms,
            "set_channel_retry_timeout_ms": config.set_channel_retry_timeout_ms,
            "signal_poll_interval_ms": config.signal_poll_interval_ms,
            "signal_wait_timeout_ms": config.signal_wait_timeout_ms,
        }
    }))
}

/// Get scan scheduler configuration.
pub async fn get_scan_config(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    
    match db.get_scan_scheduler_config() {
        Ok((interval, concurrent, timeout, signal_lock_wait_ms, ts_read_timeout_ms)) => {
            Json(json!({
                "success": true,
                "config": {
                    "check_interval_secs": interval,
                    "max_concurrent_scans": concurrent,
                    "scan_timeout_secs": timeout,
                    "signal_lock_wait_ms": signal_lock_wait_ms,
                    "ts_read_timeout_ms": ts_read_timeout_ms,
                }
            }))
        }
        Err(e) => {
            Json(json!({
                "success": false,
                "error": e.to_string()
            }))
        }
    }
}

/// Update scan scheduler configuration request.
#[derive(Debug, Deserialize)]
pub struct UpdateScanConfigRequest {
    pub check_interval_secs: Option<u64>,
    pub max_concurrent_scans: Option<usize>,
    pub scan_timeout_secs: Option<u64>,
    pub signal_lock_wait_ms: Option<u64>,
    pub ts_read_timeout_ms: Option<u64>,
}

/// Update scan scheduler configuration.
pub async fn update_scan_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateScanConfigRequest>,
) -> impl IntoResponse {
    // Get current config from database
    let db = web_state.database.lock().await;
    
    let (mut interval, mut concurrent, mut timeout, mut signal_lock_wait_ms, mut ts_read_timeout_ms) =
        match db.get_scan_scheduler_config() {
            Ok(config) => config,
            Err(_) => (60, 1, 900, 500, 300000),
        };

    // Update with provided values
    if let Some(val) = payload.check_interval_secs {
        if val > 0 {
            interval = val;
        }
    }
    if let Some(val) = payload.max_concurrent_scans {
        if val > 0 {
            concurrent = val;
        }
    }
    if let Some(val) = payload.scan_timeout_secs {
        if val > 0 {
            timeout = val;
        }
    }
    if let Some(val) = payload.signal_lock_wait_ms {
        if val > 0 {
            signal_lock_wait_ms = val;
        }
    }
    if let Some(val) = payload.ts_read_timeout_ms {
        if val > 0 {
            ts_read_timeout_ms = val;
        }
    }

    // Save to database
    if let Err(e) = db.update_scan_scheduler_config(
        interval,
        concurrent,
        timeout,
        signal_lock_wait_ms,
        ts_read_timeout_ms,
    ) {
        return Json(json!({
            "success": false,
            "error": format!("Failed to save configuration: {}", e)
        }));
    }

    // Update in-memory cache
    let config = crate::web::state::ScanSchedulerInfo {
        check_interval_secs: interval,
        max_concurrent_scans: concurrent,
        scan_timeout_secs: timeout,
        signal_lock_wait_ms,
        ts_read_timeout_ms,
    };
    web_state.update_scan_config(config.clone()).await;

    Json(json!({
        "success": true,
        "message": "Scan configuration saved successfully",
        "config": {
            "check_interval_secs": config.check_interval_secs,
            "max_concurrent_scans": config.max_concurrent_scans,
            "scan_timeout_secs": config.scan_timeout_secs,
            "signal_lock_wait_ms": config.signal_lock_wait_ms,
            "ts_read_timeout_ms": config.ts_read_timeout_ms,
        }
    }))
}

// ============================================================================
// Session history & client metrics endpoints
// ============================================================================

/// Get session history (paginated).
pub async fn get_session_history(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<SessionHistoryQuery>,
) -> impl IntoResponse {
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 200);

    let db = web_state.database.lock().await;
    match db.get_session_history(page, per_page, query.client_address.as_deref()) {
        Ok((rows, total)) => Json(json!({
            "success": true,
            "total": total,
            "page": page,
            "per_page": per_page,
            "history": rows
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

/// Get time-series quality data for a client.
pub async fn get_client_quality(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let sessions = web_state.session_registry.get_all().await;
    if let Some(session) = sessions.into_iter().find(|s| s.id == id) {
        let bitrate: Vec<(i64, f64)> = session.metrics_history.bitrate_history.into_iter().collect();
        let packet_loss: Vec<(i64, f64)> = session.metrics_history.packet_loss_history.into_iter().collect();

        return Json(json!({
            "success": true,
            "bitrate": bitrate,
            "packet_loss": packet_loss,
        }));
    }

    Json(json!({
        "success": false,
        "error": "Session not found"
    }))
}

/// Get metrics history for a client (bitrate, packet loss, signal level).
pub async fn get_client_metrics_history(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let sessions = web_state.session_registry.get_all().await;
    if let Some(session) = sessions.into_iter().find(|s| s.id == id) {
        let bitrate: Vec<(i64, f64)> = session.metrics_history.bitrate_history.into_iter().collect();
        let packet_loss: Vec<(i64, f64)> = session.metrics_history.packet_loss_history.into_iter().collect();
        let signal_level: Vec<(i64, f32)> = session.metrics_history.signal_history.into_iter().collect();

        return Json(json!({
            "success": true,
            "bitrate": bitrate,
            "packet_loss": packet_loss,
            "signal_level": signal_level
        }));
    }

    Json(json!({
        "success": false,
        "error": "Session not found"
    }))
}

/// Disconnect a client session remotely.
pub async fn disconnect_client(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let ok = web_state.session_registry.request_shutdown(id).await;
    Json(json!({
        "success": ok
    }))
}

/// Override client controls (priority/exclusive).
pub async fn override_client_controls(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
    Json(payload): Json<ClientControlOverrideRequest>,
) -> impl IntoResponse {
    // Treat JSON null as explicit clear. Absence means no change.
    web_state
        .session_registry
        .update_override_controls(id, payload.override_priority, payload.override_exclusive)
        .await;
    Json(json!({
        "success": true
    }))
}

// ============================================================================
// Alert endpoints
// ============================================================================

/// Get active alerts.
pub async fn get_alerts(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    match db.get_active_alerts() {
        Ok(alerts) => Json(json!({
            "success": true,
            "alerts": alerts,
            "count": alerts.len()
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

/// Get alert rules.
pub async fn get_alert_rules(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    match db.get_alert_rules() {
        Ok(rules) => Json(json!({
            "success": true,
            "rules": rules,
            "count": rules.len()
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

/// Create alert rule.
pub async fn create_alert_rule(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<AlertRuleRequest>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    let severity = payload.severity.unwrap_or_else(|| "warning".to_string());
    let is_enabled = payload.is_enabled.unwrap_or(true);

    match db.create_alert_rule(
        &payload.name,
        &payload.metric,
        &payload.condition,
        payload.threshold,
        &severity,
        is_enabled,
        payload.webhook_url.as_deref(),
        payload.webhook_format.as_deref(),
    ) {
        Ok(id) => Json(json!({
            "success": true,
            "id": id
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

/// Delete alert rule.
pub async fn delete_alert_rule(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    match db.delete_alert_rule(id) {
        Ok(_) => Json(json!({"success": true})),
        Err(e) => Json(json!({"success": false, "error": e.to_string()})),
    }
}

/// Acknowledge alert.
pub async fn acknowledge_alert(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    match db.acknowledge_alert_history(id) {
        Ok(_) => Json(json!({"success": true})),
        Err(e) => Json(json!({"success": false, "error": e.to_string()})),
    }
}

// ============================================================================
// BonDriver quality endpoints
// ============================================================================

/// Get quality stats for a BonDriver.
pub async fn get_bondriver_quality(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    match db.get_driver_quality_stats(id) {
        Ok(Some(stats)) => Json(json!({
            "success": true,
            "stats": stats
        })),
        Ok(None) => Json(json!({
            "success": false,
            "error": "Stats not found"
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

/// Get BonDriver ranking by quality score.
pub async fn get_bondrivers_ranking(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    match db.get_bondrivers_ranking() {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(driver, score, recent_drop_rate, total_sessions)| {
                    json!({
                        "driver": driver,
                        "quality_score": score,
                        "recent_drop_rate": recent_drop_rate,
                        "total_sessions": total_sessions
                    })
                })
                .collect();
            Json(json!({
                "success": true,
                "items": items,
                "count": items.len()
            }))
        }
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

