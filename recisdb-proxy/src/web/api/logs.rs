//! Log-viewing API for the dashboard's "ログ" tab: incremental access to the
//! in-memory ring buffer (`logging/buffer.rs`) plus download access to the
//! rotated log files on disk.

use std::path::Path as StdPath;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::logging::{LogCategory, LogQuery};
use crate::web::state::WebState;

use super::error::ApiError;

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 2000;

/// `GET /api/logs` query parameters.
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub level: Option<String>,
    pub target: Option<String>,
    pub q: Option<String>,
    /// `"all"` (default when absent) / `"server"` (everything but the HTTP
    /// access log) / `"access"` (only the access log). Combines with
    /// `target` as AND — see [`LogCategory`]. Unknown values fall back to
    /// `"all"` rather than erroring.
    pub category: Option<String>,
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

/// `GET /api/logs` — recent/incremental log lines from the in-memory ring
/// buffer.
pub async fn get_logs(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<LogsQuery>,
) -> Json<serde_json::Value> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let result = web_state.log_buffer.query(LogQuery {
        level: query.level.as_deref(),
        target: query.target.as_deref(),
        q: query.q.as_deref(),
        category: LogCategory::parse(query.category.as_deref()),
        after_seq: query.after_seq.unwrap_or(0),
        limit,
    });
    Json(json!({
        "entries": result.entries,
        "last_seq": result.last_seq,
        "dropped": result.dropped,
    }))
}

/// One rotated log file, as listed by `GET /api/logs/files`.
#[derive(Debug, Serialize)]
struct LogFileInfo {
    name: String,
    size: u64,
    /// RFC3339, local time (matches `LogEntry::timestamp`).
    modified: Option<String>,
}

/// `recisdb-proxy.log.YYYY-MM-DD` (`logging.rs`'s daily-rolling appender).
/// Anything else in `log_dir` is not one of our files and is not listed.
const LOG_FILE_PREFIX: &str = "recisdb-proxy.log.";

fn is_log_file_name(name: &str) -> bool {
    // Reject path separators and `..` outright — this same predicate gates
    // both the listing (defense in depth) and the download path traversal
    // check below.
    name.starts_with(LOG_FILE_PREFIX)
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
}

/// `GET /api/logs/files` — list rotated log files in `log_dir`, newest
/// (by name, which sorts by date) first.
pub async fn list_log_files(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let log_dir = web_state.log_dir.clone();
    let files = tokio::task::spawn_blocking(move || read_log_files(&log_dir))
        .await
        .map_err(|e| ApiError::internal(format!("log file listing task panicked: {e}")))??;
    Ok(Json(json!({ "files": files })))
}

fn read_log_files(log_dir: &StdPath) -> Result<Vec<LogFileInfo>, ApiError> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(e) => e,
        // No log directory yet (e.g. fresh install, nothing logged to disk
        // this run) is not an error — just an empty list.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ApiError::internal(format!("failed to read log_dir: {e}"))),
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| ApiError::internal(format!("failed to read log_dir entry: {e}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_log_file_name(&name) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339());
        files.push(LogFileInfo {
            name,
            size: metadata.len(),
            modified,
        });
    }
    // Descending by name: the daily suffix (YYYY-MM-DD) sorts lexically the
    // same as chronologically, so this puts the newest file first.
    files.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(files)
}

/// `GET /api/logs/files/:name` — download one rotated log file.
///
/// # Path traversal (must-fix per spec)
/// `name` comes straight from the URL path segment, so before touching the
/// filesystem: reject anything containing a path separator or `..`, then
/// canonicalize the joined path and verify it is still inside the
/// canonicalized `log_dir`. Belt-and-suspenders — `is_log_file_name` already
/// requires a plain `recisdb-proxy.log.*` filename with no separators, so
/// the join can only ever produce a direct child of `log_dir`, but the
/// canonicalize check is cheap insurance against symlink surprises.
pub async fn download_log_file(
    State(web_state): State<Arc<WebState>>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    if !is_log_file_name(&name) {
        return Err(ApiError::bad_request("invalid log file name"));
    }

    let log_dir = web_state.log_dir.clone();
    let requested_name = name.clone();
    let result =
        tokio::task::spawn_blocking(move || read_log_file_for_download(&log_dir, &requested_name))
            .await
            .map_err(|e| ApiError::internal(format!("log file read task panicked: {e}")))?;
    let bytes = result?;

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let disposition = format!("attachment; filename=\"{name}\"");
    let value = HeaderValue::from_str(&disposition)
        .map_err(|_| ApiError::bad_request("invalid log file name"))?;
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, value);
    Ok(response)
}

fn read_log_file_for_download(log_dir: &StdPath, name: &str) -> Result<Vec<u8>, ApiError> {
    let candidate = log_dir.join(name);

    let canonical_dir = std::fs::canonicalize(log_dir)
        .map_err(|e| ApiError::internal(format!("failed to canonicalize log_dir: {e}")))?;
    let canonical_file =
        std::fs::canonicalize(&candidate).map_err(|_| ApiError::not_found("log file not found"))?;
    if !canonical_file.starts_with(&canonical_dir) {
        return Err(ApiError::bad_request("invalid log file name"));
    }
    // canonicalize() already proved this is a real, existing path under
    // log_dir; is_log_file_name() already proved the name has no separators
    // or "..", so the only thing left to check is that it's a regular file
    // (not e.g. a directory someone managed to name like a log file).
    let metadata = std::fs::metadata(&canonical_file)
        .map_err(|e| ApiError::internal(format!("failed to stat log file: {e}")))?;
    if !metadata.is_file() {
        return Err(ApiError::not_found("log file not found"));
    }

    std::fs::read(&canonical_file)
        .map_err(|e| ApiError::internal(format!("failed to read log file: {e}")))
}

/// `GET /api/log-config` — current log level/retention (`log_config` table,
/// `database/mod.rs` migration 022), for the dashboard's "設定 > ログ出力"
/// panel.
pub async fn get_log_config(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (level, retention_days) = {
        let db = web_state.database.lock().await;
        db.get_log_config()?
    };
    Ok(Json(json!({
        "success": true,
        "config": {
            "level": level,
            "retention_days": retention_days,
            // What the process is actually filtering on right now. Differs
            // from `level` only while RUST_LOG is in effect (startup keeps
            // RUST_LOG; the DB level is not applied then — see `main.rs`).
            "effective_level": web_state.log_level.current(),
            "env_override": web_state.log_level.env_override(),
        }
    })))
}

/// `POST /api/log-config` request body. Both fields optional — omitted
/// fields keep their current value.
#[derive(Debug, Deserialize)]
pub struct UpdateLogConfigRequest {
    pub level: Option<String>,
    pub retention_days: Option<u64>,
}

const MIN_RETENTION_DAYS: u64 = 1;
const MAX_RETENTION_DAYS: u64 = 365;

/// `POST /api/log-config` — update log level (applied immediately via
/// [`crate::logging::LogLevelHandle::set_level`], no restart needed) and/or
/// retention (persisted; takes effect on the next cleanup pass, triggered
/// here immediately so a shortened retention is visible right away).
pub async fn update_log_config(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<UpdateLogConfigRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The DB guard is scoped: `rotate_logs` below walks the log directory on
    // a blocking thread, and nothing else should have to wait on the database
    // for that.
    let db = web_state.database.lock().await;
    let (mut level, mut retention_days) = db.get_log_config()?;

    if let Some(requested) = &payload.level {
        // Validate + apply the reload *before* touching the DB: a rejected
        // level must not get persisted.
        web_state
            .log_level
            .set_level(requested)
            .map_err(ApiError::bad_request)?;
        level = web_state.log_level.current();
    }
    if let Some(requested) = payload.retention_days {
        if !(MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&requested) {
            return Err(ApiError::bad_request(format!(
                "retention_days must be between {MIN_RETENTION_DAYS} and {MAX_RETENTION_DAYS}"
            )));
        }
        retention_days = requested;
    }

    db.update_log_config(&level, retention_days)?;
    drop(db);

    // Apply the (possibly just-shortened) retention immediately, same as
    // `main.rs` does at startup — otherwise a lowered retention_days only
    // takes effect on the next server restart.
    let log_dir = web_state.log_dir.clone();
    if let Err(e) =
        tokio::task::spawn_blocking(move || crate::logging::rotate_logs(&log_dir, retention_days))
            .await
            .map_err(|e| ApiError::internal(format!("log rotation task panicked: {e}")))?
    {
        log::warn!("Failed to rotate logs after /api/log-config update: {e}");
    }

    Ok(Json(json!({
        "success": true,
        "config": {
            "level": level,
            "retention_days": retention_days,
            "effective_level": web_state.log_level.current(),
            "env_override": web_state.log_level.env_override(),
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_log_file_name() {
        assert!(is_log_file_name("recisdb-proxy.log.2026-07-19"));
    }

    #[test]
    fn rejects_names_without_the_expected_prefix() {
        assert!(!is_log_file_name("passwd"));
        assert!(!is_log_file_name("../recisdb-proxy.log.2026-07-19"));
        assert!(!is_log_file_name("recisdb-proxy.log"));
    }

    #[test]
    fn rejects_path_traversal_attempts() {
        assert!(!is_log_file_name(
            "recisdb-proxy.log.2026-07-19/../../etc/passwd"
        ));
        assert!(!is_log_file_name(
            "recisdb-proxy.log...%2f..%2fetc%2fpasswd"
        ));
        assert!(!is_log_file_name("..\\recisdb-proxy.log.2026-07-19"));
        assert!(!is_log_file_name(
            "recisdb-proxy.log.2026-07-19\\..\\..\\secret"
        ));
    }

    #[test]
    fn read_log_file_for_download_rejects_escaping_symlink() {
        let tmp = std::env::temp_dir().join(format!("recisdb-logs-test-{}", std::process::id()));
        let log_dir = tmp.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        // A file that lives outside log_dir…
        let outside = tmp.join("recisdb-proxy.log.2026-07-19");
        std::fs::write(&outside, b"secret").unwrap();
        // …reached via a same-named symlink placed inside log_dir.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = log_dir.join("recisdb-proxy.log.2026-07-19");
            symlink(&outside, &link).unwrap();
            let result = read_log_file_for_download(&log_dir, "recisdb-proxy.log.2026-07-19");
            assert!(result.is_err(), "symlink escaping log_dir must be rejected");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_log_file_for_download_reads_real_file() {
        let tmp = std::env::temp_dir().join(format!("recisdb-logs-test-ok-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("recisdb-proxy.log.2026-07-19");
        std::fs::write(&file, b"hello").unwrap();
        let result = read_log_file_for_download(&tmp, "recisdb-proxy.log.2026-07-19");
        assert_eq!(result.unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn list_log_files_returns_empty_for_missing_dir() {
        let missing =
            std::env::temp_dir().join(format!("recisdb-logs-missing-{}", std::process::id()));
        let result = read_log_files(&missing).unwrap();
        assert!(result.is_empty());
    }
}
