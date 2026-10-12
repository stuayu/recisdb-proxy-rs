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

use crate::tuner::TunerPoolConfig;
use crate::web::state::WebState;

use super::error::ApiError;

// ============================================================================
// Legacy config endpoints (for backwards compatibility)
// ============================================================================

/// Legacy: Get server configuration.
pub async fn get_config(State(web_state): State<Arc<WebState>>) -> impl IntoResponse {
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
            .map_err(|e| {
                ApiError::internal(format!(
                    "Failed to update {}: {}",
                    driver_config.dll_path, e
                ))
            })?;

        // Update group_name if provided
        if let Some(group) = driver_config.group_name {
            db.set_group_name(driver_config.id, Some(&group))
                .map_err(|e| {
                    ApiError::internal(format!(
                        "Failed to update group_name for {}: {}",
                        driver_config.dll_path, e
                    ))
                })?;
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
    let (ts_queue_view_ms, ts_queue_preview_ms, ts_queue_record_ms) = db.get_ts_queue_config()?;
    let (min_hold_secs, reject_cooldown_ms, no_data_timeout_secs) =
        db.get_tuner_livelock_config()?;

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
            "min_hold_secs": min_hold_secs,
            "reject_cooldown_ms": reject_cooldown_ms,
            "no_data_timeout_secs": no_data_timeout_secs,
            "prefill_view_ms": prefill_view_ms,
            "prefill_preview_ms": prefill_preview_ms,
            "prefill_record_ms": prefill_record_ms,
            "jitter_safety_factor": jitter_safety_factor,
            "ts_queue_view_ms": ts_queue_view_ms,
            "ts_queue_preview_ms": ts_queue_preview_ms,
            "ts_queue_record_ms": ts_queue_record_ms,
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
    pub min_hold_secs: Option<u64>,
    pub reject_cooldown_ms: Option<u64>,
    pub no_data_timeout_secs: Option<u64>,
    /// STREAMING_DESIGN.md §4/§9 P3: fixed-duration prefill/jitter buffer.
    pub prefill_view_ms: Option<u64>,
    pub prefill_preview_ms: Option<u64>,
    pub prefill_record_ms: Option<u64>,
    pub jitter_safety_factor: Option<f64>,
    /// STREAMING_DESIGN.md §3.2: per-class TS write queue duration. The byte
    /// budget is derived from this and the measured bitrate, so the same value
    /// means the same amount of slack on any link.
    pub ts_queue_view_ms: Option<u64>,
    pub ts_queue_preview_ms: Option<u64>,
    pub ts_queue_record_ms: Option<u64>,
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
        min_hold_secs,
        reject_cooldown_ms,
        no_data_timeout_secs,
        prefill_view_ms,
        prefill_preview_ms,
        prefill_record_ms,
        jitter_safety_factor,
    ) = {
        let db = web_state.database.lock().await;
        let (mut min_hold_secs, mut reject_cooldown_ms, mut no_data_timeout_secs) =
            db.get_tuner_livelock_config().unwrap_or((10, 2_000, 30));

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
        ) = match db.get_tuner_config() {
            Ok(config) => config,
            Err(_) => (
                60, true, 30, 500, 10_000, 500, 10_000, 1000, 2000, 6000, 1.5,
            ),
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
        if let Some(val) = payload.min_hold_secs {
            if val > 0 {
                min_hold_secs = val;
            }
        }
        if let Some(val) = payload.reject_cooldown_ms {
            if val > 0 {
                reject_cooldown_ms = val;
            }
        }
        if let Some(val) = payload.no_data_timeout_secs {
            if val > 0 {
                no_data_timeout_secs = val;
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

        {
            let (mut view_ms, mut preview_ms, mut record_ms) =
                db.get_ts_queue_config().unwrap_or((8000, 12000, 15000));
            // 0 is rejected rather than accepted as "no buffering": it would
            // make every frame collide with the budget and turn a VIEW session
            // into a drop machine.
            if let Some(val) = payload.ts_queue_view_ms {
                if val > 0 {
                    view_ms = val;
                }
            }
            if let Some(val) = payload.ts_queue_preview_ms {
                if val > 0 {
                    preview_ms = val;
                }
            }
            if let Some(val) = payload.ts_queue_record_ms {
                if val > 0 {
                    record_ms = val;
                }
            }
            db.update_ts_queue_config(view_ms, preview_ms, record_ms)
                .map_err(|e| ApiError::internal(format!("Failed to save configuration: {}", e)))?;
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
        db.update_tuner_livelock_config(min_hold_secs, reject_cooldown_ms, no_data_timeout_secs)
            .map_err(|e| {
                ApiError::internal(format!("Failed to save livelock configuration: {}", e))
            })?;

        (
            keep_alive,
            prewarm_enabled,
            prewarm_timeout,
            set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms,
            signal_poll_interval_ms,
            signal_wait_timeout_ms,
            min_hold_secs,
            reject_cooldown_ms,
            no_data_timeout_secs,
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
        min_hold_secs,
        reject_cooldown_ms,
        no_data_timeout_secs,
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
        min_hold_secs,
        reject_cooldown_ms,
        no_data_timeout_secs,
        // Carried over, not rebuilt: the MMT/TLV converter comes from the
        // config file (it names an executable, so it is deliberately not
        // reachable from the Web API). Defaulting it here would silently
        // unconfigure 4K tuners the first time anyone saved this form.
        mmt_converter: web_state.tuner_pool.config().await.mmt_converter,
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
            "min_hold_secs": config.min_hold_secs,
            "reject_cooldown_ms": config.reject_cooldown_ms,
            "no_data_timeout_secs": config.no_data_timeout_secs,
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

    let (
        enabled,
        command_path,
        arguments,
        read_timeout_ms,
        passthrough_on_error,
        max_concurrent_encoders,
        preprocessor_path,
        preprocessor_arguments,
    ) = db.get_tsreplace_config()?;
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

    let (
        mut enabled,
        command_path,
        mut arguments,
        mut read_timeout_ms,
        mut passthrough_on_error,
        mut max_concurrent_encoders,
        preprocessor_path,
        mut preprocessor_arguments,
    ) = match db.get_tsreplace_config() {
        Ok(config) => config,
        Err(_) => (
            false,
            "tsreplace".to_string(),
            "".to_string(),
            10_000,
            true,
            2,
            "".to_string(),
            "".to_string(),
        ),
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
pub async fn get_preview_config(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

    let (
        mut enabled,
        command_path,
        preprocessor_path,
        mut preprocessor_arguments,
        mut read_timeout_ms,
    ) = db.get_preview_encoder_config()?;

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
pub async fn get_encode_profiles(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
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
    if payload.name.trim().is_empty()
        || payload.purpose.trim().is_empty()
        || payload.codec.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "name, purpose, and codec are required",
        ));
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
    Ok(Json(
        json!({ "success": true, "message": "Encode profile updated successfully" }),
    ))
}

/// Delete an encode profile.
pub async fn delete_encode_profile(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    db.delete_encode_profile(id)?;
    Ok(Json(
        json!({ "success": true, "message": "Encode profile deleted" }),
    ))
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

    let (
        mut interval,
        mut concurrent,
        mut timeout,
        mut signal_lock_wait_ms,
        mut ts_read_timeout_ms,
    ) = match db.get_scan_scheduler_config() {
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

// ============================================================================
// PC/SC card reader selection (`GET`/`POST /api/card-reader`)
// ============================================================================
//
// libaribb25 は「見つかったカードリーダーへ片っ端から接続を試し、最初に応答した
// ものを使う」実装しか持たない。B-CAS 以外のリーダー (銀行カード用の EMV など)
// が挿さっていると、そのリーダー1台につき十数秒待たされたうえ、間違った方が
// 選ばれることがある (macOS 実機で確認)。名前で名指しできるようにする。
//
// # Security
// ここで受け取るのは PC/SC が報告するリーダー名であって、実行ファイルのパスでは
// ない (libaribb25 はこの文字列をリーダー名の比較にしか使わず、プロセス起動には
// 一切関与しない)。それでも任意の文字列は受け付けず、**列挙結果に完全一致する
// 名前だけ**を通す。これは `[preview] command_path` 等がTOML専用である
// REVIEW S1 の趣旨 (APIから実行対象を差し込ませない) と同じ方向の制限。

/// 現在接続されている PC/SC カードリーダーと、選択中のリーダー名を返す。
pub async fn get_card_readers(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let selected = {
        let db = web_state.database.lock().await;
        db.get_card_reader_name()?
    };

    // PC/SC への問い合わせはブロッキング。
    let readers = tokio::task::spawn_blocking(b25_sys::list_card_readers)
        .await
        .map_err(|e| ApiError::internal(format!("failed to enumerate card readers: {e}")))?;

    // 選択済みのリーダーが今は外れている、という状態を UI が出せるようにする。
    let selected_present = selected.is_empty() || readers.iter().any(|r| r == &selected);

    Ok(Json(json!({
        "success": true,
        "readers": readers,
        "selected": selected,
        "selected_present": selected_present,
    })))
}

/// カードリーダー選択の更新リクエスト。
#[derive(Debug, Deserialize)]
pub struct UpdateCardReaderRequest {
    /// 空文字列 = 自動 (libaribb25 に全リーダーを試させる従来動作へ戻す)。
    pub name: String,
}

/// 使用するカードリーダーを選択する (`POST /api/card-reader`)。
///
/// 反映されるのは**次にリーダーを起動したとき**から。libaribb25 の
/// `override_card_reader_name_pattern` はプロセス全体の状態で、既に開いている
/// デコーダを作り直しはしないため。
pub async fn update_card_reader(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateCardReaderRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = payload.name.trim().to_string();

    if !name.is_empty() {
        let readers = tokio::task::spawn_blocking(b25_sys::list_card_readers)
            .await
            .map_err(|e| ApiError::internal(format!("failed to enumerate card readers: {e}")))?;
        if !readers.iter().any(|r| r == &name) {
            return Err(ApiError::bad_request(format!(
                "card reader {name:?} is not connected; pick one of {readers:?}"
            )));
        }
    }

    {
        let db = web_state.database.lock().await;
        db.set_card_reader_name(&name)?;
    }
    crate::apply_card_reader_selection(&name);

    Ok(Json(json!({
        "success": true,
        "message": if name.is_empty() {
            "card reader selection cleared (all readers will be tried)"
        } else {
            "card reader selected"
        },
        "selected": name,
    })))
}

#[derive(Debug, Deserialize)]
pub struct BonCasLinkUpdate {
    pub b25_server: String,
    pub mmttlv_server: String,
    pub network_readers: Vec<BonCasLinkNetworkReader>,
}
#[derive(Debug, Deserialize)]
pub struct BonCasLinkNetworkReader {
    pub nid: String,
    pub reader: String,
}
#[derive(Debug, Deserialize)]
pub struct BonCasLinkTestRequest {
    pub address: String,
}

fn valid_boncas_address(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let addr = raw.strip_prefix("boncaslink://").unwrap_or(raw);
    b25_sys::normalize_boncaslink_address(addr)
}

pub async fn get_boncaslink(
    State(state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = state.database.lock().await;
    let b25 = db.get_boncaslink_setting("b25")?;
    let mmttlv = db.get_boncaslink_setting("mmttlv")?;
    let has_readers = db.get_boncaslink_setting("network_readers")?.is_some();
    let configured = db.get_boncaslink_network_readers()?;
    drop(db);
    let pcsc = tokio::task::spawn_blocking(b25_sys::list_card_readers)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let toml = &state.boncaslink_toml;
    let resolved_b25 = crate::resolve_boncaslink_value(b25, toml.0.clone());
    let resolved_mmttlv = crate::resolve_boncaslink_value(mmttlv, toml.1.clone());
    let readers = if has_readers {
        configured
    } else {
        toml.2.clone()
    };
    let nids = readers.iter().map(|(nid,value)| json!({"nid":u16::from_str_radix(nid,16).unwrap_or(0),"value":value,"source":if has_readers{"db"}else{"toml"}})).collect::<Vec<_>>();
    Ok(Json(
        json!({"success":true,"b25":{"value":resolved_b25.value,"source":resolved_b25.source},"mmttlv":{"value":resolved_mmttlv.value,"source":resolved_mmttlv.source},"network_readers":nids,"readers":pcsc}),
    ))
}

pub async fn update_boncaslink(
    State(state): State<Arc<WebState>>,
    Json(payload): Json<BonCasLinkUpdate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let b25 = if payload.b25_server.trim().is_empty() {
        String::new()
    } else {
        valid_boncas_address(&payload.b25_server)
            .map_err(|e| ApiError::bad_request(format!("b25_server: {e}")))?
    };
    let fourk = if payload.mmttlv_server.trim().is_empty() {
        String::new()
    } else {
        valid_boncas_address(&payload.mmttlv_server)
            .map_err(|e| ApiError::bad_request(format!("mmttlv_server: {e}")))?
    };
    let mut rows = std::collections::HashMap::new();
    for row in payload.network_readers {
        let digits = row
            .nid
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        let nid = u16::from_str_radix(digits, 16).map_err(|_| {
            ApiError::bad_request(format!(
                "network_readers nid {:?} must be hexadecimal u16",
                row.nid
            ))
        })?;
        if digits.is_empty() || digits.len() > 4 {
            return Err(ApiError::bad_request(format!("network_readers nid {:?} must contain 1 to 4 hexadecimal digits", row.nid)));
        }
        let reader = row.reader.trim();
        if reader.is_empty() || reader.chars().count() >= 1024 || reader.as_bytes().contains(&0) {
            return Err(ApiError::bad_request(format!("network_readers {:04X}: reader must be non-empty, under 1024 bytes, and contain no NUL",nid)));
        }
        let reader = if let Some(addr) = reader.strip_prefix("boncaslink://") {
            format!(
                "boncaslink://{}",
                valid_boncas_address(addr).map_err(|e| ApiError::bad_request(format!(
                    "network_readers {:04X}: {e}",
                    nid
                )))?
            )
        } else {
            reader.to_owned()
        };
        if rows.insert(nid, reader).is_some() {
            return Err(ApiError::bad_request(format!(
                "duplicate network_readers NID {:04X}",
                nid
            )));
        }
    }
    {
        let db = state.database.lock().await;
        db.set_boncaslink_settings(&Some(b25.clone()), &Some(fourk.clone()), &rows)?;
    }
    b25_sys::set_boncaslink_server(if b25.is_empty() { None } else { Some(&b25) })
        .map_err(|e| ApiError::bad_request(e))?;
    let string_rows = rows
        .iter()
        .map(|(n, r)| (format!("{:04X}", n), r.clone()))
        .collect();
    crate::apply_network_card_reader_selections(&string_rows);
    state
        .tuner_pool
        .set_mmt_boncaslink_server(if fourk.is_empty() { None } else { Some(fourk) })
        .await;
    Ok(Json(
        json!({"success":true,"message":"Saved; settings apply when the next tuner starts."}),
    ))
}

pub async fn test_boncaslink(
    Json(payload): Json<BonCasLinkTestRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let address = valid_boncas_address(&payload.address)
        .map_err(|e| ApiError::bad_request(format!("address: {e}")))?;
    let result = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::task::spawn_blocking(move || boncaslink_tcp_connect(&address)),
    )
    .await
    {
        Ok(joined) => joined.map_err(|e| ApiError::internal(e.to_string()))?,
        Err(_) => Err("connection test timed out after 3 seconds".to_owned()),
    };
    Ok(Json(match result {
        Ok(()) => json!({"success":true,"ok":true}),
        Err(error) => json!({"success":true,"ok":false,"error":error}),
    }))
}

fn boncaslink_tcp_connect(address: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let endpoints = address
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .collect::<Vec<_>>();
    if endpoints.is_empty() {
        return Err("address resolved to no endpoints".to_owned());
    }
    let mut last = String::new();
    for endpoint in endpoints {
        match std::net::TcpStream::connect_timeout(&endpoint, std::time::Duration::from_secs(3)) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

#[cfg(test)]
mod boncaslink_connection_tests {
    use super::boncaslink_tcp_connect;
    use std::net::TcpListener;

    #[test]
    fn connects_to_a_mock_tcp_listener_without_sending_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let accept = std::thread::spawn(move || listener.accept().unwrap());
        boncaslink_tcp_connect(&address).unwrap();
        let (mut stream, _) = accept.join().unwrap();
        use std::io::Read;
        stream.set_nonblocking(true).unwrap();
        let mut byte = [0];
        let result = stream.read(&mut byte);
        assert!(match result {
            Ok(0) => true,
            Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
            _ => false,
        });
    }
}

// ============================================================================
// Browser-preview auto-setup (`POST /api/preview-config/auto-setup`)
// ============================================================================
//
// プレビューを使うには、これまで利用者が自分でエンコーダと tsreadex を用意し、
// recisdb-proxy.toml にパスを2つ書く必要があった。用意できていないと
// `?profile=preview` は 503 を返すだけで、何をすればいいのかは分からない。
// それを1操作で済ませる。
//
// # Security
// **このエンドポイントはリクエストボディを一切受け取らない。** 実行ファイルの
// パスは検出結果かダウンロード結果しか使わず、外から差し込めない。
// `[preview] command_path` が TOML 専用である理由 (REVIEW S1: APIから任意の
// プログラムを起動させない) をそのまま維持している。

/// エンコーダと前段処理を用意してプレビューを有効化する。
///
/// 検出 → (無ければ) ダウンロード → DB と TOML を更新 → 有効化。
/// ネットワークアクセスとプロセス起動を伴うため、完了までに時間がかかる。
pub async fn auto_setup_preview(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config_path = web_state.config_path.clone();
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let database = web_state.database.clone();

    // ダウンロード・展開・ビルド・テストエンコードはすべてブロッキング。
    // DB ロックもこの中で完結させる (blocking の中で await できないため、
    // 非同期 Mutex ではなく blocking_lock を使う)。
    let report = tokio::task::spawn_blocking(move || {
        let db = database.blocking_lock();
        crate::preview_setup::ensure_preview_ready(&db, &install_dir, config_path.as_deref())
    })
    .await
    .map_err(|e| ApiError::internal(format!("auto-setup task panicked: {e}")))?
    .map_err(ApiError::internal)?;

    Ok(Json(json!({
        "success": true,
        "report": {
            "enabled": report.enabled,
            "encoder_path": report.encoder_path,
            "encoder_source": report.encoder_source,
            "video_encoder": report.video_encoder,
            "preprocessor_path": report.preprocessor_path,
            "warnings": report.warnings,
        }
    })))
}
