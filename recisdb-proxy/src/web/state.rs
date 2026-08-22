//! Web server shared state.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use serde::Serialize;
use dns_lookup::lookup_addr;

use recisdb_protocol::StreamClass;

use crate::database::ProgramUpsert;
use crate::logging::LogBuffer;
use crate::server::listener::DatabaseHandle;
use crate::tuner::{EncoderPool, TunerPool};
use crate::web::auth::AuthConfig;

/// Scan scheduler configuration (for Web API).
#[derive(Debug, Clone, Serialize)]
pub struct ScanSchedulerInfo {
    /// Interval between scheduler checks (seconds).
    pub check_interval_secs: u64,
    /// Maximum concurrent scans.
    pub max_concurrent_scans: usize,
    /// Scan timeout per BonDriver (seconds).
    pub scan_timeout_secs: u64,
    /// Wait time after SetChannel before checking signal/read (milliseconds).
    pub signal_lock_wait_ms: u64,
    /// Max time to read/analyze TS for one channel (milliseconds).
    pub ts_read_timeout_ms: u64,
}

/// Tuner optimization configuration (for Web API).
#[derive(Debug, Clone, Serialize)]
pub struct TunerConfigInfo {
    pub keep_alive_secs: u64,
    pub prewarm_enabled: bool,
    pub prewarm_timeout_secs: u64,
    pub set_channel_retry_interval_ms: u64,
    pub set_channel_retry_timeout_ms: u64,
    pub signal_poll_interval_ms: u64,
    pub signal_wait_timeout_ms: u64,
    pub min_hold_secs: u64,
    pub reject_cooldown_ms: u64,
    pub no_data_timeout_secs: u64,
    /// Fixed-duration prefill/jitter buffer settings (STREAMING_DESIGN.md
    /// §4/§9 P3), per stream class plus a shared safety margin.
    pub prefill_view_ms: u64,
    pub prefill_preview_ms: u64,
    pub prefill_record_ms: u64,
    pub jitter_safety_factor: f64,
}

/// How a session reaches the proxy.
///
/// The dashboard's client list used to show BNDP sessions only, because that
/// was the only path that registered itself. HTTP viewers (the dashboard's
/// own preview, and everything going through the Mirakurun-compatible API —
/// EPGStation's live viewing and recording) occupied tuners while staying
/// invisible, which breaks the rule that a busy tuner must always show why
/// (CLAUDE.md, Web ダッシュボード). Both paths now register; this says which
/// one a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionProtocol {
    /// BonDriver proxy protocol over TCP (`server/listener.rs`) — TVTest/EDCB.
    Bndp,
    /// Dashboard HTTP stream (`GET /api/stream/service/...`).
    Http,
    /// Mirakurun-compatible HTTP API (`web/mirakurun.rs`) — EPGStation etc.
    Mirakurun,
}

impl SessionProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionProtocol::Bndp => "bndp",
            SessionProtocol::Http => "http",
            SessionProtocol::Mirakurun => "mirakurun",
        }
    }
}

/// Information about an active session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// Session ID.
    pub id: u64,
    /// Which transport this session arrived on.
    pub protocol: SessionProtocol,
    /// Client address.
    pub addr: String,
    /// Client hostname (reverse DNS).
    pub host: Option<String>,
    /// Current tuner path (if any).
    pub tuner_path: Option<String>,
    /// Current channel info (if any).
    pub channel_info: Option<String>,
    /// Channel name from database.
    pub channel_name: Option<String>,
    /// Current channel NID (for logo display).
    pub channel_nid: Option<u16>,
    /// Current channel SID (for logo display).
    pub channel_sid: Option<u16>,
    /// Whether the session is streaming.
    pub is_streaming: bool,
    /// Connection time (seconds since connection).
    #[serde(skip)]
    pub connected_at: Instant,
    /// Signal level (dB).
    pub signal_level: f32,
    /// Total TS packets sent to client.
    pub packets_sent: u64,
    /// Dropped TS packets.
    pub packets_dropped: u64,
    /// Scrambled TS packets.
    pub packets_scrambled: u64,
    /// Error TS packets.
    pub packets_error: u64,
    /// Current bitrate (Mbps).
    pub current_bitrate_mbps: f64,
    /// Client-specified priority (if provided).
    pub client_priority: Option<i32>,
    /// Client-specified exclusive lock request.
    pub client_exclusive: bool,
    /// Server override priority (if set).
    pub override_priority: Option<i32>,
    /// Server override exclusive lock (if set).
    pub override_exclusive: Option<bool>,
    /// Metrics history (last 60 seconds).
    pub metrics_history: SessionMetricsHistory,
    /// Chunks skipped due to broadcast::Receiver lag (unit: broadcast
    /// chunks, NOT TS packets — see STREAMING_DESIGN.md §3.1).
    pub loss_broadcast_lag_chunks: u64,
    /// Frames dropped because the per-session TS write mpsc buffer was full.
    pub loss_ts_queue_chunks: u64,
    /// tsreplace input-queue stall events (Full or Closed).
    pub loss_encoder_stall_events: u64,
    /// Top PIDs by continuity-counter error count, descending (max 10).
    pub top_loss_pids: Vec<(u16, u64)>,
    /// Stream reliability class (STREAMING_DESIGN.md §2): "view"/"record"/"preview".
    /// Set from `Hello.stream_class`, may be auto-promoted to "record" by the
    /// session based on effective priority.
    pub stream_class: String,
    /// Whether the session's prefill/jitter buffer (STREAMING_DESIGN.md §4
    /// P3) is currently filling (true) or has released and is passing TS
    /// straight through (false).
    pub prefilling: bool,
}

impl SessionInfo {
    /// Get connection duration in seconds.
    pub fn connected_seconds(&self) -> u64 {
        self.connected_at.elapsed().as_secs()
    }
}

/// Registry for tracking active sessions.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: RwLock<HashMap<u64, SessionInfo>>,
    shutdown_txs: RwLock<HashMap<u64, mpsc::Sender<()>>>,
    /// Source of session ids for **every** transport. BNDP used to count its
    /// own accepted connections; HTTP sessions share the same id space (they
    /// live in the same map and are addressed by the same
    /// `POST /api/clients/:id/disconnect`), so the counter has to be shared
    /// or the two would collide.
    next_id: std::sync::atomic::AtomicU64,
}

/// Session metrics history for sparklines.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionMetricsHistory {
    /// Bitrate history (timestamp_ms, mbps) - last 60 seconds.
    pub bitrate_history: VecDeque<(i64, f64)>,
    /// Packet loss rate history (timestamp_ms, rate) - last 60 seconds.
    pub packet_loss_history: VecDeque<(i64, f64)>,
    /// Signal level history (timestamp_ms, db) - last 60 seconds.
    pub signal_history: VecDeque<(i64, f32)>,
}

impl SessionMetricsHistory {
    /// Push a sample and trim to last 60 seconds.
    pub fn push_sample(&mut self, timestamp_ms: i64, bitrate_mbps: f64, packet_loss_rate: f64, signal_level: f32) {
        self.bitrate_history.push_back((timestamp_ms, bitrate_mbps));
        self.packet_loss_history.push_back((timestamp_ms, packet_loss_rate));
        self.signal_history.push_back((timestamp_ms, signal_level));

        let cutoff = timestamp_ms - 60_000;
        while self.bitrate_history.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            self.bitrate_history.pop_front();
        }
        while self.packet_loss_history.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            self.packet_loss_history.pop_front();
        }
        while self.signal_history.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            self.signal_history.pop_front();
        }
    }
}

impl SessionRegistry {
    /// Create a new session registry.
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            shutdown_txs: RwLock::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Allocate the next session id. Shared by every transport — see
    /// [`SessionRegistry::next_id`].
    pub fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Register a new session.
    pub async fn register(&self, id: u64, addr: SocketAddr, protocol: SessionProtocol) -> mpsc::Receiver<()> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let ip = addr.ip();
        let host = tokio::task::spawn_blocking(move || lookup_addr(&ip).ok())
            .await
            .ok()
            .flatten();
        let info = SessionInfo {
            id,
            protocol,
            addr: addr.to_string(),
            host,
            tuner_path: None,
            channel_info: None,
            channel_name: None,
            channel_nid: None,
            channel_sid: None,
            is_streaming: false,
            connected_at: Instant::now(),
            signal_level: 0.0,
            packets_sent: 0,
            packets_dropped: 0,
            packets_scrambled: 0,
            packets_error: 0,
            current_bitrate_mbps: 0.0,
            client_priority: None,
            client_exclusive: false,
            override_priority: None,
            override_exclusive: None,
            metrics_history: SessionMetricsHistory::default(),
            loss_broadcast_lag_chunks: 0,
            loss_ts_queue_chunks: 0,
            loss_encoder_stall_events: 0,
            top_loss_pids: Vec::new(),
            stream_class: StreamClass::View.as_str().to_string(),
            prefilling: false,
        };
        self.sessions.write().await.insert(id, info);
        self.shutdown_txs.write().await.insert(id, shutdown_tx);
        shutdown_rx
    }

    /// Unregister a session.
    pub async fn unregister(&self, id: u64) {
        self.sessions.write().await.remove(&id);
        self.shutdown_txs.write().await.remove(&id);
    }

    /// Update session tuner path.
    pub async fn update_tuner(&self, id: u64, tuner_path: Option<String>) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.tuner_path = tuner_path;
        }
    }

    /// Update session channel info.
    pub async fn update_channel(&self, id: u64, channel_info: Option<String>) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.channel_info = channel_info;
        }
    }

    /// Update session streaming status.
    pub async fn update_streaming(&self, id: u64, is_streaming: bool) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.is_streaming = is_streaming;
        }
    }

    /// Update session channel name.
    pub async fn update_channel_name(&self, id: u64, channel_name: Option<String>) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.channel_name = channel_name;
        }
    }

    /// Update session stream reliability class (STREAMING_DESIGN.md §2).
    pub async fn update_stream_class(&self, id: u64, stream_class: StreamClass) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.stream_class = stream_class.as_str().to_string();
        }
    }

    /// Update session prefill/jitter buffer status (STREAMING_DESIGN.md §4 P3).
    pub async fn update_prefilling(&self, id: u64, prefilling: bool) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.prefilling = prefilling;
        }
    }

    /// Update session channel NID/SID (for logo display on dashboard).
    pub async fn update_channel_ids(&self, id: u64, nid: Option<u16>, sid: Option<u16>) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.channel_nid = nid;
            info.channel_sid = sid;
        }
    }

    /// Update session signal and packet stats.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_stats(
        &self,
        id: u64,
        signal_level: f32,
        packets_sent: u64,
        packets_dropped: u64,
        packets_scrambled: u64,
        packets_error: u64,
        current_bitrate_mbps: f64,
        loss_broadcast_lag_chunks: u64,
        loss_ts_queue_chunks: u64,
        loss_encoder_stall_events: u64,
        top_loss_pids: Vec<(u16, u64)>,
    ) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.signal_level = signal_level;
            info.packets_sent = packets_sent;
            info.packets_dropped = packets_dropped;
            info.packets_scrambled = packets_scrambled;
            info.packets_error = packets_error;
            info.current_bitrate_mbps = current_bitrate_mbps;
            info.loss_broadcast_lag_chunks = loss_broadcast_lag_chunks;
            info.loss_ts_queue_chunks = loss_ts_queue_chunks;
            info.loss_encoder_stall_events = loss_encoder_stall_events;
            info.top_loss_pids = top_loss_pids;
        }
    }

    /// Update client-specified priority and exclusive lock request.
    pub async fn update_client_controls(
        &self,
        id: u64,
        priority: Option<i32>,
        exclusive: Option<bool>,
    ) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            if let Some(p) = priority {
                info.client_priority = Some(p);
            }
            if let Some(e) = exclusive {
                info.client_exclusive = e;
            }
        }
    }

    /// Update server override controls (use None to clear).
    pub async fn update_override_controls(
        &self,
        id: u64,
        override_priority: Option<Option<i32>>,
        override_exclusive: Option<Option<bool>>,
    ) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            if let Some(p) = override_priority {
                info.override_priority = p;
            }
            if let Some(e) = override_exclusive {
                info.override_exclusive = e;
            }
        }
    }

    /// Get effective controls (override if set, otherwise client values).
    pub async fn get_effective_controls(&self, id: u64) -> Option<(Option<i32>, bool)> {
        let info = self.sessions.read().await.get(&id)?.clone();
        let priority = info.override_priority.or(info.client_priority);
        let exclusive = info.override_exclusive.unwrap_or(info.client_exclusive);
        Some((priority, exclusive))
    }

    /// Push a metrics sample for session sparklines.
    pub async fn push_metrics_sample(
        &self,
        id: u64,
        timestamp_ms: i64,
        bitrate_mbps: f64,
        packet_loss_rate: f64,
        signal_level: f32,
    ) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.metrics_history
                .push_sample(timestamp_ms, bitrate_mbps, packet_loss_rate, signal_level);
        }
    }

    /// Request remote shutdown for a session.
    pub async fn request_shutdown(&self, id: u64) -> bool {
        if let Some(tx) = self.shutdown_txs.read().await.get(&id) {
            tx.send(()).await.is_ok()
        } else {
            false
        }
    }

    /// Get all active sessions.
    pub async fn get_all(&self) -> Vec<SessionInfo> {
        self.sessions.read().await.values().cloned().collect()
    }

    /// Get session count.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// Cached result of the last successful GitHub releases fetch
/// (`web/api/update.rs`, `GET /api/update/check`). Held in memory only —
/// never persisted to the database — and treated as stale after
/// `update::CACHE_TTL` (6 hours), at which point the next non-`force` check
/// triggers a re-fetch.
pub struct UpdateCheckCache {
    pub fetched_at: Instant,
    pub releases: Vec<crate::web::api::GithubRelease>,
}

/// Progress of the most recent (or in-flight) self-update
/// (`web/api/update.rs`, `POST /api/update/apply`). Starts at `Idle`.
/// `apply_update` only accepts a new run while this is `Idle` or `Error`;
/// any of the other variants means one is already running and a concurrent
/// request gets `409 Conflict`.
#[derive(Debug, Clone)]
pub enum UpdateStatus {
    Idle,
    Downloading,
    Extracting,
    Replacing,
    Restarting,
    /// Carries a human-readable failure reason. Reachable from any step —
    /// see `run_self_update_inner` in `web/api/update.rs`.
    Error(String),
}

/// Shared state for the web server.
pub struct WebState {
    /// Monotonic process start time used by the dashboard uptime statistic.
    pub started_at: Instant,
    /// Database handle.
    pub database: DatabaseHandle,
    /// Tuner pool reference.
    pub tuner_pool: Arc<TunerPool>,
    /// Shared tsreplace encoder pool (STREAMING_DESIGN.md §5/§6 P4/P5): the
    /// same pool `server::listener::Server` hands to every BNDP session, so
    /// an HTTP `?profile=preview` request and a TVTest session watching the
    /// same channel join the same running encoder chain.
    pub encoder_pool: Arc<EncoderPool>,
    /// Session registry.
    pub session_registry: Arc<SessionRegistry>,
    /// Scan scheduler configuration.
    pub scan_config: RwLock<ScanSchedulerInfo>,
    /// Tuner optimization configuration.
    pub tuner_config: RwLock<TunerConfigInfo>,
    /// Web API authentication (REVIEW_2026-07.md S2).
    pub auth: AuthConfig,
    /// The BNDP (BonDriver protocol) listen address, shown by the
    /// dashboard's client-setup guide so users can copy a ready-made
    /// BonDriver_NetworkProxy.ini. `None` when unknown (e.g. tests).
    pub proxy_listen_addr: Option<SocketAddr>,
    /// 6h in-memory cache of the last GitHub releases fetch
    /// (`web/api/update.rs`). `None` until the first check.
    pub update_check_cache: RwLock<Option<UpdateCheckCache>>,
    /// Progress of the most recent/in-flight self-update.
    pub update_status: Mutex<UpdateStatus>,
    /// Shared in-memory ring buffer of recent log lines (`logging.rs`),
    /// backing the dashboard's "ログ" tab (`web/api/logs.rs`).
    pub log_buffer: Arc<LogBuffer>,
    /// Directory holding the daily-rotated log files (`recisdb-proxy.log.*`)
    /// — `--log-dir` (`app_config::resolve_log_dir`). Used by
    /// `GET /api/logs/files` and `GET /api/logs/files/:name`.
    pub log_dir: PathBuf,
    /// Runtime log-level control (`logging::LogLevelHandle`), backing
    /// `GET`/`POST /api/log-config` (`web/api/logs.rs`). The level itself is
    /// stored in the database (`log_config` table); this handle is what
    /// actually reloads the `tracing_subscriber::EnvFilter` without a
    /// restart.
    pub log_level: Arc<crate::logging::LogLevelHandle>,
    /// 起動時に読み込んだ設定ファイルのパス。プレビュー自動セットアップが
    /// `[preview]` の実行ファイルパスを書き戻す先。`None` のときは書き戻せない
    /// (= 次回起動で設定が巻き戻る) ので、その旨を警告として返す。
    pub config_path: Option<PathBuf>,
    /// Region IDs treated as *local* by the Mirakurun-compatible API: their
    /// terrestrial stations are reported as channel type `GR`, everything
    /// else terrestrial as `NW1`..`NW40`
    /// (`web/mirakurun.rs::terrestrial_type_map`). Empty (the default) means
    /// every terrestrial station is `GR`. Resolved from `[mirakurun]
    /// home_region` in `app_config.rs`.
    pub mirakurun_home_regions: Vec<u8>,
    /// Fan-out for `GET /mirakurun/api/events/stream`
    /// (`web/mirakurun_events.rs`, `docs/EPGSTATION_COMPAT.md` §3/§6): every
    /// handler call does `.subscribe()` on this to get its own receiver.
    /// This is the *same* `broadcast::Sender` handle `main.rs` also gives to
    /// `crate::epg_writer::EpgWriter`, which is the only thing that ever
    /// sends on it (after each successful `programs` UPSERT) — created once
    /// in `main.rs`, not here, so both sides share one channel instead of
    /// each independently creating a dead one. See `main.rs` for the
    /// capacity-1024 rationale.
    pub epg_events_tx: broadcast::Sender<ProgramUpsert>,
    /// Live state of the dedicated node-to-node transport listener
    /// (`node::transport`), when the distributed fabric is enabled. The
    /// dashboard needs it to trust a newly paired peer immediately, without
    /// waiting for a restart. `None` when `[node] enabled = false`.
    pub node_transport: Option<Arc<crate::node::NodeTransportState>>,
    /// `[node] listen` as configured, shown next to a freshly issued pairing
    /// code so the operator knows which URL to type on the other node.
    pub node_listen_addr: Option<String>,
}

impl WebState {
    /// Create a new web state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: DatabaseHandle,
        tuner_pool: Arc<TunerPool>,
        encoder_pool: Arc<EncoderPool>,
        session_registry: Arc<SessionRegistry>,
        auth: AuthConfig,
        log_buffer: Arc<LogBuffer>,
        log_dir: PathBuf,
        log_level: Arc<crate::logging::LogLevelHandle>,
        epg_events_tx: broadcast::Sender<ProgramUpsert>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            database,
            tuner_pool,
            encoder_pool,
            session_registry,
            auth,
            proxy_listen_addr: None,
            config_path: None,
            mirakurun_home_regions: Vec::new(),
            update_check_cache: RwLock::new(None),
            update_status: Mutex::new(UpdateStatus::Idle),
            log_buffer,
            log_dir,
            log_level,
            epg_events_tx,
            node_transport: None,
            node_listen_addr: None,
            scan_config: RwLock::new(ScanSchedulerInfo {
                check_interval_secs: 60,
                max_concurrent_scans: 1,
                scan_timeout_secs: 900,
                signal_lock_wait_ms: 500,
                ts_read_timeout_ms: 300000,
            }),
            tuner_config: RwLock::new(TunerConfigInfo {
                keep_alive_secs: 60,
                prewarm_enabled: true,
                prewarm_timeout_secs: 30,
                set_channel_retry_interval_ms: 500,
                set_channel_retry_timeout_ms: 10_000,
                signal_poll_interval_ms: 500,
                signal_wait_timeout_ms: 10_000,
                min_hold_secs: 10,
                reject_cooldown_ms: 2_000,
                no_data_timeout_secs: 30,
                prefill_view_ms: 1000,
                prefill_preview_ms: 2000,
                prefill_record_ms: 6000,
                jitter_safety_factor: 1.5,
            }),
        }
    }

    /// Update scan scheduler configuration.
    pub async fn update_scan_config(&self, config: ScanSchedulerInfo) {
        *self.scan_config.write().await = config;
    }

    /// Update tuner optimization configuration.
    pub async fn update_tuner_config(&self, config: TunerConfigInfo) {
        *self.tuner_config.write().await = config;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn update_stats_propagates_loss_breakdown() {
        let registry = SessionRegistry::new();
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let _shutdown_rx = registry.register(1, addr, SessionProtocol::Bndp).await;

        registry
            .update_stats(
                1,
                -5.0,
                1000,
                10,
                2,
                1,
                18.5,
                7,   // loss_broadcast_lag_chunks
                3,   // loss_ts_queue_chunks
                2,   // loss_encoder_stall_events
                vec![(0x0100, 5), (0x0200, 2)],
            )
            .await;

        let sessions = registry.get_all().await;
        let info = sessions.into_iter().find(|s| s.id == 1).expect("session present");
        assert_eq!(info.loss_broadcast_lag_chunks, 7);
        assert_eq!(info.loss_ts_queue_chunks, 3);
        assert_eq!(info.loss_encoder_stall_events, 2);
        assert_eq!(info.top_loss_pids, vec![(0x0100, 5), (0x0200, 2)]);
    }

    #[test]
    fn session_info_loss_fields_serialize_to_json() {
        let info = SessionInfo {
            id: 42,
            protocol: SessionProtocol::Bndp,
            addr: "127.0.0.1:1".to_string(),
            host: None,
            tuner_path: None,
            channel_info: None,
            channel_name: None,
            channel_nid: None,
            channel_sid: None,
            is_streaming: true,
            connected_at: Instant::now(),
            signal_level: 10.0,
            packets_sent: 100,
            packets_dropped: 5,
            packets_scrambled: 0,
            packets_error: 0,
            current_bitrate_mbps: 18.0,
            client_priority: None,
            client_exclusive: false,
            override_priority: None,
            override_exclusive: None,
            metrics_history: SessionMetricsHistory::default(),
            loss_broadcast_lag_chunks: 4,
            loss_ts_queue_chunks: 1,
            loss_encoder_stall_events: 9,
            top_loss_pids: vec![(0x0100, 3), (0x0200, 1)],
            stream_class: "view".to_string(),
            prefilling: false,
        };

        let value = serde_json::to_value(&info).expect("serialize SessionInfo");
        assert_eq!(value["loss_broadcast_lag_chunks"], 4);
        assert_eq!(value["loss_ts_queue_chunks"], 1);
        assert_eq!(value["loss_encoder_stall_events"], 9);
        assert_eq!(
            value["top_loss_pids"],
            serde_json::json!([[0x0100, 3], [0x0200, 1]])
        );
    }
}
