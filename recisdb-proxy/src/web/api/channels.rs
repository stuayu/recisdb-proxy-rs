//! Channel CRUD, CSV export/import, and batch-update endpoints.

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

use super::error::ApiError;

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
    // u16: CS110 rows carry the 3-digit channel number (= SID)
    pub remote_control_key: Option<u16>,
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
    // Metadata timestamps (0 when the source record does not carry them)
    pub created_at: i64,
    pub updated_at: i64,
    // Grouped channel info (only when group_logical=true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuner_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuner_names: Option<Vec<String>>,
}

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
) -> Result<Json<serde_json::Value>, ApiError> {
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
                        created_at: c.created_at,
                        updated_at: c.updated_at,
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
                                    // Keep the newest metadata timestamps
                                    if c.updated_at > existing.updated_at {
                                        existing.updated_at = c.updated_at;
                                    }
                                    if existing.created_at == 0 || (c.created_at != 0 && c.created_at < existing.created_at) {
                                        existing.created_at = c.created_at;
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
                                    created_at: c.created_at,
                                    updated_at: c.updated_at,
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
                        remote_control_key: c.remote_control_key.map(|v| v as u16),
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
                        // ClientChannelRecord does not carry these timestamps
                        created_at: 0,
                        updated_at: 0,
                        tuner_count: None,
                        tuner_names: None,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    };

    let infos = channel_infos.map_err(ApiError::internal)?;
    Ok(Json(json!({
        "success": true,
        "channels": infos,
        "count": infos.len()
    })))
}

/// Update channel request.
#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub channel_name: Option<String>,
    pub priority: Option<i32>,
    pub is_enabled: Option<bool>,
    // Extended fields
    pub bon_driver_id: Option<i64>,
    pub nid: Option<u16>,
    pub sid: Option<u16>,
    pub tsid: Option<u16>,
    /// null = clear, number = set
    pub bon_space: Option<Option<u32>>,
    pub bon_channel: Option<Option<u32>>,
}

/// Update channel.
pub async fn update_channel(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateChannelRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let has_any = payload.channel_name.is_some()
        || payload.priority.is_some()
        || payload.is_enabled.is_some()
        || payload.bon_driver_id.is_some()
        || payload.nid.is_some()
        || payload.sid.is_some()
        || payload.tsid.is_some()
        || payload.bon_space.is_some()
        || payload.bon_channel.is_some();

    if !has_any {
        return Err(ApiError::bad_request("No fields to update"));
    }

    db.update_channel_full(
        id,
        payload.channel_name.as_deref(),
        payload.priority,
        payload.is_enabled,
        payload.bon_driver_id,
        payload.nid,
        payload.sid,
        payload.tsid,
        payload.bon_space,
        payload.bon_channel,
    )?;
    Ok(Json(json!({ "success": true, "message": "Channel updated successfully" })))
}

/// Enable/disable channel.
pub async fn toggle_channel(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    let enabled = payload.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

    let result = if enabled {
        db.enable_channel(id)
    } else {
        db.disable_channel(id)
    };

    result?;
    Ok(Json(json!({
        "success": true,
        "message": if enabled { "Channel enabled" } else { "Channel disabled" }
    })))
}

/// Delete channel.
pub async fn delete_channel(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;

    db.delete_channel(id)?;
    Ok(Json(json!({
        "success": true,
        "message": "Channel deleted successfully"
    })))
}

// ============================================================================
// CSV helpers
// ============================================================================

/// RFC 4180 準拠の単純なCSVフィールドエスケープ。
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// RFC 4180 CSVを行・フィールドのVec<Vec<String>>に変換する。
fn parse_csv_rows(input: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut chars = input.chars().peekable();

    loop {
        let mut row = Vec::new();
        loop {
            // フィールド開始
            if chars.peek() == Some(&'"') {
                // quoted field
                chars.next(); // consume opening quote
                let mut field = String::new();
                loop {
                    match chars.next() {
                        None => break,
                        Some('"') => {
                            if chars.peek() == Some(&'"') {
                                chars.next();
                                field.push('"');
                            } else {
                                break; // closing quote
                            }
                        }
                        Some(c) => field.push(c),
                    }
                }
                row.push(field);
            } else {
                // unquoted field
                let mut field = String::new();
                loop {
                    match chars.peek() {
                        None | Some(&',') | Some(&'\n') | Some(&'\r') => break,
                        Some(_) => field.push(chars.next().unwrap()),
                    }
                }
                row.push(field);
            }
            // セパレータ or 行末
            match chars.peek() {
                Some(&',') => { chars.next(); }
                Some(&'\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') { chars.next(); }
                    break;
                }
                Some(&'\n') => { chars.next(); break; }
                None => break,
                _ => break,
            }
        }
        if row.iter().all(|f| f.is_empty()) && chars.peek().is_none() {
            break;
        }
        rows.push(row);
        if chars.peek().is_none() { break; }
    }
    rows
}

/// Export channels as CSV.
pub async fn export_channels(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    let rows = match db.get_all_channels_for_export() {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(CONTENT_TYPE, "text/plain")],
                format!("error: {}", e),
            ).into_response();
        }
    };

    let header = "id,bon_driver_id,nid,sid,tsid,channel_name,network_name,bon_space,bon_channel,band_type,terrestrial_region,priority,is_enabled\r\n";
    let mut csv = header.to_string();

    for (ch, _dll) in &rows {
        let line = format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{}\r\n",
            ch.id,
            ch.bon_driver_id,
            ch.nid,
            ch.sid,
            ch.tsid,
            csv_field(ch.channel_name.as_deref().unwrap_or("")),
            csv_field(ch.network_name.as_deref().unwrap_or("")),
            ch.bon_space.map_or(String::new(), |v| v.to_string()),
            ch.bon_channel.map_or(String::new(), |v| v.to_string()),
            ch.band_type.map_or(String::new(), |v| v.to_string()),
            csv_field(ch.terrestrial_region.as_deref().unwrap_or("")),
            ch.priority,
            if ch.is_enabled { "true" } else { "false" },
        );
        csv.push_str(&line);
    }

    use axum::http::header::{CONTENT_DISPOSITION, HeaderValue};
    let mut resp = axum::response::Response::new(axum::body::Body::from(csv));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/csv; charset=utf-8"));
    resp.headers_mut().insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"channels.csv\""));
    resp.into_response()
}

/// Import result summary.
#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub inserted: usize,
    pub updated: usize,
    pub errors: Vec<String>,
}

/// Import channels from CSV body (text/csv).
///
/// NOTE: the final response deliberately stays a plain 200
/// `{"success": ..., "inserted", "updated", "errors": [...]}` rather than an
/// `ApiError` — like a batch job, it can partially succeed (some rows
/// inserted/updated, others rejected), so there is no single HTTP status
/// that fits. Only the up-front, whole-request validation failures below
/// (bad encoding / empty body / missing required columns) are real 400s.
pub async fn import_channels(
    State(web_state): State<Arc<WebState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    use recisdb_protocol::ChannelInfo;

    let text = std::str::from_utf8(&body).map_err(|_| ApiError::bad_request("invalid UTF-8"))?;

    let all_rows = parse_csv_rows(text);
    if all_rows.is_empty() {
        return Err(ApiError::bad_request("empty CSV"));
    }

    // ヘッダー行からカラムインデックスを取得
    let headers: Vec<String> = all_rows[0].iter().map(|s| s.trim().to_lowercase()).collect();
    let col = |name: &str| -> Option<usize> { headers.iter().position(|h| h == name) };

    let col_id            = col("id");
    let col_bon_driver_id = col("bon_driver_id");
    let col_nid           = col("nid");
    let col_sid           = col("sid");
    let col_tsid          = col("tsid");
    let col_channel_name  = col("channel_name");
    let col_bon_space     = col("bon_space");
    let col_bon_channel   = col("bon_channel");
    let col_priority      = col("priority");
    let col_is_enabled    = col("is_enabled");

    // nid/sid/tsid は必須
    let (col_nid, col_sid, col_tsid) = match (col_nid, col_sid, col_tsid) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return Err(ApiError::bad_request("CSVにnid/sid/tsidカラムが必要です")),
    };

    let db = web_state.database.lock().await;

    let mut inserted = 0usize;
    let mut updated  = 0usize;
    let mut errors: Vec<String> = Vec::new();

    let get_field = |row: &Vec<String>, idx: Option<usize>| -> Option<String> {
        idx.and_then(|i| row.get(i)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };

    for (line_no, row) in all_rows.iter().skip(1).enumerate() {
        let line_no = line_no + 2; // 1-indexed, skip header

        // nid / sid / tsid をパース
        let nid = match get_field(row, Some(col_nid)).and_then(|s| s.parse::<u16>().ok()) {
            Some(v) => v,
            None => { errors.push(format!("行{}: nidが不正", line_no)); continue; }
        };
        let sid = match get_field(row, Some(col_sid)).and_then(|s| s.parse::<u16>().ok()) {
            Some(v) => v,
            None => { errors.push(format!("行{}: sidが不正", line_no)); continue; }
        };
        let tsid = match get_field(row, Some(col_tsid)).and_then(|s| s.parse::<u16>().ok()) {
            Some(v) => v,
            None => { errors.push(format!("行{}: tsidが不正", line_no)); continue; }
        };

        let channel_name = get_field(row, col_channel_name);
        let bon_driver_id = get_field(row, col_bon_driver_id).and_then(|s| s.parse::<i64>().ok());
        let bon_space    = get_field(row, col_bon_space).and_then(|s| s.parse::<u32>().ok());
        let bon_channel  = get_field(row, col_bon_channel).and_then(|s| s.parse::<u32>().ok());
        let bon_space_update = col_bon_space.map(|_| bon_space);
        let bon_channel_update = col_bon_channel.map(|_| bon_channel);
        let priority     = get_field(row, col_priority).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
        let is_enabled   = get_field(row, col_is_enabled)
            .map(|s| s == "true" || s == "1")
            .unwrap_or(true);

        // キー照合: まず id で検索、次に (bon_driver_id, nid, sid, tsid) で検索
        let existing_id: Option<i64> = {
            let id_val = get_field(row, col_id).and_then(|s| s.parse::<i64>().ok());
            if let Some(id) = id_val {
                match db.get_channel_by_id(id) {
                    Ok(Some(ch)) => Some(ch.id),
                    Ok(None) => {
                        // IDが指定されているが存在しない → 自然キーで再検索
                        if let Some(bd_id) = bon_driver_id {
                            db.get_channel_by_key(bd_id, nid, sid, tsid, None).ok().flatten().map(|c| c.id)
                        } else { None }
                    }
                    Err(_) => None,
                }
            } else {
                // id 未指定 → 自然キーで検索
                if let Some(bd_id) = bon_driver_id {
                    db.get_channel_by_key(bd_id, nid, sid, tsid, None).ok().flatten().map(|c| c.id)
                } else { None }
            }
        };

        if let Some(ch_id) = existing_id {
            // Update
            if let Err(e) = db.update_channel_full(
                ch_id,
                channel_name.as_deref(),
                Some(priority),
                Some(is_enabled),
                bon_driver_id,
                Some(nid),
                Some(sid),
                Some(tsid),
                bon_space_update,
                bon_channel_update,
            ) {
                errors.push(format!("行{}: 更新失敗 ({})", line_no, e));
            } else {
                updated += 1;
            }
        } else {
            // Insert — bon_driver_id 必須
            let bon_drv = match bon_driver_id {
                Some(v) => v,
                None => {
                    errors.push(format!("行{}: 新規登録にはbon_driver_idが必要です", line_no));
                    continue;
                }
            };
            let info = ChannelInfo {
                nid, sid, tsid,
                manual_sheet: None,
                raw_name: None,
                channel_name: channel_name.clone(),
                physical_ch: None,
                remote_control_key: None,
                service_type: None,
                network_name: None,
                bon_space,
                bon_channel,
                band_type: None,
                terrestrial_region: None,
            };
            match db.insert_channel(bon_drv, &info) {
                Ok(new_id) => {
                    let _ = db.update_channel_fields(new_id, None, Some(priority), Some(is_enabled));
                    inserted += 1;
                }
                Err(e) => errors.push(format!("行{}: 挿入失敗 ({})", line_no, e)),
            }
        }
    }

    Ok(Json(json!({
        "success": errors.is_empty() || inserted + updated > 0,
        "inserted": inserted,
        "updated": updated,
        "errors": errors
    })))
}

/// Create channel request.
#[derive(Debug, Deserialize)]
pub struct CreateChannelRequest {
    pub bon_driver_id: i64,
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub channel_name: Option<String>,
    pub bon_space: Option<u32>,
    pub bon_channel: Option<u32>,
    pub priority: Option<i32>,
    pub is_enabled: Option<bool>,
}

/// Create a new channel manually.
pub async fn create_channel(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<CreateChannelRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use recisdb_protocol::ChannelInfo;

    let db = web_state.database.lock().await;

    let info = ChannelInfo {
        nid: payload.nid,
        sid: payload.sid,
        tsid: payload.tsid,
        manual_sheet: None,
        raw_name: None,
        channel_name: payload.channel_name,
        physical_ch: None,
        remote_control_key: None,
        service_type: None,
        network_name: None,
        bon_space: payload.bon_space,
        bon_channel: payload.bon_channel,
        band_type: None,
        terrestrial_region: None,
    };

    let id = db.insert_channel(payload.bon_driver_id, &info)?;
    let priority = payload.priority.unwrap_or(0);
    let is_enabled = payload.is_enabled.unwrap_or(true);
    let _ = db.update_channel_fields(id, None, Some(priority), Some(is_enabled));
    Ok(Json(json!({
        "success": true,
        "id": id,
        "message": "Channel created successfully"
    })))
}

/// Batch update item.
#[derive(Debug, Deserialize)]
pub struct BatchUpdateItem {
    pub id: i64,
    pub channel_name: Option<String>,
    pub priority: Option<i32>,
    pub is_enabled: Option<bool>,
    pub deleted: Option<bool>,
    // Extended fields
    pub bon_driver_id: Option<i64>,
    pub nid: Option<u16>,
    pub sid: Option<u16>,
    pub tsid: Option<u16>,
    pub bon_space: Option<Option<u32>>,
    pub bon_channel: Option<Option<u32>>,
}

/// Batch update channels (update multiple channels at once).
///
/// NOTE: like `import_channels`, this aggregates multiple independent
/// per-item outcomes into one body, so it deliberately stays a plain 200
/// response rather than `ApiError` even when `errors` is non-empty.
pub async fn batch_update_channels(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<Vec<BatchUpdateItem>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;
    let mut errors = Vec::new();

    for item in &payload {
        if item.deleted.unwrap_or(false) {
            if let Err(e) = db.delete_channel(item.id) {
                errors.push(format!("id={}: {}", item.id, e));
            }
        } else {
            let has_any = item.channel_name.is_some()
                || item.priority.is_some()
                || item.is_enabled.is_some()
                || item.bon_driver_id.is_some()
                || item.nid.is_some()
                || item.sid.is_some()
                || item.tsid.is_some()
                || item.bon_space.is_some()
                || item.bon_channel.is_some();
            if has_any {
                if let Err(e) = db.update_channel_full(
                    item.id,
                    item.channel_name.as_deref(),
                    item.priority,
                    item.is_enabled,
                    item.bon_driver_id,
                    item.nid,
                    item.sid,
                    item.tsid,
                    item.bon_space,
                    item.bon_channel,
                ) {
                    errors.push(format!("id={}: {}", item.id, e));
                }
            }
        }
    }

    if errors.is_empty() {
        Json(json!({
            "success": true,
            "message": format!("{} 件を更新しました", payload.len())
        }))
    } else {
        Json(json!({
            "success": false,
            "error": errors.join("; ")
        }))
    }
}
