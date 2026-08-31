//! DB-backed EPG automatic collection settings.

use super::error::ApiError;
use crate::{
    database::{epg_reason, Database, EpgGlobalSettings, EpgReasonCode},
    web::state::WebState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
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

fn reason_values(
    db: &Database,
    states: &[crate::database::EpgScanState],
) -> Result<Vec<serde_json::Value>, crate::database::DatabaseError> {
    let mut labels = HashMap::new();
    for state in states {
        if labels.contains_key(&(state.network_id, state.tsid)) {
            continue;
        }
        let label = db
            .get_channels_by_nid_tsid(state.network_id, state.tsid)?
            .into_iter()
            .next()
            .and_then(|(channel, _)| channel.service_name);
        labels.insert((state.network_id, state.tsid), label);
    }

    Ok(reason_values_with_labels(states, &labels))
}

fn reason_values_with_labels(
    states: &[crate::database::EpgScanState],
    labels: &HashMap<(u16, u16), Option<String>>,
) -> Vec<serde_json::Value> {
    let mut entries = HashMap::new();
    let mut code_systems: HashMap<String, HashSet<(u16, u16)>> = HashMap::new();
    for state in states {
        let Some(value) = state
            .last_failure_reason
            .clone()
            .map(|reason| reason_value(Some(reason)))
        else {
            continue;
        };

        let mut codes = Vec::new();
        if let Some(code) = value
            .get("code")
            .and_then(|v| serde_json::from_value::<EpgReasonCode>(v.clone()).ok())
        {
            codes.push((
                code,
                value.get("details").cloned().unwrap_or_else(|| json!({})),
            ));
        }
        if let Some(additional) = value
            .get("details")
            .and_then(|details| details.get("additional_codes"))
            .and_then(serde_json::Value::as_array)
        {
            codes.extend(additional.iter().filter_map(|code| {
                serde_json::from_value::<EpgReasonCode>(code.clone())
                    .ok()
                    .map(|code| (code, json!({})))
            }));
        }

        let mut seen_codes = HashSet::new();
        for (code, details) in codes {
            let code_name = serde_json::to_string(&code).expect("EpgReasonCode serializes");
            if !seen_codes.insert(code_name.clone()) {
                continue;
            }
            code_systems
                .entry(code_name.clone())
                .or_default()
                .insert((state.network_id, state.tsid));
            entries
                .entry((code_name, state.network_id, state.tsid))
                .or_insert((
                    code,
                    details,
                    state.last_tuner_id,
                    state.last_node_id.clone(),
                ));
        }
    }

    let mut values: Vec<_> = entries
        .into_iter()
        .map(
            |((code, network_id, tsid), (code_enum, details, tuner_id, node_id))| {
                let count = code_systems.get(&code).map_or(0, HashSet::len);
                json!({
                    "code": code_enum,
                    "details": details,
                    "networkId": network_id,
                    "tsid": tsid,
                    "label": labels.get(&(network_id, tsid)).cloned().flatten(),
                    "tunerId": tuner_id,
                    "nodeId": node_id,
                    "count": count,
                })
            },
        )
        .collect();
    values.sort_by(|a, b| {
        let count = |v: &serde_json::Value| v.get("count").and_then(|n| n.as_u64()).unwrap_or(0);
        let number = |v: &serde_json::Value, key: &str| {
            v.get(key).and_then(|n| n.as_u64()).unwrap_or(u64::MAX)
        };
        count(b)
            .cmp(&count(a))
            .then_with(|| number(a, "networkId").cmp(&number(b, "networkId")))
            .then_with(|| number(a, "tsid").cmp(&number(b, "tsid")))
    });
    let mut per_code = HashMap::<String, usize>::new();
    values.retain(|value| {
        let code = value
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let shown = per_code.entry(code.to_owned()).or_default();
        if *shown < 3 {
            *shown += 1;
            true
        } else {
            false
        }
    });
    values
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
    let reasons = reason_values(&db, &states)?;
    let reason = states
        .iter()
        .find_map(|state| {
            state
                .last_failure_reason
                .clone()
                .map(|value| reason_value(Some(value)))
        })
        .unwrap_or(serde_json::Value::Null);
    let states_json =
        serde_json::to_value(states).map_err(|e| ApiError::internal(e.to_string()))?;
    let cpu_source = crate::scheduler::epg_scheduler::cpu_limit_source();
    Ok(Json(
        json!({"success":true,"summary":{"coverageUntil":minimum_coverage,"multiplexCount":states_json.as_array().map_or(0, |items| items.len())},"state":{"coverageUntil":minimum_coverage},"states":states_json,"active":active,"reason":reason,"reasons":reasons,"cpu":{"available":!cpu_source.starts_with("unavailable:"),"source":cpu_source}}),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::EpgScanState;

    fn state(network_id: u16, tsid: u16, reason: EpgReasonCode) -> EpgScanState {
        EpgScanState {
            network_id,
            tsid,
            last_scan_started_at: None,
            last_scan_completed_at: None,
            last_eit_received_at: None,
            coverage_until: None,
            next_eligible_at: None,
            last_tuner_id: None,
            last_node_id: None,
            failure_count: 1,
            last_failure_reason: Some(epg_reason(reason, json!({}))),
        }
    }

    #[test]
    fn reason_values_group_by_code_and_multiplex_with_metadata() {
        let mut first = state(20, 2, EpgReasonCode::NoTunerAvailable);
        first.last_tuner_id = Some(12);
        first.last_node_id = Some("sendai".to_owned());
        first.last_failure_reason = Some(epg_reason(
            EpgReasonCode::NoTunerAvailable,
            json!({"additional_codes": [EpgReasonCode::Backoff]}),
        ));
        let states = vec![
            first,
            state(10, 1, EpgReasonCode::NoTunerAvailable),
            state(30, 3, EpgReasonCode::Backoff),
        ];
        let labels = HashMap::from([
            ((20, 2), Some("二つ目".to_owned())),
            ((10, 1), Some("一つ目".to_owned())),
        ]);

        let values = reason_values_with_labels(&states, &labels);
        assert_eq!(values.len(), 4);
        assert_eq!(values[0]["code"], "no_tuner_available");
        assert_eq!(values[0]["networkId"], 10);
        assert_eq!(values[0]["count"], 2);
        let tuner_reason = values
            .iter()
            .find(|value| value["code"] == "no_tuner_available" && value["networkId"] == 20)
            .unwrap();
        assert_eq!(tuner_reason["label"], "二つ目");
        assert_eq!(tuner_reason["tunerId"], 12);
        assert_eq!(tuner_reason["nodeId"], "sendai");
        let backoff_reason = values
            .iter()
            .find(|value| value["code"] == "backoff")
            .unwrap();
        assert_eq!(backoff_reason["count"], 2);
        assert_eq!(
            values
                .iter()
                .find(|value| value["networkId"] == 30)
                .unwrap()["label"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn reason_values_limits_each_code_to_three_systems() {
        let states = (1..=4)
            .map(|nid| state(nid, nid, EpgReasonCode::ScanFailed))
            .collect::<Vec<_>>();
        let values = reason_values_with_labels(&states, &HashMap::new());
        assert_eq!(values.len(), 3);
        assert!(values.iter().all(|value| value["count"] == 4));
    }
}
