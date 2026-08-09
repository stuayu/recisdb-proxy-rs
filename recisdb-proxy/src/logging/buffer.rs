//! In-memory ring buffer of recent log lines, exposed to the Web dashboard
//! (`web/api/logs.rs`, `GET /api/logs`).
//!
//! Implemented as a `tracing_subscriber::Layer` stacked onto the same
//! `registry()` as the stdout/file `fmt::layer()`s in `logging.rs`. Neither
//! of those layers overrides `Layer::enabled`, so the `EnvFilter` already in
//! the stack (`registry().with(env_filter).with(...))`) is the only thing
//! that decides whether an event fires at all: `tracing_subscriber::Layered`
//! ANDs every layer's `enabled()` result, so a plain `fmt::layer()` (or this
//! one) is unconditionally "enabled" and simply rides on whatever the
//! `EnvFilter` already let through. That means this layer needs no filter of
//! its own — it only ever sees events the `EnvFilter` already allowed, which
//! is exactly the "don't disturb the existing global-filter setup" behavior
//! CLAUDE.md-adjacent design notes asked for.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use chrono::Local;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// Maximum number of log lines retained in memory. Oldest entries are
/// evicted first once this is exceeded.
pub const LOG_BUFFER_CAPACITY: usize = 5000;

/// `tracing` target used exclusively by the HTTP access-log line
/// (`web/mod.rs`'s `access_log` middleware). Kept as a single constant
/// (rather than a string literal repeated at each call site) because both
/// the emitter (`web/mod.rs`) and the consumers of `LogQuery::category`
/// (this module, `web/api/logs.rs`) need to agree on the exact string.
///
/// This is deliberately *not* the same target as the rest of `web/mod.rs`
/// (`recisdb_proxy::web`, the module path default): that target is shared
/// with other log lines from the same module (e.g. startup logging), so it
/// cannot be used on its own to separate "access log" from "everything
/// else". Splitting it into its own target is what makes the dashboard's
/// server/access category filter (`LogQuery::category`) possible.
pub const ACCESS_LOG_TARGET: &str = "recisdb_proxy::access";

/// One buffered log line as shown by the dashboard's "ログ" tab.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Monotonically increasing sequence number (1-based). Used by the API
    /// for incremental polling (`after_seq`) and to detect eviction gaps.
    pub seq: u64,
    /// RFC3339 local timestamp.
    pub timestamp: String,
    /// `"ERROR"` / `"WARN"` / `"INFO"` / `"DEBUG"` / `"TRACE"`.
    pub level: String,
    /// `tracing` target (usually the module path).
    pub target: String,
    /// The event's `message` field, with any other fields appended as
    /// `key=value` pairs separated by spaces.
    pub message: String,
}

/// Rank used for "this level or more severe" filtering: higher = more
/// severe. Unknown strings are treated as INFO so a malformed level never
/// silently filters out or floods.
fn level_rank(level: &str) -> u8 {
    match level {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        "TRACE" => 0,
        _ => 2,
    }
}

/// A query against the [`LogBuffer`].
#[derive(Debug, Clone, Default)]
pub struct LogQuery<'a> {
    /// Minimum level (inclusive): `Some("warn")` returns WARN and ERROR.
    /// Case-insensitive; unrecognized values are treated as "no filter".
    pub level: Option<&'a str>,
    /// Case-sensitive substring match against `target`.
    pub target: Option<&'a str>,
    /// Case-insensitive substring match against `message`.
    pub q: Option<&'a str>,
    /// HTTP access log vs. everything else. Combines with `target` as AND
    /// (both must pass), so e.g. `category=server` + `target=tuner` narrows
    /// to non-access-log entries whose target contains "tuner".
    pub category: LogCategory,
    /// Only entries with `seq > after_seq` are returned. `0` means "from
    /// the start of whatever is still buffered".
    pub after_seq: u64,
    /// Maximum entries to return. The most recent matches are kept when
    /// more than `limit` entries pass the filters.
    pub limit: usize,
}

/// `category` filter for [`LogQuery`]: separates the HTTP access log
/// (`ACCESS_LOG_TARGET`) from server-side processing logs (scan, tuner, EPG,
/// ...), since the two are emitted at very different rates and otherwise
/// drown each other out in the dashboard's "ログ" tab.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogCategory {
    /// No category filter — everything passes.
    #[default]
    All,
    /// Everything except the access log.
    Server,
    /// Only the access log.
    Access,
}

impl LogCategory {
    /// Parses the `category` query parameter. Unknown/absent values fall
    /// back to `All` rather than erroring, matching the rest of this API's
    /// "malformed filter never floods or 400s" convention (see
    /// `level_rank`'s doc comment).
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("server") => LogCategory::Server,
            Some("access") => LogCategory::Access,
            _ => LogCategory::All,
        }
    }

    fn matches(self, target: &str) -> bool {
        match self {
            LogCategory::All => true,
            LogCategory::Server => target != ACCESS_LOG_TARGET,
            LogCategory::Access => target == ACCESS_LOG_TARGET,
        }
    }
}

/// Result of a [`LogBuffer::query`] call.
#[derive(Debug, Clone, Serialize)]
pub struct LogQueryResult {
    pub entries: Vec<LogEntry>,
    /// Highest seq currently held in the buffer (0 if empty), regardless of
    /// filtering — callers poll again with `after_seq = last_seq`.
    pub last_seq: u64,
    /// `true` when `after_seq` pointed at (or before) an entry that has
    /// already been evicted from the ring buffer, meaning the caller may
    /// have missed lines between its last fetch and this one. The caller
    /// should treat this as "refetch from scratch" (drop `after_seq`).
    pub dropped: bool,
}

/// Thread-safe fixed-capacity ring buffer of recent [`LogEntry`] values.
pub struct LogBuffer {
    entries: RwLock<VecDeque<LogEntry>>,
    next_seq: AtomicU64,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(VecDeque::with_capacity(capacity)),
            next_seq: AtomicU64::new(1),
            capacity,
        })
    }

    fn push(&self, level: Level, target: &str, message: String) {
        // A poisoned lock (a panic while holding the write guard elsewhere)
        // must not take logging down with it — recover the guard and carry
        // on, same spirit as the rest of the codebase avoiding unwrap() on
        // anything that can be reached from a background/async task.
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        // Seq must be assigned while holding the write lock: the deque must
        // stay sorted by seq (query() relies on front()/back() and
        // `seq > after_seq` assuming monotonic order), so number-then-lock
        // would let two threads insert out of order.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let entry = LogEntry {
            seq,
            timestamp: Local::now().to_rfc3339(),
            level: level.to_string(),
            target: target.to_string(),
            message,
        };
        if guard.len() >= self.capacity {
            guard.pop_front();
        }
        guard.push_back(entry);
    }

    /// Run a query against the currently buffered entries.
    pub fn query(&self, query: LogQuery<'_>) -> LogQueryResult {
        let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());

        let last_seq = guard.back().map(|e| e.seq).unwrap_or(0);
        let oldest_seq = guard.front().map(|e| e.seq).unwrap_or(0);
        // `after_seq` pointed at something older than the oldest entry we
        // still hold: at least one entry in between was evicted.
        let dropped = query.after_seq > 0 && oldest_seq > query.after_seq + 1;

        let min_rank = query.level.and_then(|l| match l.to_ascii_uppercase().as_str() {
            "ERROR" => Some(4),
            "WARN" | "WARNING" => Some(3),
            "INFO" => Some(2),
            "DEBUG" => Some(1),
            "TRACE" => Some(0),
            _ => None,
        });
        let target_needle = query.target.filter(|t| !t.is_empty());
        let q_needle = query.q.filter(|q| !q.is_empty()).map(|q| q.to_lowercase());

        let mut entries: Vec<LogEntry> = guard
            .iter()
            .filter(|e| e.seq > query.after_seq)
            .filter(|e| min_rank.is_none_or(|m| level_rank(&e.level) >= m))
            .filter(|e| target_needle.is_none_or(|t| e.target.contains(t)))
            .filter(|e| query.category.matches(&e.target))
            .filter(|e| q_needle.as_deref().is_none_or(|kw| e.message.to_lowercase().contains(kw)))
            .cloned()
            .collect();

        let limit = query.limit.max(1);
        if entries.len() > limit {
            let drop_from_front = entries.len() - limit;
            entries.drain(..drop_from_front);
        }

        LogQueryResult { entries, last_seq, dropped }
    }
}

/// Extracts `message` (and any other fields, appended as `key=value`) from a
/// tracing event into a single display string.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    extra: Vec<(String, String)>,
}

/// Fields `tracing-log` synthesizes when bridging a `log` record into a
/// `tracing` event: `log.target`, `log.module_path`, `log.file`, `log.line`.
/// They are bookkeeping, not part of what the line says — and the first two
/// duplicate [`LogEntry::target`] once it's normalized — so they're dropped
/// rather than appended to the displayed message.
fn is_log_bridge_field(name: &str) -> bool {
    name.starts_with("log.")
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if is_log_bridge_field(field.name()) {
            return;
        }
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.extra.push((field.name().to_string(), text));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if is_log_bridge_field(field.name()) {
            return;
        }
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.extra.push((field.name().to_string(), value.to_string()));
        }
    }
}

impl MessageVisitor {
    fn into_message(self) -> String {
        let mut message = self.message.unwrap_or_default();
        for (key, value) in self.extra {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(&key);
            message.push('=');
            message.push_str(&value);
        }
        message
    }
}

/// `tracing_subscriber::Layer` that mirrors every event it sees into a
/// shared [`LogBuffer`]. See the module doc comment for why it needs no
/// filter of its own.
pub struct LogBufferLayer {
    buffer: Arc<LogBuffer>,
}

impl LogBufferLayer {
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        Self { buffer }
    }
}

impl<S: Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.into_message();

        // Almost everything in this crate logs through the `log` macros, and
        // `tracing-log` cannot give those events a real `target` in their
        // metadata: a `tracing` callsite needs a `'static` target, so the
        // bridge uses the fixed target `"log"` for all of them and stashes
        // the true one in a `log.target` field. Reading
        // `event.metadata().target()` directly therefore records `"log"` for
        // every `log::info!` in the process, which silently breaks both the
        // dashboard's target filter and its access/server split.
        // `normalized_metadata()` reconstructs the original target (and
        // level); it returns `None` for events that came from `tracing`
        // natively, which already carry correct metadata.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        self.buffer.push(*metadata.level(), metadata.target(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, level: &str) -> LogEntry {
        LogEntry {
            seq,
            timestamp: "2026-07-19T00:00:00+09:00".to_string(),
            level: level.to_string(),
            target: "recisdb_proxy::tuner".to_string(),
            message: format!("line {seq}"),
        }
    }

    fn buffer_with(entries: Vec<LogEntry>) -> Arc<LogBuffer> {
        let buf = LogBuffer::new(LOG_BUFFER_CAPACITY);
        {
            let mut guard = buf.entries.write().unwrap();
            for e in entries {
                guard.push_back(e);
            }
        }
        buf
    }

    #[test]
    fn push_evicts_oldest_beyond_capacity() {
        let buf = LogBuffer::new(3);
        buf.push(Level::INFO, "t", "a".into());
        buf.push(Level::INFO, "t", "b".into());
        buf.push(Level::INFO, "t", "c".into());
        buf.push(Level::INFO, "t", "d".into());

        let result = buf.query(LogQuery { limit: 100, ..Default::default() });
        let messages: Vec<&str> = result.entries.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, vec!["b", "c", "d"]);
        assert_eq!(result.last_seq, 4);
    }

    #[test]
    fn level_filter_is_at_least_severity() {
        let buf = buffer_with(vec![entry(1, "TRACE"), entry(2, "INFO"), entry(3, "WARN"), entry(4, "ERROR")]);
        let result = buf.query(LogQuery { level: Some("warn"), limit: 100, ..Default::default() });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 4]);
    }

    #[test]
    fn target_filter_is_substring_case_sensitive() {
        let mut e = entry(1, "INFO");
        e.target = "recisdb_proxy::tuner::pool".to_string();
        let buf = buffer_with(vec![e, entry(2, "INFO")]);
        let result = buf.query(LogQuery { target: Some("tuner::pool"), limit: 100, ..Default::default() });
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].seq, 1);
    }

    #[test]
    fn message_query_is_case_insensitive() {
        let mut e = entry(1, "INFO");
        e.message = "Signal LOCKED".to_string();
        let buf = buffer_with(vec![e]);
        let result = buf.query(LogQuery { q: Some("locked"), limit: 100, ..Default::default() });
        assert_eq!(result.entries.len(), 1);
    }

    #[test]
    fn after_seq_returns_only_newer_entries() {
        let buf = buffer_with(vec![entry(1, "INFO"), entry(2, "INFO"), entry(3, "INFO")]);
        let result = buf.query(LogQuery { after_seq: 1, limit: 100, ..Default::default() });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert!(!result.dropped);
    }

    #[test]
    fn after_seq_older_than_buffer_reports_dropped() {
        // Buffer only holds seq 5..=7 (2..=4 were evicted); client last saw seq 2.
        let buf = buffer_with(vec![entry(5, "INFO"), entry(6, "INFO"), entry(7, "INFO")]);
        let result = buf.query(LogQuery { after_seq: 2, limit: 100, ..Default::default() });
        assert!(result.dropped);
        // Entries are still returned (best-effort) even though dropped=true.
        assert_eq!(result.entries.len(), 3);
    }

    #[test]
    fn after_seq_zero_is_never_dropped() {
        let buf = buffer_with(vec![entry(50, "INFO"), entry(51, "INFO")]);
        let result = buf.query(LogQuery { after_seq: 0, limit: 100, ..Default::default() });
        assert!(!result.dropped);
    }

    #[test]
    fn after_seq_adjacent_to_oldest_is_not_dropped() {
        // Client's last_seq (4) is exactly one before the oldest surviving
        // entry (5): no gap, nothing was missed.
        let buf = buffer_with(vec![entry(5, "INFO"), entry(6, "INFO")]);
        let result = buf.query(LogQuery { after_seq: 4, limit: 100, ..Default::default() });
        assert!(!result.dropped);
    }

    #[test]
    fn limit_keeps_the_most_recent_matches() {
        let buf = buffer_with((1..=10).map(|i| entry(i, "INFO")).collect());
        let result = buf.query(LogQuery { limit: 3, ..Default::default() });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![8, 9, 10]);
    }

    fn access_entry(seq: u64) -> LogEntry {
        let mut e = entry(seq, "INFO");
        e.target = ACCESS_LOG_TARGET.to_string();
        e.message = format!("access {seq}");
        e
    }

    #[test]
    fn category_server_excludes_access_log() {
        let buf = buffer_with(vec![access_entry(1), entry(2, "INFO"), access_entry(3)]);
        let result = buf.query(LogQuery { category: LogCategory::Server, limit: 100, ..Default::default() });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2]);
    }

    #[test]
    fn category_access_returns_only_access_log() {
        let buf = buffer_with(vec![access_entry(1), entry(2, "INFO"), access_entry(3)]);
        let result = buf.query(LogQuery { category: LogCategory::Access, limit: 100, ..Default::default() });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 3]);
    }

    #[test]
    fn category_all_by_default_returns_both() {
        let buf = buffer_with(vec![access_entry(1), entry(2, "INFO")]);
        let result = buf.query(LogQuery { limit: 100, ..Default::default() });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn category_combines_with_target_filter_as_and() {
        let mut tuner_entry = entry(1, "INFO");
        tuner_entry.target = "recisdb_proxy::tuner".to_string();
        let mut web_entry = entry(2, "INFO");
        web_entry.target = "recisdb_proxy::web".to_string();
        let buf = buffer_with(vec![tuner_entry, web_entry, access_entry(3)]);

        // category=server AND target="tuner": only the non-access entry
        // whose target also contains "tuner" survives.
        let result = buf.query(LogQuery {
            category: LogCategory::Server,
            target: Some("tuner"),
            limit: 100,
            ..Default::default()
        });
        let seqs: Vec<u64> = result.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1]);
    }

    #[test]
    fn log_category_parse_treats_unknown_as_all() {
        assert_eq!(LogCategory::parse(Some("server")), LogCategory::Server);
        assert_eq!(LogCategory::parse(Some("access")), LogCategory::Access);
        assert_eq!(LogCategory::parse(Some("all")), LogCategory::All);
        assert_eq!(LogCategory::parse(Some("bogus")), LogCategory::All);
        assert_eq!(LogCategory::parse(None), LogCategory::All);
    }

    // -- LogBufferLayer over `log`-bridged events ----------------------------
    //
    // Everything in this crate logs through the `log` macros, so these two
    // exercise the path that actually matters in production.
    // `tracing_log::format_trace` builds exactly the event `LogTracer` would
    // dispatch for a `log::Record`, without having to install a global
    // logger.

    fn capture_log_record(target: &str, message: &str) -> LogEntry {
        use tracing_subscriber::layer::SubscriberExt;

        let buffer = LogBuffer::new(LOG_BUFFER_CAPACITY);
        let subscriber = tracing_subscriber::registry().with(LogBufferLayer::new(buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            // `Record::builder()` borrows the `Arguments`, so the format
            // args have to outlive the record — hence the separate binding.
            let args = format_args!("{message}");
            let record = log::Record::builder()
                .args(args)
                .level(log::Level::Info)
                .target(target)
                .module_path(Some("recisdb_proxy::web"))
                .file(Some("recisdb-proxy/src/web/mod.rs"))
                .line(Some(190))
                .build();
            tracing_log::format_trace(&record).unwrap();
        });

        let result = buffer.query(LogQuery { limit: 10, ..Default::default() });
        assert_eq!(result.entries.len(), 1, "expected exactly one buffered entry");
        result.entries.into_iter().next().unwrap()
    }

    #[test]
    fn log_bridged_events_keep_their_real_target() {
        // Regression: the bridge gives every `log` event the fixed tracing
        // target "log", so reading `event.metadata().target()` recorded
        // "log" for all of them — the target filter matched nothing and the
        // access/server split put the access log on the server side.
        let entry = capture_log_record(ACCESS_LOG_TARGET, "1.2.3.4 \"GET /api/stats\" 200 0ms");
        assert_eq!(entry.target, ACCESS_LOG_TARGET);
        assert_eq!(entry.level, "INFO");
        assert!(LogCategory::Access.matches(&entry.target));
        assert!(!LogCategory::Server.matches(&entry.target));
    }

    #[test]
    fn log_bridge_bookkeeping_fields_stay_out_of_the_message() {
        // log.target / log.module_path / log.file / log.line would otherwise
        // be appended as `key=value` noise on every single line.
        let entry = capture_log_record("recisdb_proxy::scheduler", "Scan completed");
        assert_eq!(entry.message, "Scan completed");
    }
}
