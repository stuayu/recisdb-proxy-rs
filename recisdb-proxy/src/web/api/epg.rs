//! DB-backed EPG automatic collection settings.

use super::error::ApiError;
use crate::{database::EpgGlobalSettings, web::state::WebState};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub async fn get_epg_settings(
    State(s): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    Ok(Json(
        json!({"success":true,"config":db.get_epg_global_settings()?}),
    ))
}
pub async fn update_epg_settings(
    State(s): State<Arc<WebState>>,
    Json(c): Json<EpgGlobalSettings>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    db.update_epg_global_settings(&c)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(
        json!({"success":true,"message":"EPG設定を保存しました。次回の判定から適用されます。","config":db.get_epg_global_settings()?}),
    ))
}
pub async fn get_epg_presets(
    State(s): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    Ok(Json(
        json!({"success":true,"presets":db.list_epg_presets()?}),
    ))
}
pub async fn get_epg_preset(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    let p = db
        .list_epg_presets()?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("Preset not found"))?;
    Ok(Json(json!({"success":true,"preset":p})))
}
#[derive(Debug, Deserialize)]
pub struct CreatePreset {
    pub name: String,
    pub description: Option<String>,
}
pub async fn create_epg_preset(
    State(s): State<Arc<WebState>>,
    Json(p): Json<CreatePreset>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if p.name.trim().is_empty() {
        return Err(ApiError::bad_request("プリセット名が空です"));
    }
    let db = s.database.lock().await;
    db.connection()
        .execute(
            "INSERT INTO epg_scan_presets(name,description,is_system) VALUES(?1,?2,0)",
            rusqlite::params![p.name.trim(), p.description.unwrap_or_default()],
        )
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({"success":true})))
}
pub async fn update_epg_preset(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(p): Json<CreatePreset>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    let system: bool = db
        .connection()
        .query_row(
            "SELECT is_system FROM epg_scan_presets WHERE id=?",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .map_err(|_| ApiError::not_found("Preset not found"))?;
    if system {
        return Err(ApiError::bad_request(
            "system presetは複製して編集してください",
        ));
    }
    db.connection().execute("UPDATE epg_scan_presets SET name=?1,description=?2,updated_at=strftime('%s','now') WHERE id=?3",rusqlite::params![p.name.trim(),p.description.unwrap_or_default(),id]).map_err(|e|ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({"success":true})))
}
pub async fn delete_epg_preset(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    let system: bool = db
        .connection()
        .query_row(
            "SELECT is_system FROM epg_scan_presets WHERE id=?",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .map_err(|_| ApiError::not_found("Preset not found"))?;
    if system {
        return Err(ApiError::bad_request("system presetは削除できません"));
    }
    db.connection()
        .execute("DELETE FROM epg_scan_presets WHERE id=?", [id])
        .map_err(crate::database::DatabaseError::from)?;
    Ok(Json(json!({"success":true})))
}
pub async fn get_epg_effective(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    Ok(Json(
        serde_json::to_value(db.get_epg_effective(Some(id))?)
            .map_err(|e| ApiError::internal(e.to_string()))?,
    ))
}
pub async fn get_tuner_epg_settings(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    Ok(Json(
        json!({"settings":db.get_physical_tuner_epg_settings(id)?,"effective":db.get_epg_effective(Some(id))?}),
    ))
}
pub async fn update_tuner_epg_settings(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(mut c): Json<crate::database::PhysicalTunerEpgSettings>,
) -> Result<Json<serde_json::Value>, ApiError> {
    c.physical_tuner_id = id;
    let db = s.database.lock().await;
    db.update_physical_tuner_epg_settings(&c)?;
    Ok(Json(
        json!({"success":true,"settings":db.get_physical_tuner_epg_settings(id)?,"effective":db.get_epg_effective(Some(id))?}),
    ))
}

pub async fn get_epg_status(
    State(s): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    let state: serde_json::Value = db.connection().query_row(
        "SELECT last_scan_started_at,last_scan_completed_at,last_eit_received_at,coverage_until,next_eligible_at,last_tuner_id,last_node_id,failure_count,last_failure_reason FROM epg_scan_states WHERE id=1", [],
        |r| Ok(json!({"lastScanStartedAt":r.get::<_,Option<i64>>(0)?,"lastScanCompletedAt":r.get::<_,Option<i64>>(1)?,"lastEitReceivedAt":r.get::<_,Option<i64>>(2)?,"coverageUntil":r.get::<_,Option<i64>>(3)?,"nextEligibleAt":r.get::<_,Option<i64>>(4)?,"lastTunerId":r.get::<_,Option<i64>>(5)?,"lastNodeId":r.get::<_,Option<String>>(6)?,"failureCount":r.get::<_,i64>(7)?,"lastFailureReason":r.get::<_,Option<String>>(8)?})),
    )?;
    let active: bool = db.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM epg_scan_history WHERE status='running')",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0;
    let reason = state
        .get("lastFailureReason")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(Json(
        json!({"success":true,"state":state,"active":active,"reason":reason}),
    ))
}

pub async fn get_epg_scan_history(
    State(s): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    let mut st = db.connection().prepare("SELECT id,started_at,finished_at,status,reason,physical_tuner_id,node_id,network_id,tsid,coverage_before,coverage_after,error FROM epg_scan_history ORDER BY started_at DESC LIMIT 100")?;
    let rows = st.query_map([], |r| Ok(json!({"id":r.get::<_,i64>(0)?,"startedAt":r.get::<_,i64>(1)?,"finishedAt":r.get::<_,Option<i64>>(2)?,"status":r.get::<_,String>(3)?,"reason":r.get::<_,Option<String>>(4)?,"physicalTunerId":r.get::<_,Option<i64>>(5)?,"nodeId":r.get::<_,Option<String>>(6)?,"networkId":r.get::<_,Option<i64>>(7)?,"tsid":r.get::<_,Option<i64>>(8)?,"coverageBefore":r.get::<_,Option<i64>>(9)?,"coverageAfter":r.get::<_,Option<i64>>(10)?,"error":r.get::<_,Option<String>>(11)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(json!({"success":true,"scans":rows})))
}
