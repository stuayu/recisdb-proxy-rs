//! Web server shared state.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use serde::Serialize;
use dns_lookup::lookup_addr;

use crate::server::listener::DatabaseHandle;
use crate::tuner::TunerPool;

/// Scan scheduler configuration (for Web API).
#[derive(Debug, Clone, Serialize)]
pub struct ScanSchedulerInfo {
    /// Interval between scheduler checks (seconds).
    pub check_interval_secs: u64,
    /// Maximum concurrent scans.
    pub max_concurrent_scans: usize,
    /// Scan timeout per BonDriver (seconds).
    pub scan_timeout_secs: u64,
}

/// Tuner optimization configuration (for Web API).
#[derive(Debug, Clone, Serialize)]
pub struct TunerConfigInfo {
    pub keep_alive_secs: u64,
    pub prewarm_enabled: bool,
    pub prewarm_timeout_secs: u64,
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

    /// Update session signal and packet stats.
    pub async fn update_stats(
        &self,
        id: u64,
        signal_level: f32,
        packets_sent: u64,
        packets_dropped: u64,
        packets_scrambled: u64,
        packets_error: u64,
        current_bitrate_mbps: f64,
    ) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.signal_level = signal_level;
            info.packets_sent = packets_sent;
            info.packets_dropped = packets_dropped;
            info.packets_scrambled = packets_scrambled;
            info.packets_error = packets_error;
            info.current_bitrate_mbps = current_bitrate_mbps;
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

/// Shared state for the web server.
pub struct WebState {
    /// Database handle.
    pub database: DatabaseHandle,
    /// Tuner pool reference.
    pub tuner_pool: Arc<TunerPool>,
    /// Session registry.
    pub session_registry: Arc<SessionRegistry>,
    /// Scan scheduler configuration.
    pub scan_config: RwLock<ScanSchedulerInfo>,
    /// Tuner optimization configuration.
    pub tuner_config: RwLock<TunerConfigInfo>,
}

impl WebState {
    /// Create a new web state.
    pub fn new(database: DatabaseHandle, tuner_pool: Arc<TunerPool>, session_registry: Arc<SessionRegistry>) -> Self {
        Self {
            database,
            tuner_pool,
            session_registry,
            scan_config: RwLock::new(ScanSchedulerInfo {
                check_interval_secs: 60,
                max_concurrent_scans: 1,
                scan_timeout_secs: 900,
            }),
            tuner_config: RwLock::new(TunerConfigInfo {
                keep_alive_secs: 60,
                prewarm_enabled: true,
                prewarm_timeout_secs: 30,
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
