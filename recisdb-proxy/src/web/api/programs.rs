//! EPG (program guide) dashboard endpoint: `GET /api/programs`.
//!
//! Reads from the `programs` table (Migration 015), populated by
//! `crate::epg_writer::EpgWriter` from live EIT collection
//! (`tuner/epg_collector.rs`). See `web/mirakurun.rs::get_programs` for the
//! Mirakurun-compatible equivalent.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::database::ProgramRecord;
use crate::web::state::WebState;

use super::error::ApiError;

/// Query parameters for `GET /api/programs`.
///
/// `since`/`until` are epoch seconds bounding the overlap window (a program
/// is included when `start_at < until AND start_at + duration_secs >
/// since`). Omitted bounds default to "unbounded" in that direction, so a
/// bare `GET /api/programs` returns everything currently stored.
#[derive(Debug, Deserialize)]
pub struct ProgramQuery {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub nid: Option<u16>,
    pub sid: Option<u16>,
}

/// A single program in the API response. Field names intentionally match
/// the `programs` table's column names verbatim (per design), so this is a
/// thin re-serialization of [`ProgramRecord`] rather than a distinct shape.
#[derive(Debug, Serialize)]
pub struct ProgramApi {
    pub id: i64,
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub event_id: u16,
    pub start_at: i64,
    pub duration_secs: i64,
    pub free_ca_mode: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub extended: Option<String>,
    pub genre: Option<i64>,
}

impl From<ProgramRecord> for ProgramApi {
    fn from(r: ProgramRecord) -> Self {
        Self {
            id: r.id,
            nid: r.nid,
            sid: r.sid,
            tsid: r.tsid,
            event_id: r.event_id,
            start_at: r.start_at,
            duration_secs: r.duration_secs,
            free_ca_mode: r.free_ca_mode,
            name: r.name,
            description: r.description,
            extended: r.extended,
            genre: r.genre,
        }
    }
}

/// `GET /api/programs?since=&until=&nid=&sid=`.
pub async fn get_programs(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<ProgramQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let since = query.since.unwrap_or(i64::MIN);
    let until = query.until.unwrap_or(i64::MAX);

    let db = web_state.database.lock().await;
    let programs: Vec<ProgramApi> = db
        .get_programs(since, until, query.nid, query.sid)?
        .into_iter()
        .map(ProgramApi::from)
        .collect();

    Ok(Json(serde_json::json!({
        "success": true,
        "count": programs.len(),
        "programs": programs,
    })))
}
