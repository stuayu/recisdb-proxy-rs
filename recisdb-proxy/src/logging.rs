//! Logging system with file output and log rotation.
//!
//! This module provides structured logging with both console and file output.
//! Log files are automatically rotated based on time, keeping only logs from
//! the last N days.
//!
//! The log level is **not** read from the TOML config file: the `[logging]`
//! section was removed in favor of a DB-backed setting (`log_config` table,
//! `database/mod.rs` migration) editable from the Web dashboard ("設定 > ログ
//! 出力"). The level effective at any moment is whatever was last applied via
//! [`LogLevelHandle::set_level`] — the DB is the source of truth, and
//! `main.rs` applies it once right after opening the database. This module
//! only decides the level to use *before* the DB is available (RUST_LOG >
//! `--verbose` > `"info"`), via a `tracing_subscriber::reload` layer that lets
//! the level change at runtime without restarting the process.

use chrono::Local;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, reload, EnvFilter, Registry};

mod buffer;
pub use buffer::{
    LogBuffer, LogBufferLayer, LogCategory, LogEntry, LogQuery, LogQueryResult, ACCESS_LOG_TARGET,
    LOG_BUFFER_CAPACITY,
};

/// Log levels accepted by [`LogLevelHandle::set_level`] and the `/api/log-config`
/// Web endpoint. Anything else (including `"off"`, which `EnvFilter` itself
/// would accept) is rejected — the dashboard select only offers these five.
const VALID_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// Runtime handle to the global log level, backed by a
/// `tracing_subscriber::reload` layer so the level can change without a
/// process restart. Held by `WebState` and used by the `/api/log-config`
/// handlers (`web/api/logs.rs`).
pub struct LogLevelHandle {
    handle: reload::Handle<EnvFilter, Registry>,
    current: Mutex<String>,
    /// Whether `RUST_LOG` was set at startup. When true, the effective
    /// filter is whatever `RUST_LOG` specified (a full directive string,
    /// possibly per-module), not just `current` — surfaced to the dashboard
    /// so it can explain why a level change might look like it "didn't
    /// stick" for some modules.
    env_override: bool,
}

impl LogLevelHandle {
    /// The filter currently in effect: the level last applied via
    /// [`Self::set_level`], or — until then, when [`Self::env_override`] is
    /// true — the raw `RUST_LOG` directive string, which may name modules
    /// individually and so need not be one of the five canonical levels.
    pub fn current(&self) -> String {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether `RUST_LOG` was present (and non-empty) in the environment at
    /// startup. It takes priority as the initial filter — `main.rs` skips
    /// applying the DB level in that case — but [`Self::set_level`] can still
    /// override it afterward like any other reload.
    pub fn env_override(&self) -> bool {
        self.env_override
    }

    /// Validate, apply, and remember a new log level. Rejects anything but
    /// `trace|debug|info|warn|error` (case-insensitive) — the dashboard
    /// exposes only these five, so we don't need to accept arbitrary
    /// `EnvFilter` directive syntax here.
    pub fn set_level(&self, level: &str) -> Result<(), String> {
        let normalized = level.trim().to_ascii_lowercase();
        if !VALID_LEVELS.contains(&normalized.as_str()) {
            return Err(format!(
                "invalid log level '{level}': must be one of {}",
                VALID_LEVELS.join(", ")
            ));
        }
        let filter = EnvFilter::new(&normalized);
        self.handle
            .reload(filter)
            .map_err(|e| format!("failed to reload log filter: {e}"))?;
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = normalized;
        Ok(())
    }
}

/// A [`LogLevelHandle`] not wired to the process-global subscriber, for unit
/// tests that need a `WebState` (`web/mod.rs`, `web/stream.rs` test helpers)
/// without calling [`init_logging`] (which can only run once per process —
/// `tracing::subscriber::set_global_default` errors on a second call).
/// `reload::Handle` itself has no dependency on a global subscriber being
/// set, so `set_level`/`current`/`env_override` all behave normally; the
/// resulting filter changes just never apply to any live subscriber.
#[cfg(test)]
pub fn test_handle() -> Arc<LogLevelHandle> {
    let (layer, handle) = reload::Layer::new(EnvFilter::new("info"));
    // `reload::Handle` only holds a `Weak` reference into the `Layer` it was
    // created from (see reload.rs upstream): if the `Layer` half is dropped,
    // every `handle.reload(...)` call fails with `SubscriberGone`. Nothing
    // in these tests ever attaches `layer` to a subscriber, so leak it to
    // keep the `Arc` alive for the life of the test process — acceptable
    // since this is `#[cfg(test)]`-only.
    Box::leak(Box::new(layer));
    Arc::new(LogLevelHandle {
        handle,
        current: Mutex::new("info".to_string()),
        env_override: false,
    })
}

/// Initialize the logging system with both console and file output.
///
/// # Arguments
/// * `log_dir` - Directory where log files will be stored
/// * `retention_days` - Number of days to keep log files (used only for the
///   startup cleanup pass here; the DB value, once loaded, is what governs
///   later cleanups — see `logging::rotate_logs` calls in `main.rs`)
/// * `verbose` - Whether to enable debug-level logging
///
/// The initial level is resolved as RUST_LOG (if set) > `--verbose` (debug) >
/// `"info"`. `main.rs` overwrites it right after opening the database with
/// whatever `log_config` says, via the returned [`LogLevelHandle`].
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
/// - The shared [`LogLevelHandle`], for runtime level changes from the Web
///   dashboard and for `main.rs` to apply the DB-configured level at startup.
/// 強制終了の直前にログを吐き切るための置き場所。
///
/// ファイル出力は `tracing_appender::non_blocking` なので、まだ書けていない
/// 行はワーカースレッド側のキューに残る。`WorkerGuard` の `Drop` がその
/// フラッシュを行うため、`std::process::exit` のように Drop を走らせない
/// 終わり方をすると**停止時のログがまるごと消える**。
///
/// 実際、Windows サービスの強制停止では「停止要求を受けた」というログすら
/// 残らず、原因調査を空振りさせていた。強制終了する経路は落ちる前に
/// [`flush_log_writer`] を呼ぶこと。
static EXIT_FLUSH_GUARD: std::sync::Mutex<Option<WorkerGuard>> = std::sync::Mutex::new(None);

/// [`EXIT_FLUSH_GUARD`] に `WorkerGuard` の複製を預ける…ことはできない
/// (`WorkerGuard` は `Clone` ではない) ので、代わりに **所有権を預ける**。
/// 呼び出し側は戻り値の guard を保持し続ける必要がなくなる。
///
/// `main` が `let _log_guard = ...` で持ち続ける従来の使い方と併用できる
/// よう、預けるかどうかは呼び出し側が選ぶ。
pub fn park_log_guard_for_exit(guard: WorkerGuard) {
    if let Ok(mut slot) = EXIT_FLUSH_GUARD.lock() {
        *slot = Some(guard);
    }
}

/// フラッシュを待つ上限。これを過ぎたら書き切れていなくても先へ進む。
const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// 預けた `WorkerGuard` を落として、バッファに残ったログを書き切る。
/// `std::process::exit` の直前に呼ぶ。二回呼んでも安全 (二回目は何もしない)。
///
/// **待ちには必ず上限を置く。** `WorkerGuard` の `Drop` は書き込みワーカーが
/// キューを空にするまで待つが、停止時には `Runtime::shutdown_timeout` が
/// 見切りをつけた BonDriver リーダースレッドがまだ生きていて、ログを吐き
/// 続けていることがある。そうなるとキューは空にならず Drop が返らない。
/// 実際、本番でサービスが STOPPED を報告したあともプロセスが 30 秒以上
/// 残り続けた原因がこれだった。書き切れないログより、終われないプロセスの
/// 方が害が大きい。
pub fn flush_log_writer() {
    let Ok(mut slot) = EXIT_FLUSH_GUARD.lock() else {
        return;
    };
    let Some(guard) = slot.take() else {
        return;
    };
    // Drop を別スレッドでやらせ、こちらは期限付きで待つ。時間切れの場合、
    // そのスレッドは置き去りになるが、直後にプロセスごと終わるので問題ない。
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(guard);
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(FLUSH_TIMEOUT);
}

pub fn init_logging(
    log_dir: &Path,
    retention_days: u64,
    verbose: bool,
) -> Result<(WorkerGuard, Arc<LogBuffer>, Arc<LogLevelHandle>), Box<dyn std::error::Error>> {
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

    // Priority: RUST_LOG env > --verbose flag > default "info". The DB-backed
    // level (`log_config` table) is applied afterward by `main.rs` via the
    // returned `LogLevelHandle`, once the database is open.
    let rust_log = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let default_level = if verbose { "debug" } else { "info" };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    // `current()` reports what is actually in effect, so when RUST_LOG is set
    // it holds that directive string verbatim (which may be per-module, e.g.
    // `recisdb_proxy::tuner=debug,info`) rather than one of the five
    // canonical level words. `env_override()` is what tells callers to expect
    // that.
    let (env_filter, reload_handle) = reload::Layer::new(env_filter);
    let log_level_handle = Arc::new(LogLevelHandle {
        handle: reload_handle,
        current: Mutex::new(
            rust_log
                .clone()
                .unwrap_or_else(|| default_level.to_string()),
        ),
        env_override: rust_log.is_some(),
    });

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
                .with_timer(LocalTimeTimer),
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
                .with_timer(LocalTimeTimer),
        )
        .with(buffer::LogBufferLayer::new(Arc::clone(&log_buffer)));

    // Initialize tracing-log FIRST so `log::` macro records are bridged to
    // tracing from the very first event; this is also the order recommended
    // by tracing-log (set the LogTracer before the global subscriber).
    tracing_log::LogTracer::init().map_err(|e| format!("Failed to initialize LogTracer: {}", e))?;

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| format!("Failed to set default subscriber: {}", e))?;

    Ok((guard, log_buffer, log_level_handle))
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
                                        eprintln!(
                                            "Failed to remove old log file {:?}: {}",
                                            path, e
                                        );
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
