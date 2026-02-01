//! Web server shared state.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use serde::Serialize;

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

/// Information about an active session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// Session ID.
    pub id: u64,
    /// Client address.
    pub addr: String,
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
}

impl SessionRegistry {
    /// Create a new session registry.
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new session.
    pub async fn register(&self, id: u64, addr: SocketAddr) {
        let info = SessionInfo {
            id,
            addr: addr.to_string(),
            tuner_path: None,
            channel_info: None,
            channel_name: None,
            is_streaming: false,
            connected_at: Instant::now(),
            signal_level: 0.0,
            packets_sent: 0,
        };
        self.sessions.write().await.insert(id, info);
    }

    /// Unregister a session.
    pub async fn unregister(&self, id: u64) {
        self.sessions.write().await.remove(&id);
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
    pub async fn update_stats(&self, id: u64, signal_level: f32, packets_sent: u64) {
        if let Some(info) = self.sessions.write().await.get_mut(&id) {
            info.signal_level = signal_level;
            info.packets_sent = packets_sent;
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
        }
    }

    /// Update scan scheduler configuration.
    pub async fn update_scan_config(&self, config: ScanSchedulerInfo) {
        *self.scan_config.write().await = config;
    }

    /// Get scan scheduler configuration.
    pub async fn get_scan_config(&self) -> ScanSchedulerInfo {
        self.scan_config.read().await.clone()
    }
}
