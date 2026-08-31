//! DB-backed EPG automatic collection settings.

use super::error::ApiError;
use crate::{
    database::{epg_reason, EpgGlobalSettings, EpgReasonCode},
    web::state::WebState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn reason_value(reason: Option<String>) -> serde_json::Value {
    let Some(reason) = reason else {
        return serde_json::Value::Null;
    };
    serde_json::from_str(&reason).unwrap_or_else(|_| {
        serde_json::from_str(&epg_reason(
            EpgReasonCode::ScanFailed,
            json!({"message":reason}),
        ))
        .unwrap_or(serde_json::Value::Null)
    })
}

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
#[derive(Debug, Deserialize, Default)]
pub struct CreatePreset {
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub target_refresh_secs: Option<i64>,
    #[serde(default)]
    pub max_stale_secs: Option<i64>,
    #[serde(default)]
    pub min_future_coverage_hours: Option<i64>,
    #[serde(default)]
    pub target_future_coverage_hours: Option<i64>,
    #[serde(default)]
    pub min_dwell_secs: Option<i64>,
    #[serde(default)]
    pub normal_dwell_secs: Option<i64>,
    #[serde(default)]
    pub max_dwell_secs: Option<i64>,
    #[serde(default)]
    pub idle_section_timeout_secs: Option<i64>,
    #[serde(default)]
    pub reserve_tuners: Option<bool>,
    #[serde(default)]
    pub prefer_local: Option<bool>,
    #[serde(default)]
    pub allow_remote: Option<bool>,
    #[serde(default)]
    pub preemptible: Option<bool>,
    #[serde(default)]
    pub cpu_soft_limit_percent: Option<i64>,
    #[serde(default)]
    pub cpu_hard_limit_percent: Option<i64>,
    #[serde(default)]
    pub remote_prefer_metadata_execution: Option<bool>,
    #[serde(default)]
    pub remote_allow_ts_transport: Option<bool>,
}

fn validate_preset(p: &CreatePreset) -> Result<(), ApiError> {
    if let (Some(min), Some(normal), Some(max)) =
        (p.min_dwell_secs, p.normal_dwell_secs, p.max_dwell_secs)
    {
        if !(min <= normal && normal <= max) {
            return Err(ApiError::bad_request("滞在時間の順序が不正です"));
        }
    }
    if let (Some(soft), Some(hard)) = (p.cpu_soft_limit_percent, p.cpu_hard_limit_percent) {
        if soft >= hard {
            return Err(ApiError::bad_request(
                "CPU soft limitはhard limit未満にしてください",
            ));
        }
    }
    if let (Some(min), Some(target)) = (p.min_future_coverage_hours, p.target_future_coverage_hours)
    {
        if min > target {
            return Err(ApiError::bad_request("coverageの下限が目標を超えています"));
        }
    }
    Ok(())
}

fn preset_bool(value: Option<bool>) -> Option<i64> {
    value.map(i64::from)
}
pub async fn create_epg_preset(
    State(s): State<Arc<WebState>>,
    Json(p): Json<CreatePreset>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if p.name.trim().is_empty() {
        return Err(ApiError::bad_request("プリセット名が空です"));
    }
    validate_preset(&p)?;
    let db = s.database.lock().await;
    db.connection()
        .execute(
            "INSERT INTO epg_scan_presets(name,description,is_system,enabled,target_refresh_secs,max_stale_secs,min_future_coverage_hours,target_future_coverage_hours,min_dwell_secs,normal_dwell_secs,max_dwell_secs,idle_section_timeout_secs,reserve_tuners,prefer_local,allow_remote,preemptible,cpu_soft_limit_percent,cpu_hard_limit_percent,remote_prefer_metadata_execution,remote_allow_ts_transport) VALUES(?1,?2,0,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            rusqlite::params![p.name.trim(), p.description.unwrap_or_default(), p.enabled.map(i64::from).unwrap_or(1), p.target_refresh_secs, p.max_stale_secs, p.min_future_coverage_hours, p.target_future_coverage_hours, p.min_dwell_secs, p.normal_dwell_secs, p.max_dwell_secs, p.idle_section_timeout_secs, preset_bool(p.reserve_tuners), preset_bool(p.prefer_local), preset_bool(p.allow_remote), preset_bool(p.preemptible), p.cpu_soft_limit_percent, p.cpu_hard_limit_percent, preset_bool(p.remote_prefer_metadata_execution), preset_bool(p.remote_allow_ts_transport)],
        )
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({"success":true})))
}
pub async fn update_epg_preset(
    State(s): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(p): Json<CreatePreset>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if p.name.trim().is_empty() {
        return Err(ApiError::bad_request("プリセット名が空です"));
    }
    validate_preset(&p)?;
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
    db.connection().execute("UPDATE epg_scan_presets SET name=?1,description=?2,enabled=?3,target_refresh_secs=?4,max_stale_secs=?5,min_future_coverage_hours=?6,target_future_coverage_hours=?7,min_dwell_secs=?8,normal_dwell_secs=?9,max_dwell_secs=?10,idle_section_timeout_secs=?11,reserve_tuners=?12,prefer_local=?13,allow_remote=?14,preemptible=?15,cpu_soft_limit_percent=?16,cpu_hard_limit_percent=?17,remote_prefer_metadata_execution=?18,remote_allow_ts_transport=?19,updated_at=strftime('%s','now') WHERE id=?20",rusqlite::params![p.name.trim(),p.description.unwrap_or_default(),p.enabled.map(i64::from).unwrap_or(1),p.target_refresh_secs,p.max_stale_secs,p.min_future_coverage_hours,p.target_future_coverage_hours,p.min_dwell_secs,p.normal_dwell_secs,p.max_dwell_secs,p.idle_section_timeout_secs,preset_bool(p.reserve_tuners),preset_bool(p.prefer_local),preset_bool(p.allow_remote),preset_bool(p.preemptible),p.cpu_soft_limit_percent,p.cpu_hard_limit_percent,preset_bool(p.remote_prefer_metadata_execution),preset_bool(p.remote_allow_ts_transport),id]).map_err(|e|ApiError::bad_request(e.to_string()))?;
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
    let in_use: i64 = db.connection().query_row(
        "SELECT COUNT(*) FROM physical_tuner_epg_settings WHERE preset_id=?",
        [id],
        |r| r.get(0),
    )?;
    db.connection()
        .execute_batch(&format!(
            "BEGIN IMMEDIATE;
         UPDATE physical_tuner_epg_settings SET preset_id=NULL WHERE preset_id={id};
         UPDATE epg_global_settings SET selected_preset_id=NULL WHERE selected_preset_id={id};
         DELETE FROM epg_scan_presets WHERE id={id};
         COMMIT;"
        ))
        .map_err(crate::database::DatabaseError::from)?;
    Ok(Json(json!({"success":true,"releasedTuners":in_use})))
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
    let states = db.get_epg_scan_states()?;
    let minimum_coverage = states.iter().filter_map(|state| state.coverage_until).min();
    let active: bool = db.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM epg_scan_history WHERE status='running')",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0;
    let reason = states
        .iter()
        .find_map(|state| state.last_failure_reason.clone())
        .map(|value| reason_value(Some(value)))
        .unwrap_or(serde_json::Value::Null);
    let states_json =
        serde_json::to_value(states).map_err(|e| ApiError::internal(e.to_string()))?;
    let cpu_source = crate::scheduler::epg_scheduler::cpu_limit_source();
    Ok(Json(
        json!({"success":true,"summary":{"coverageUntil":minimum_coverage,"multiplexCount":states_json.as_array().map_or(0, |items| items.len())},"state":{"coverageUntil":minimum_coverage},"states":states_json,"active":active,"reason":reason,"reasons":if reason.is_null(){vec![]}else{vec![reason.clone()]},"cpu":{"available":!cpu_source.starts_with("unavailable:"),"source":cpu_source}}),
    ))
}

pub async fn get_epg_scan_history(
    State(s): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = s.database.lock().await;
    let mut st = db.connection().prepare("SELECT id,started_at,finished_at,status,reason,physical_tuner_id,node_id,network_id,tsid,coverage_before,coverage_after,error FROM epg_scan_history ORDER BY started_at DESC LIMIT 100")?;
    let rows = st.query_map([], |r| Ok(json!({"id":r.get::<_,i64>(0)?,"startedAt":r.get::<_,i64>(1)?,"finishedAt":r.get::<_,Option<i64>>(2)?,"status":r.get::<_,String>(3)?,"reason":reason_value(r.get::<_,Option<String>>(4)?),"physicalTunerId":r.get::<_,Option<i64>>(5)?,"nodeId":r.get::<_,Option<String>>(6)?,"networkId":r.get::<_,Option<i64>>(7)?,"tsid":r.get::<_,Option<i64>>(8)?,"coverageBefore":r.get::<_,Option<i64>>(9)?,"coverageAfter":r.get::<_,Option<i64>>(10)?,"error":r.get::<_,Option<String>>(11)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(json!({"success":true,"scans":rows})))
}
