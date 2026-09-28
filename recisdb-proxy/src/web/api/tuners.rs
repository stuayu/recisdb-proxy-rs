//! Legacy tuner alias endpoint plus BonDriver quality/ranking endpoints.

use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;

use super::error::ApiError;

/// Legacy: Get all active tuners (alias for get_bondrivers).
pub async fn get_tuners(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = web_state.tuner_pool.keys().await;
    let mut runtime_by_path = std::collections::HashMap::new();
    for key in runtime {
        if let Some(tuner) = web_state.tuner_pool.get(&key).await {
            runtime_by_path
                .entry(key.tuner_path.clone())
                .or_insert(tuner);
        }
    }
    let db = web_state.database.lock().await;
    let drivers = db.get_all_bon_drivers()?;
    let tuners: Vec<serde_json::Value> = drivers
        .iter()
        .map(|d| {
            let runtime_tuner = runtime_by_path.get(&d.dll_path);
            let converter = runtime_tuner.and_then(|t| t.mmt_status());
            let startup_waiting = runtime_tuner
                .map(|t| t.awaiting_first_ts())
                .unwrap_or(false);
            let startup_active = startup_waiting
                || runtime_tuner.is_some_and(|t| {
                    matches!(
                        t.state(),
                        crate::tuner::shared::ReaderState::Reserved
                            | crate::tuner::shared::ReaderState::Starting
                    )
                });
            json!({
            "id": d.id,
            "dll_path": d.dll_path,
            "display_name": d.driver_name,
            "group_name": d.group_name,
            "max_instances": d.max_instances,
            "stream_format": db.driver_stream_format(&d.dll_path).as_db_value(),
                "state": runtime_tuner.map(|t| format!("{:?}", t.state())).unwrap_or_else(|| "Idle".to_string()),
                "startup_state": if startup_active { "starting" } else if runtime_tuner.is_some() { "streaming" } else { "idle" },
                "startup_elapsed_seconds": runtime_tuner.and_then(|t| t.startup_elapsed_ms()).map(|ms| ms / 1000).unwrap_or(0),
                "startup_grace_seconds": runtime_tuner.map(|t| t.startup_grace_ms() / 1000).unwrap_or(0),
                "startup_slow": runtime_tuner.map(|t| t.startup_slow()).unwrap_or(false),
                "mmt_converter": converter.map(|s| json!({
                "active": runtime_tuner.map(|t| matches!(t.state(), crate::tuner::shared::ReaderState::Starting | crate::tuner::shared::ReaderState::Running)).unwrap_or(false),
                "input_bytes": s.received_bytes(),
                "output_bytes": s.read_bytes(),
                "queued_chunks": s.queued_chunks(),
                "backlog_capacity": crate::tuner::mmt_pipe::ConverterStatus::backlog_capacity(),
                "dropped_chunks": s.dropped_chunks(),
                "descramble_errors": s.descramble_error_count(),
                "exit_code": s.process_exit_code(),
                "last_message": s.last_message()
            })).unwrap_or_else(|| json!({"active": false}))
            })
        })
        .collect();

    Ok(Json(json!({
        "success": true,
        "tuners": tuners,
        "count": tuners.len()
    })))
}

/// Get quality stats for a BonDriver.
pub async fn get_bondriver_quality(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    match db.get_driver_quality_stats(id)? {
        Some(stats) => Ok(Json(json!({
            "success": true,
            "stats": stats
        }))),
        None => Err(ApiError::not_found("Stats not found")),
    }
}

/// Get BonDriver ranking by quality score.
pub async fn get_bondrivers_ranking(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    let rows = db.get_bondrivers_ranking()?;
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
    Ok(Json(json!({
        "success": true,
        "items": items,
        "count": items.len()
    })))
}
