//! BonDriver CRUD endpoints and scan history.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;
use crate::database::NewBonDriver;

use super::channels::ChannelQuery;
use super::error::ApiError;

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
    /// Whether a channel scan is holding this driver right now. The scan
    /// reserves a real tuner slot, so this is also why the driver may look
    /// unavailable to viewers.
    #[serde(default)]
    pub is_scanning: bool,
    pub created_at: i64,
    pub updated_at: i64,
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

/// Get all BonDrivers with full details.
pub async fn get_bondrivers(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let drivers = db.get_all_bon_drivers()?;
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
            is_scanning: web_state.tuner_pool.is_scanning(&d.dll_path),
            created_at: d.created_at,
            updated_at: d.updated_at,
        })
        .collect();

    Ok(Json(json!({
        "success": true,
        "bondrivers": bondrivers,
        "count": bondrivers.len()
    })))
}

/// Get single BonDriver.
pub async fn get_bondriver(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    match db.get_bon_driver(id)? {
        Some(d) => Ok(Json(json!({
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
                is_scanning: web_state.tuner_pool.is_scanning(&d.dll_path),
                created_at: d.created_at,
                updated_at: d.updated_at,
            }
        }))),
        None => Err(ApiError::not_found("BonDriver not found")),
    }
}

/// Create BonDriver.
pub async fn create_bondriver(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<CreateBonDriverRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let dll_path = payload.dll_path.trim();
    if dll_path.is_empty() {
        return Err(ApiError::bad_request("dll_path is required"));
    }

    if db.get_bon_driver_by_path(dll_path)?.is_some() {
        return Err(ApiError::bad_request("BonDriver already exists"));
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

    let id = db.insert_bon_driver(&new_driver)?;

    if let Some(group) = payload.group_name.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        db.set_group_name(id, Some(group))
            .map_err(|e| ApiError::internal(format!("Failed to set group_name: {}", e)))?;
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

        db.update_scan_config(id, Some(auto_scan), Some(interval), Some(priority), Some(passive))
            .map_err(|e| ApiError::internal(format!("Failed to update scan config: {}", e)))?;
    }

    Ok(Json(json!({
        "success": true,
        "id": id,
        "message": "BonDriver created successfully"
    })))
}

/// Update BonDriver.
pub async fn update_bondriver(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateBonDriverRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    if let Some(path) = payload.dll_path.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        db.update_bon_driver_path(id, path)
            .map_err(|e| ApiError::internal(format!("Failed to update dll_path: {}", e)))?;
    }

    // Update individual fields
    if let Some(max_instances) = payload.max_instances {
        db.update_bon_driver_max_instances(id, max_instances)
            .map_err(|e| ApiError::internal(format!("Failed to update max_instances: {}", e)))?;
    }

    if let Some(name) = &payload.driver_name {
        db.update_bon_driver_display_name(id, name)
            .map_err(|e| ApiError::internal(format!("Failed to update driver_name: {}", e)))?;
    }

    if let Some(group) = &payload.group_name {
        db.set_group_name(id, Some(group.as_str()))
            .map_err(|e| ApiError::internal(format!("Failed to update group_name: {}", e)))?;
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
            Ok(None) => return Err(ApiError::not_found("BonDriver not found")),
            Err(e) => return Err(e.into()),
        };

        let auto_scan = payload.auto_scan_enabled.unwrap_or(current.auto_scan_enabled);
        let interval = payload.scan_interval_hours.unwrap_or(current.scan_interval_hours);
        let priority = payload.scan_priority.unwrap_or(current.scan_priority);
        let passive = payload.passive_scan_enabled.unwrap_or(current.passive_scan_enabled);

        db.update_scan_config(id, Some(auto_scan), Some(interval), Some(priority), Some(passive))
            .map_err(|e| ApiError::internal(format!("Failed to update scan config: {}", e)))?;
    }

    Ok(Json(json!({
        "success": true,
        "message": "BonDriver updated successfully"
    })))
}

/// Delete BonDriver.
pub async fn delete_bondriver(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    db.delete_bon_driver(id)?;
    Ok(Json(json!({
        "success": true,
        "message": "BonDriver deleted successfully"
    })))
}

/// Trigger immediate scan for a BonDriver.
pub async fn trigger_scan(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    db.request_immediate_scan(id)?;
    Ok(Json(json!({
        "success": true,
        "message": "Scan scheduled"
    })))
}

/// Get scan history.
pub async fn get_scan_history(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<ChannelQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

    let history = result?;
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

    Ok(Json(json!({
        "success": true,
        "history": history_infos,
        "count": history_infos.len()
    })))
}
