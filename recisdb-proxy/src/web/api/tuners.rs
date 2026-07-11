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
    let db = web_state.database.lock().await;

    let drivers = db.get_all_bon_drivers()?;
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
