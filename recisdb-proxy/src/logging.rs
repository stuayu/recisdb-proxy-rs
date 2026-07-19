//! Logging system with file output and log rotation.
//!
//! This module provides structured logging with both console and file output.
//! Log files are automatically rotated based on time, keeping only logs from
//! the last N days.

use std::io;
use std::path::Path;
use std::sync::Arc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use chrono::Local;
use std::fs;

mod buffer;
pub use buffer::{LogBuffer, LogBufferLayer, LogEntry, LogQuery, LogQueryResult, LOG_BUFFER_CAPACITY};

/// Initialize the logging system with both console and file output.
///
/// # Arguments
/// * `log_dir` - Directory where log files will be stored
/// * `retention_days` - Number of days to keep log files
/// * `verbose` - Whether to enable debug-level logging
/// * `level` - Log level override from config file (e.g. "warn", "info", "error")
///
/// # Returns
/// A tuple of:
/// - The [`WorkerGuard`] of the non-blocking file writer. The caller MUST
///   keep it alive for the whole program lifetime (e.g. `let _log_guard =
///   ...` in `main`): dropping it shuts the background writer thread down,
///   and its `Drop` is also what flushes any still-buffered lines on
///   graceful exit. (Previously the guard was `Box::leak`ed here, which kept
///   the writer alive but skipped the final flush-on-drop.)
/// - The shared [`LogBuffer`] handle, for the Web dashboard's "ログ" tab
///   (`web/api/logs.rs`). Pass it into `web::state::WebState` alongside the
///   other shared handles.
pub fn init_logging(
    log_dir: &Path,
    retention_days: u64,
    verbose: bool,
    level: Option<&str>,
) -> Result<(WorkerGuard, Arc<LogBuffer>), Box<dyn std::error::Error>> {
    // Create logs directory if it doesn't exist
    fs::create_dir_all(log_dir)?;

    // Clean up old log files
    clean_old_logs(log_dir, retention_days)?;

    // Create a file appender for daily rotation.
    // NOTE: rotation happens on the UTC date boundary (tracing-appender 0.2),
    // so file `recisdb-proxy.log.YYYY-MM-DD` covers 09:00 JST of that day
    // through 08:59 JST of the next.
    let file_appender = tracing_appender::rolling::daily(log_dir, "recisdb-proxy.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Priority: RUST_LOG env > --verbose flag > config file level > default "info"
    let default_level = if verbose {
        "debug"
    } else {
        level.unwrap_or("info")
    };
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    // In-memory ring buffer of recent log lines, for the Web dashboard's
    // "ログ" tab (web/api/logs.rs). See logging/buffer.rs's module doc for
    // why stacking it here (rather than giving it its own filter) is
    // sufficient: it only ever sees events the EnvFilter below already let
    // through, same as the two fmt::layer()s.
    let log_buffer = LogBuffer::new(LOG_BUFFER_CAPACITY);

    // Build the subscriber with both console and file output
    // Use tracing_log to bridge log:: macros to tracing
    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_writer(io::stdout)
                .with_target(true)
                .with_level(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_timer(LocalTimeTimer)
        )
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_target(true)
                .with_level(true)
                .with_thread_ids(true)
                .with_file(true)
                .with_line_number(true)
                .with_ansi(false)
                .with_timer(LocalTimeTimer)
        )
        .with(buffer::LogBufferLayer::new(Arc::clone(&log_buffer)));

    // Initialize tracing-log FIRST so `log::` macro records are bridged to
    // tracing from the very first event; this is also the order recommended
    // by tracing-log (set the LogTracer before the global subscriber).
    tracing_log::LogTracer::init()
        .map_err(|e| format!("Failed to initialize LogTracer: {}", e))?;

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| format!("Failed to set default subscriber: {}", e))?;

    Ok((guard, log_buffer))
}

/// Clean up log files older than the specified number of days.
fn clean_old_logs(log_dir: &Path, retention_days: u64) -> io::Result<()> {
    if !log_dir.exists() {
        return Ok(());
    }

    let now = Local::now();
    let cutoff = now - chrono::Duration::days(retention_days as i64);

    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            // Check if filename contains "recisdb-proxy.log"
            if let Some(filename) = path.file_name() {
                if let Some(filename_str) = filename.to_str() {
                    if filename_str.contains("recisdb-proxy.log") {
                        // Get file modification time
                        if let Ok(metadata) = entry.metadata() {
                            if let Ok(modified) = metadata.modified() {
                                let modified_datetime: chrono::DateTime<Local> = modified.into();
                                if modified_datetime < cutoff {
                                    if let Err(e) = fs::remove_file(&path) {
                                        eprintln!("Failed to remove old log file {:?}: {}", path, e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Optional: Manually trigger log rotation/cleanup.
/// Can be called periodically if needed.
pub fn rotate_logs(log_dir: &Path, retention_days: u64) -> io::Result<()> {
    clean_old_logs(log_dir, retention_days)
}

/// Custom timer for local time formatting in logs
#[derive(Debug, Clone, Copy)]
struct LocalTimeTimer;

impl fmt::time::FormatTime for LocalTimeTimer {
    fn format_time(&self, w: &mut fmt::format::Writer) -> std::fmt::Result {
        let now = Local::now();
        write!(w, "{}", now.format("%Y-%m-%dT%H:%M:%S%.6f"))
    }
}
