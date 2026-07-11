//! Configuration endpoints: legacy BonDriver config, tuner optimization,
//! external encoder (tsreplace/preview) settings, encode profiles, and the
//! scan scheduler.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;
use crate::tuner::TunerPoolConfig;

use super::error::ApiError;

// ============================================================================
// Legacy config endpoints (for backwards compatibility)
// ============================================================================

/// Legacy: Get server configuration.
pub async fn get_config(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    super::bondrivers::get_bondrivers(State(web_state)).await
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    for driver_config in payload.bon_drivers {
        db.update_bon_driver_max_instances(driver_config.id, driver_config.max_instances)
            .map_err(|e| ApiError::internal(format!("Failed to update {}: {}", driver_config.dll_path, e)))?;

        // Update group_name if provided
        if let Some(group) = driver_config.group_name {
            db.set_group_name(driver_config.id, Some(&group))
                .map_err(|e| ApiError::internal(format!("Failed to update group_name for {}: {}", driver_config.dll_path, e)))?;
        }
    }

    Ok(Json(json!({
        "success": true,
        "message": "Configuration updated successfully"
    })))
}

// ============================================================================
// Scan scheduler configuration endpoints
// ============================================================================

/// Get tuner optimization configuration.
pub async fn get_tuner_config(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let (
        keep_alive,
        prewarm_enabled,
        prewarm_timeout,
        set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms,
        signal_poll_interval_ms,
        signal_wait_timeout_ms,
        prefill_view_ms,
        prefill_preview_ms,
        prefill_record_ms,
        jitter_safety_factor,
    ) = db.get_tuner_config()?;

    Ok(Json(json!({
        "success": true,
        "config": {
            "keep_alive_secs": keep_alive,
            "prewarm_enabled": prewarm_enabled,
            "prewarm_timeout_secs": prewarm_timeout,
            "set_channel_retry_interval_ms": set_channel_retry_interval_ms,
            "set_channel_retry_timeout_ms": set_channel_retry_timeout_ms,
            "signal_poll_interval_ms": signal_poll_interval_ms,
            "signal_wait_timeout_ms": signal_wait_timeout_ms,
            "prefill_view_ms": prefill_view_ms,
            "prefill_preview_ms": prefill_preview_ms,
            "prefill_record_ms": prefill_record_ms,
            "jitter_safety_factor": jitter_safety_factor,
        }
    })))
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
    /// STREAMING_DESIGN.md §4/§9 P3: fixed-duration prefill/jitter buffer.
    pub prefill_view_ms: Option<u64>,
    pub prefill_preview_ms: Option<u64>,
    pub prefill_record_ms: Option<u64>,
    pub jitter_safety_factor: Option<f64>,
}

/// Update tuner optimization configuration.
pub async fn update_tuner_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateTunerConfigRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (
        keep_alive,
        prewarm_enabled,
        prewarm_timeout,
        set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms,
        signal_poll_interval_ms,
        signal_wait_timeout_ms,
        prefill_view_ms,
        prefill_preview_ms,
        prefill_record_ms,
        jitter_safety_factor,
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
            mut prefill_view_ms,
            mut prefill_preview_ms,
            mut prefill_record_ms,
            mut jitter_safety_factor,
        ) =
            match db.get_tuner_config() {
                Ok(config) => config,
                Err(_) => (60, true, 30, 500, 10_000, 500, 10_000, 1000, 2000, 6000, 1.5),
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
        if let Some(val) = payload.prefill_view_ms {
            prefill_view_ms = val;
        }
        if let Some(val) = payload.prefill_preview_ms {
            prefill_preview_ms = val;
        }
        if let Some(val) = payload.prefill_record_ms {
            prefill_record_ms = val;
        }
        if let Some(val) = payload.jitter_safety_factor {
            if val > 0.0 {
                jitter_safety_factor = val;
            }
        }

        db.update_tuner_config(
            keep_alive,
            prewarm_enabled,
            prewarm_timeout,
            set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms,
            signal_poll_interval_ms,
            signal_wait_timeout_ms,
            prefill_view_ms,
            prefill_preview_ms,
            prefill_record_ms,
            jitter_safety_factor,
        )
        .map_err(|e| ApiError::internal(format!("Failed to save configuration: {}", e)))?;

        (
            keep_alive,
            prewarm_enabled,
            prewarm_timeout,
            set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms,
            signal_poll_interval_ms,
            signal_wait_timeout_ms,
            prefill_view_ms,
            prefill_preview_ms,
            prefill_record_ms,
            jitter_safety_factor,
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
        prefill_view_ms,
        prefill_preview_ms,
        prefill_record_ms,
        jitter_safety_factor,
    };
    web_state.update_tuner_config(config.clone()).await;

    // NOTE: prefill/jitter settings are intentionally not forwarded to
    // `TunerPoolConfig` — they configure per-session output buffering
    // (`Session::load_prefill_runtime_config` reads the DB directly at
    // StartStream time), not tuner lifecycle, which is all `TunerPool`
    // uses its config for (STREAMING_DESIGN.md §4/§9 P3).
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

    Ok(Json(json!({
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
            "prefill_view_ms": config.prefill_view_ms,
            "prefill_preview_ms": config.prefill_preview_ms,
            "prefill_record_ms": config.prefill_record_ms,
            "jitter_safety_factor": config.jitter_safety_factor,
        }
    })))
}

/// Get external encoder (tsreplace) configuration — BNDP (TVTest) session
/// pipeline ONLY. The browser-preview pipeline has its own settings at
/// `GET /api/preview-config`.
pub async fn get_tsreplace_config(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let (enabled, command_path, arguments, read_timeout_ms, passthrough_on_error, max_concurrent_encoders, preprocessor_path, preprocessor_arguments) =
        db.get_tsreplace_config()?;
    Ok(Json(json!({
        "success": true,
        "config": {
            "enabled": enabled,
            "command_path": command_path,
            "arguments": arguments,
            "read_timeout_ms": read_timeout_ms,
            "passthrough_on_error": passthrough_on_error,
            "max_concurrent_encoders": max_concurrent_encoders,
            // Read-only in the API (TOML-only, REVIEW S1), exposed
            // for display just like `command_path`.
            "preprocessor_path": preprocessor_path,
            "preprocessor_arguments": preprocessor_arguments,
        }
    })))
}

/// Update external encoder (tsreplace) configuration request.
///
/// `command_path` and `preprocessor_path` are intentionally **not** fields
/// here (REVIEW_2026-07.md S1): they are the programs the server executes
/// (`Command::new(...)`), so they must only be changeable via the TOML
/// config file (`Database::set_tsreplace_command_path` /
/// `set_tsreplace_preprocessor_path`, called once at startup from
/// `main.rs`). If a client sends either in the request body it is silently
/// ignored by serde (unknown field) and the stored value is left untouched
/// below. `preprocessor_arguments` stays API-editable for the same reason
/// `arguments` is: it is passed as an argument vector, never resolved as a
/// program to execute.
#[derive(Debug, Deserialize)]
pub struct UpdateTsreplaceConfigRequest {
    /// Gates only the BNDP (TVTest) session encode pipeline. The browser
    /// preview pipeline is configured via `POST /api/preview-config`.
    pub enabled: Option<bool>,
    pub arguments: Option<String>,
    pub read_timeout_ms: Option<u64>,
    pub passthrough_on_error: Option<bool>,
    pub max_concurrent_encoders: Option<i64>,
    pub preprocessor_arguments: Option<String>,
}

/// Update external encoder (tsreplace) configuration.
///
/// # Security (REVIEW_2026-07.md S1)
/// `command_path` cannot be changed through this endpoint by design — see
/// [`UpdateTsreplaceConfigRequest`]. The existing DB value is always kept.
pub async fn update_tsreplace_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateTsreplaceConfigRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let (mut enabled, command_path, mut arguments, mut read_timeout_ms, mut passthrough_on_error, mut max_concurrent_encoders, preprocessor_path, mut preprocessor_arguments) =
        match db.get_tsreplace_config() {
            Ok(config) => config,
            Err(_) => (false, "tsreplace".to_string(), "".to_string(), 10_000, true, 2, "".to_string(), "".to_string()),
        };

    if let Some(val) = payload.enabled {
        enabled = val;
    }
    if let Some(val) = payload.arguments {
        arguments = val;
    }
    if let Some(val) = payload.read_timeout_ms {
        if val > 0 {
            read_timeout_ms = val;
        }
    }
    if let Some(val) = payload.passthrough_on_error {
        passthrough_on_error = val;
    }
    if let Some(val) = payload.max_concurrent_encoders {
        if val > 0 {
            max_concurrent_encoders = val;
        }
    }
    if let Some(val) = payload.preprocessor_arguments {
        preprocessor_arguments = val;
    }

    db.update_tsreplace_config(
        enabled,
        &command_path,
        &arguments,
        read_timeout_ms,
        passthrough_on_error,
        max_concurrent_encoders,
        // TOML-only (REVIEW S1): always written back exactly as read above.
        &preprocessor_path,
        &preprocessor_arguments,
    )
    .map_err(|e| ApiError::internal(format!("Failed to save configuration: {}", e)))?;

    Ok(Json(json!({
        "success": true,
        "message": "tsreplace configuration saved successfully",
        "config": {
            "enabled": enabled,
            "command_path": command_path,
            "arguments": arguments,
            "read_timeout_ms": read_timeout_ms,
            "passthrough_on_error": passthrough_on_error,
            "max_concurrent_encoders": max_concurrent_encoders,
            "preprocessor_path": preprocessor_path,
            "preprocessor_arguments": preprocessor_arguments,
        }
    })))
}

// ============================================================================
// Browser preview encoder configuration (`preview_encoder_config`)
//
// Fully separate from the BNDP tsreplace endpoints above: gates and
// configures ONLY the HTTP `?profile=preview` streaming path
// (`web/stream.rs::load_preview_encoder_config`).
// ============================================================================

/// Get browser-preview encoder configuration (`GET /api/preview-config`).
pub async fn get_preview_config(State(web_state): State<Arc<WebState>>) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let (enabled, command_path, preprocessor_path, preprocessor_arguments, read_timeout_ms) =
        db.get_preview_encoder_config()?;
    Ok(Json(json!({
        "success": true,
        "config": {
            "enabled": enabled,
            // Both paths are read-only in the API (TOML-only,
            // REVIEW S1: `[preview]` section), exposed for display.
            "command_path": command_path,
            "preprocessor_path": preprocessor_path,
            "preprocessor_arguments": preprocessor_arguments,
            "read_timeout_ms": read_timeout_ms,
        }
    })))
}

/// Update request for the browser-preview encoder configuration.
///
/// `command_path` and `preprocessor_path` are intentionally **not** fields
/// here (REVIEW_2026-07.md S1): they are programs the server executes, so
/// they are only changeable via the TOML `[preview]` section
/// (`Database::set_preview_command_path` / `set_preview_preprocessor_path`,
/// applied once at startup in `main.rs`). If a client sends either, serde
/// silently drops the unknown field and the stored value stays untouched.
#[derive(Debug, Deserialize)]
pub struct UpdatePreviewConfigRequest {
    /// Gates only the HTTP `?profile=preview` streaming path.
    pub enabled: Option<bool>,
    pub preprocessor_arguments: Option<String>,
    pub read_timeout_ms: Option<u64>,
}

/// Update browser-preview encoder configuration (`POST /api/preview-config`).
///
/// # Security (REVIEW_2026-07.md S1)
/// Neither executable path can be changed through this endpoint by design —
/// see [`UpdatePreviewConfigRequest`].
pub async fn update_preview_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdatePreviewConfigRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let (mut enabled, command_path, preprocessor_path, mut preprocessor_arguments, mut read_timeout_ms) =
        db.get_preview_encoder_config()?;

    if let Some(val) = payload.enabled {
        enabled = val;
    }
    if let Some(val) = payload.preprocessor_arguments {
        preprocessor_arguments = val;
    }
    if let Some(val) = payload.read_timeout_ms {
        if val > 0 {
            read_timeout_ms = val;
        }
    }

    db.update_preview_encoder_config(enabled, &preprocessor_arguments, read_timeout_ms)
        .map_err(|e| ApiError::internal(format!("Failed to save configuration: {}", e)))?;

    Ok(Json(json!({
        "success": true,
        "message": "preview configuration saved successfully",
        "config": {
            "enabled": enabled,
            "command_path": command_path,
            "preprocessor_path": preprocessor_path,
            "preprocessor_arguments": preprocessor_arguments,
            "read_timeout_ms": read_timeout_ms,
        }
    })))
}

// ============================================================================
// Encode profile endpoints (STREAMING_DESIGN.md §5.3/§9 P5)
//
// `command_path` (the executable actually run) is never a field on any of
// these request/response types — it stays governed solely by
// `tsreplace_config.command_path` (TOML-only, REVIEW S1). A profile only
// ever supplies codec/container/bitrate/extra arguments, mirroring how
// `update_tsreplace_config` above omits `command_path`.
// ============================================================================

/// Get all encode profiles.
pub async fn get_encode_profiles(State(web_state): State<Arc<WebState>>) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    let profiles = db.get_all_encode_profiles()?;
    Ok(Json(json!({
        "success": true,
        "profiles": profiles,
        "count": profiles.len(),
    })))
}

/// Request body for `POST /api/encode-profiles`.
#[derive(Debug, Deserialize)]
pub struct CreateEncodeProfileRequest {
    pub name: String,
    pub purpose: String,
    pub codec: String,
    pub container: Option<String>,
    pub target_bitrate: Option<i64>,
    pub extra_args: Option<String>,
    pub is_enabled: Option<bool>,
}

/// Create a new encode profile.
pub async fn create_encode_profile(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<CreateEncodeProfileRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if payload.name.trim().is_empty() || payload.purpose.trim().is_empty() || payload.codec.trim().is_empty() {
        return Err(ApiError::bad_request("name, purpose, and codec are required"));
    }

    let db = web_state.database.lock().await;
    let id = db.insert_encode_profile(
        &payload.name,
        &payload.purpose,
        &payload.codec,
        payload.container.as_deref().unwrap_or("mpegts"),
        payload.target_bitrate,
        payload.extra_args.as_deref(),
        payload.is_enabled.unwrap_or(true),
    )?;
    Ok(Json(json!({ "success": true, "id": id })))
}

/// Request body for `POST /api/encode-profiles/:id`.
#[derive(Debug, Deserialize)]
pub struct UpdateEncodeProfileRequest {
    pub name: Option<String>,
    pub purpose: Option<String>,
    pub codec: Option<String>,
    pub container: Option<String>,
    /// `null` = clear to NULL, number = set, field omitted = leave alone.
    pub target_bitrate: Option<Option<i64>>,
    /// `null` = clear to NULL, string = set, field omitted = leave alone.
    pub extra_args: Option<Option<String>>,
    pub is_enabled: Option<bool>,
}

/// Update an existing encode profile.
pub async fn update_encode_profile(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateEncodeProfileRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let has_any = payload.name.is_some()
        || payload.purpose.is_some()
        || payload.codec.is_some()
        || payload.container.is_some()
        || payload.target_bitrate.is_some()
        || payload.extra_args.is_some()
        || payload.is_enabled.is_some();

    if !has_any {
        return Err(ApiError::bad_request("No fields to update"));
    }

    let db = web_state.database.lock().await;
    db.update_encode_profile(
        id,
        payload.name.as_deref(),
        payload.purpose.as_deref(),
        payload.codec.as_deref(),
        payload.container.as_deref(),
        payload.target_bitrate,
        payload.extra_args.as_ref().map(|v| v.as_deref()),
        payload.is_enabled,
    )?;
    Ok(Json(json!({ "success": true, "message": "Encode profile updated successfully" })))
}

/// Delete an encode profile.
pub async fn delete_encode_profile(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    db.delete_encode_profile(id)?;
    Ok(Json(json!({ "success": true, "message": "Encode profile deleted" })))
}

/// Get scan scheduler configuration.
pub async fn get_scan_config(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let (interval, concurrent, timeout, signal_lock_wait_ms, ts_read_timeout_ms) =
        db.get_scan_scheduler_config()?;
    Ok(Json(json!({
        "success": true,
        "config": {
            "check_interval_secs": interval,
            "max_concurrent_scans": concurrent,
            "scan_timeout_secs": timeout,
            "signal_lock_wait_ms": signal_lock_wait_ms,
            "ts_read_timeout_ms": ts_read_timeout_ms,
        }
    })))
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
) -> Result<Json<serde_json::Value>, ApiError> {
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
    db.update_scan_scheduler_config(
        interval,
        concurrent,
        timeout,
        signal_lock_wait_ms,
        ts_read_timeout_ms,
    )
    .map_err(|e| ApiError::internal(format!("Failed to save configuration: {}", e)))?;

    // Update in-memory cache
    let config = crate::web::state::ScanSchedulerInfo {
        check_interval_secs: interval,
        max_concurrent_scans: concurrent,
        scan_timeout_secs: timeout,
        signal_lock_wait_ms,
        ts_read_timeout_ms,
    };
    web_state.update_scan_config(config.clone()).await;

    Ok(Json(json!({
        "success": true,
        "message": "Scan configuration saved successfully",
        "config": {
            "check_interval_secs": config.check_interval_secs,
            "max_concurrent_scans": config.max_concurrent_scans,
            "scan_timeout_secs": config.scan_timeout_secs,
            "signal_lock_wait_ms": config.signal_lock_wait_ms,
            "ts_read_timeout_ms": config.ts_read_timeout_ms,
        }
    })))
}
