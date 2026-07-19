//! Web server shared state.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, RwLock};
use serde::Serialize;
use dns_lookup::lookup_addr;

use recisdb_protocol::StreamClass;

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
    /// Fixed-duration prefill/jitter buffer settings (STREAMING_DESIGN.md
    /// §4/§9 P3), per stream class plus a shared safety margin.
    pub prefill_view_ms: u64,
    pub prefill_preview_ms: u64,
    pub prefill_record_ms: u64,
    pub jitter_safety_factor: f64,
}

/// Information about an active session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// Session ID.
    pub id: u64,
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
        }
    }

    /// Register a new session.
    pub async fn register(&self, id: u64, addr: SocketAddr) -> mpsc::Receiver<()> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let ip = addr.ip();
        let host = tokio::task::spawn_blocking(move || lookup_addr(&ip).ok())
            .await
            .ok()
            .flatten();
        let info = SessionInfo {
            id,
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
}

impl WebState {
    /// Create a new web state.
    pub fn new(
        database: DatabaseHandle,
        tuner_pool: Arc<TunerPool>,
        encoder_pool: Arc<EncoderPool>,
        session_registry: Arc<SessionRegistry>,
        auth: AuthConfig,
    ) -> Self {
        Self {
            database,
            tuner_pool,
            encoder_pool,
            session_registry,
            auth,
            proxy_listen_addr: None,
            update_check_cache: RwLock::new(None),
            update_status: Mutex::new(UpdateStatus::Idle),
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
        let _shutdown_rx = registry.register(1, addr).await;

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
