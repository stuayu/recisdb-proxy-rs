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
    pub brief: Option<bool>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub services: Option<String>,
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

/// How much of `description` the brief form keeps.
///
/// The grid cell shows a short summary under the title, so dropping the
/// description entirely would make the timetable look wrong. Sending it in
/// full is what made the response huge in the first place, so keep only what
/// a cell can physically display. A cell is at most ~150px wide and a handful
/// of lines tall; past roughly this many characters nothing more is visible.
const BRIEF_DESCRIPTION_CHARS: usize = 80;

/// Truncate on a character boundary (Japanese text is multi-byte, so slicing
/// by byte index would panic).
fn brief_description(value: Option<String>) -> Option<String> {
    let text = value?;
    let mut end = None;
    for (count, (index, _)) in text.char_indices().enumerate() {
        if count == BRIEF_DESCRIPTION_CHARS {
            end = Some(index);
            break;
        }
    }
    Some(match end {
        Some(index) => format!("{}…", &text[..index]),
        None => text,
    })
}

#[derive(Debug, Serialize)]
struct BriefProgramApi {
    id: i64,
    nid: u16,
    sid: u16,
    tsid: u16,
    event_id: u16,
    start_at: i64,
    duration_secs: i64,
    name: Option<String>,
    /// Shortened to [`BRIEF_DESCRIPTION_CHARS`]; the full text (and
    /// `extended`) come from a non-brief fetch when a program is opened.
    description: Option<String>,
    genre: Option<i64>,
}

impl From<ProgramRecord> for BriefProgramApi {
    fn from(r: ProgramRecord) -> Self {
        Self {
            id: r.id,
            nid: r.nid,
            sid: r.sid,
            tsid: r.tsid,
            event_id: r.event_id,
            start_at: r.start_at,
            duration_secs: r.duration_secs,
            name: r.name,
            description: brief_description(r.description),
            genre: r.genre,
        }
    }
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

    let services = query.services.as_deref().map(parse_services).transpose()?;
    let limit = query.limit.unwrap_or(20_000).min(50_000);
    let offset = query.offset.unwrap_or(0);
    let db = web_state.database.lock().await;
    let (records, total) = db.get_programs_page(
        since,
        until,
        query.nid,
        query.sid,
        services.as_deref().unwrap_or(&[]),
        limit,
        offset,
    )?;
    let truncated = offset as u64 + (records.len() as u64) < total;
    let programs = if query.brief.unwrap_or(false) {
        serde_json::to_value(
            records
                .into_iter()
                .map(BriefProgramApi::from)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| ApiError::internal(e.to_string()))?
    } else {
        serde_json::to_value(
            records
                .into_iter()
                .map(ProgramApi::from)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| ApiError::internal(e.to_string()))?
    };

    Ok(Json(serde_json::json!({
        "success": true,
        "count": programs.as_array().map_or(0, Vec::len),
        "total": total,
        "truncated": truncated,
        "programs": programs,
    })))
}

fn parse_services(value: &str) -> Result<Vec<(u16, u16)>, ApiError> {
    let mut result = Vec::new();
    for item in value.split(',').filter(|item| !item.trim().is_empty()) {
        let mut parts = item.trim().split(':');
        let (Some(nid), Some(sid), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(nid), Ok(sid)) = (nid.parse::<u16>(), sid.parse::<u16>()) else {
            continue;
        };
        result.push((nid, sid));
        if result.len() > 500 {
            return Err(ApiError::bad_request("servicesは500件以下にしてください"));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{brief_description, parse_services};

    #[test]
    fn brief_description_truncates_on_a_character_boundary() {
        // Multi-byte text: slicing by byte index would panic here.
        let long = "あ".repeat(200);
        let shortened = brief_description(Some(long)).unwrap();
        assert_eq!(shortened.chars().count(), super::BRIEF_DESCRIPTION_CHARS + 1);
        assert!(shortened.ends_with('…'));

        // Short enough to keep verbatim, with no ellipsis appended.
        let short = "短い説明".to_string();
        assert_eq!(brief_description(Some(short.clone())), Some(short));
        assert_eq!(brief_description(None), None);
    }


    #[test]
    fn services_ignores_malformed_items() {
        assert_eq!(
            parse_services("bad,1:2,,3:x,4:5:6,7:8").unwrap(),
            vec![(1, 2), (7, 8)]
        );
    }

    #[test]
    fn services_rejects_more_than_500_valid_items() {
        let value = (0..501)
            .map(|n| format!("1:{}", n))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_services(&value).is_err());
    }
}
