//! Logging system with file output and log rotation.
//!
//! This module provides structured logging with both console and file output.
//! Log files are automatically rotated based on time, keeping only logs from
//! the last N days.

use std::io;
use std::path::Path;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use chrono::Local;
use std::fs;
use std::sync::Arc;

/// Initialize the logging system with both console and file output.
///
/// # Arguments
/// * `log_dir` - Directory where log files will be stored
/// * `retention_days` - Number of days to keep log files
/// * `verbose` - Whether to enable debug-level logging
pub fn init_logging(
    log_dir: &Path,
    retention_days: u64,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create logs directory if it doesn't exist
    fs::create_dir_all(log_dir)?;

    // Clean up old log files
    clean_old_logs(log_dir, retention_days)?;

    // Create a file appender for daily rotation
    let file_appender = tracing_appender::rolling::daily(log_dir, "recisdb-proxy.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Wrap the guard in an Arc and leak it to keep it alive for the program lifetime
    let _ = Box::leak(Box::new(Arc::new(guard)));

    // Set up the filter
    let env_filter = if verbose {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"))
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

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
        );

    // Initialize with tracing and tracing-log to bridge log:: macros
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| format!("Failed to set default subscriber: {}", e))?;

    // Initialize tracing-log to bridge log:: macros to tracing
    tracing_log::LogTracer::init()
        .map_err(|e| format!("Failed to initialize LogTracer: {}", e))?;

    Ok(())
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
