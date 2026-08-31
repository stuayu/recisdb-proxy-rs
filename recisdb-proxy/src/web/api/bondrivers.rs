//! BonDriver CRUD endpoints and scan history.

use crate::database::StreamFormat;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::database::NewBonDriver;
use crate::web::state::WebState;

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
    /// What the driver hands back: `"ts"` or `"mmttlv"` (4K).
    ///
    /// Cannot be derived from a scan — scanning parses TS, so a raw 4K tuner
    /// produces nothing to classify until the converter is already in the
    /// path. It is a property of the driver, set here.
    pub stream_format: String,
    /// Never run libaribb25 on this driver, because the source arrives
    /// already descrambled. 4K is switched off automatically by band as well;
    /// this covers descrambled sources that are not 4K.
    pub disable_b25: bool,
    /// Circuit state of the driver's open path: `"healthy"`, `"degraded"`
    /// (answers, but far too slowly), `"open"` (refusing opens after repeated
    /// failures) or `"half_open"` (one trial open is being allowed through).
    ///
    /// A tuner that is busy must always show *why* (CLAUDE.md, Web
    /// ダッシュボード) — "the circuit is open for another 12 s" is a reason a
    /// user can act on, whereas a bare failure is not.
    pub breaker_state: String,
    /// Seconds until this driver will be tried again, when the circuit is
    /// open. `None` otherwise.
    pub breaker_retry_in_secs: Option<u64>,
    /// Combined driver health, `0.0`–`1.0`: stream integrity multiplied by
    /// runtime health (startup latency, stalls, failures). `1.0` also means
    /// "no observations yet".
    pub quality_score: f64,
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
    /// `"ts"` or `"mmttlv"`. Anything unrecognised is treated as `"ts"`.
    pub stream_format: Option<String>,
    pub disable_b25: Option<bool>,
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
    /// `"ts"` or `"mmttlv"`. Anything unrecognised is treated as `"ts"`.
    pub stream_format: Option<String>,
    pub disable_b25: Option<bool>,
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
            stream_format: db
                .driver_stream_format(&d.dll_path)
                .as_db_value()
                .to_string(),
            disable_b25: db.driver_disables_b25(&d.dll_path),
            breaker_state: breaker_state_str(&web_state, &d.dll_path),
            breaker_retry_in_secs: breaker_retry_in_secs(&web_state, &d.dll_path),
            quality_score: db
                .get_driver_quality_score_by_path(&d.dll_path)
                .unwrap_or(1.0),
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

/// Circuit state of `dll_path`'s open path, as a stable API string.
fn breaker_state_str(web_state: &Arc<WebState>, dll_path: &str) -> String {
    use crate::tuner::open_backoff::BreakerState;
    match web_state.tuner_pool.open_backoff().state(dll_path) {
        BreakerState::Healthy => "healthy",
        BreakerState::Degraded => "degraded",
        BreakerState::Open => "open",
        BreakerState::HalfOpen => "half_open",
    }
    .to_string()
}

fn breaker_retry_in_secs(web_state: &Arc<WebState>, dll_path: &str) -> Option<u64> {
    web_state
        .tuner_pool
        .open_backoff()
        .cooldown_remaining(dll_path)
        .map(|d| d.as_secs().max(1))
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
                stream_format: db.driver_stream_format(&d.dll_path).as_db_value().to_string(),
                disable_b25: db.driver_disables_b25(&d.dll_path),
                breaker_state: breaker_state_str(&web_state, &d.dll_path),
                breaker_retry_in_secs: breaker_retry_in_secs(&web_state, &d.dll_path),
                quality_score: db.get_driver_quality_score_by_path(&d.dll_path).unwrap_or(1.0),
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
    if let Some(name) = payload
        .driver_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        new_driver.driver_name = Some(name.to_string());
    }
    if let Some(max_instances) = payload.max_instances {
        if max_instances > 0 {
            new_driver.max_instances = Some(max_instances);
        }
    }

    let id = db.insert_bon_driver(&new_driver)?;

    if let Some(group) = payload
        .group_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        db.set_group_name(id, Some(group))
            .map_err(|e| ApiError::internal(format!("Failed to set group_name: {}", e)))?;
    }

    // Settable at registration: a 4K driver is unusable until it is marked as
    // one, so making the caller register first and patch afterwards would just
    // guarantee a broken first scan.
    if let Some(format) = &payload.stream_format {
        db.set_driver_stream_format(dll_path, StreamFormat::from_db_value(format))
            .map_err(|e| ApiError::internal(format!("Failed to set stream_format: {}", e)))?;
    }
    if let Some(disable) = payload.disable_b25 {
        db.set_driver_disable_b25(dll_path, disable)
            .map_err(|e| ApiError::internal(format!("Failed to set disable_b25: {}", e)))?;
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

        db.update_scan_config(
            id,
            Some(auto_scan),
            Some(interval),
            Some(priority),
            Some(passive),
        )
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

    if let Some(path) = payload
        .dll_path
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
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

    // Stream format / B25 are keyed by dll_path rather than id, so resolve the
    // row once — and after any dll_path change above, so the new path is used.
    if payload.stream_format.is_some() || payload.disable_b25.is_some() {
        let dll_path = match db.get_bon_driver(id) {
            Ok(Some(d)) => d.dll_path,
            Ok(None) => return Err(ApiError::not_found("BonDriver not found")),
            Err(e) => return Err(e.into()),
        };

        if let Some(format) = &payload.stream_format {
            let parsed = StreamFormat::from_db_value(format);
            db.set_driver_stream_format(&dll_path, parsed)
                .map_err(|e| {
                    ApiError::internal(format!("Failed to update stream_format: {}", e))
                })?;
        }
        if let Some(disable) = payload.disable_b25 {
            db.set_driver_disable_b25(&dll_path, disable)
                .map_err(|e| ApiError::internal(format!("Failed to update disable_b25: {}", e)))?;
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
            Ok(None) => return Err(ApiError::not_found("BonDriver not found")),
            Err(e) => return Err(e.into()),
        };

        let auto_scan = payload
            .auto_scan_enabled
            .unwrap_or(current.auto_scan_enabled);
        let interval = payload
            .scan_interval_hours
            .unwrap_or(current.scan_interval_hours);
        let priority = payload.scan_priority.unwrap_or(current.scan_priority);
        let passive = payload
            .passive_scan_enabled
            .unwrap_or(current.passive_scan_enabled);

        db.update_scan_config(
            id,
            Some(auto_scan),
            Some(interval),
            Some(priority),
            Some(passive),
        )
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
