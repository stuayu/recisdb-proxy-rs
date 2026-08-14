//! Registration of HTTP stream viewers in the dashboard's session registry.
//!
//! Everything that occupies a tuner has to be visible on the dashboard — a
//! tuner that is busy must always show *why* (CLAUDE.md, Web ダッシュボード).
//! BNDP sessions do that from `server/listener.rs`; this module is the
//! equivalent for the HTTP paths (`web/stream.rs`, `web/mirakurun.rs`,
//! `web/mirakurun_program_stream.rs`), including everything EPGStation does
//! through the Mirakurun-compatible API.
//!
//! # Why a guard type
//!
//! An HTTP stream has no "session loop" to hang cleanup off: its lifetime is
//! the lifetime of the response body, which axum/hyper drops on client
//! disconnect. [`HttpStreamSession`] is therefore an RAII guard held by
//! `stream::StreamCleanup`, exactly like the tuner subscription it sits
//! next to, and unregisters from a short detached task in `Drop` (the
//! registry's API is `async`, `Drop` is not).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use log::info;
use tokio::sync::mpsc;

use recisdb_protocol::StreamClass;

use crate::ts_analyzer::TS_PACKET_SIZE;
use crate::web::state::{SessionProtocol, SessionRegistry};

/// What a newly started HTTP stream knows about itself at registration time.
pub struct HttpStreamSessionInfo {
    pub protocol: SessionProtocol,
    /// Peer address, when axum could provide one (`ConnectInfo`). Unknown
    /// peers register as `0.0.0.0:0` rather than failing the request.
    pub addr: Option<SocketAddr>,
    /// BonDriver serving this stream, for the dashboard's tuner column.
    pub tuner_path: Option<String>,
    /// Human-readable channel label (`channels.channel_name`).
    pub channel_name: Option<String>,
    /// Physical/logical channel label shown next to the name.
    pub channel_info: Option<String>,
    pub nid: Option<u16>,
    pub sid: Option<u16>,
    /// Reliability class of this stream (STREAMING_DESIGN.md §2). Recording
    /// clients (`GET /programs/{id}/stream`) are `Record`; live viewing is
    /// `View`.
    pub stream_class: StreamClass,
}

/// Byte/packet counters for one HTTP stream.
///
/// The BNDP path gets these from `server/session.rs`, which owns an explicit
/// write loop. HTTP bodies are a `Stream` of chunks, so the counting happens
/// where the chunks are yielded (`web/stream.rs`) and is pushed into the
/// registry at most once a second — the dashboard polls far slower than the
/// chunk rate, and taking the registry's write lock per chunk would be
/// pointless contention.
pub struct HttpStreamStats {
    bytes_total: AtomicU64,
    /// `bytes_total` as of the last flush, to derive the interval bitrate.
    bytes_at_last_flush: AtomicU64,
    /// Millis since `started` at the last flush.
    last_flush_ms: AtomicU64,
    started: Instant,
}

impl HttpStreamStats {
    fn new() -> Self {
        Self {
            bytes_total: AtomicU64::new(0),
            bytes_at_last_flush: AtomicU64::new(0),
            last_flush_ms: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    /// Record a chunk handed to the client. Returns the flush payload
    /// (packets sent, Mbps over the interval) when at least a second has
    /// passed since the last one, otherwise `None`.
    fn record(&self, len: usize) -> Option<(u64, f64)> {
        let total = self.bytes_total.fetch_add(len as u64, Ordering::Relaxed) + len as u64;

        let now_ms = self.started.elapsed().as_millis() as u64;
        let last_ms = self.last_flush_ms.load(Ordering::Relaxed);
        let elapsed_ms = now_ms.saturating_sub(last_ms);
        if elapsed_ms < 1000 {
            return None;
        }

        // Only the task that wins this exchange flushes, so concurrent
        // chunks (there are none today — one stream, one poller — but the
        // counters are shared by `&self`) cannot double-report an interval.
        if self
            .last_flush_ms
            .compare_exchange(last_ms, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }

        let previous = self.bytes_at_last_flush.swap(total, Ordering::Relaxed);
        let delta_bytes = total.saturating_sub(previous);
        let mbps = (delta_bytes as f64 * 8.0) / (elapsed_ms as f64 * 1000.0);

        Some((total / TS_PACKET_SIZE as u64, mbps))
    }
}

/// RAII registration of one HTTP stream in the dashboard's session registry.
pub struct HttpStreamSession {
    registry: Arc<SessionRegistry>,
    id: u64,
    stats: Arc<HttpStreamStats>,
}

impl HttpStreamSession {
    /// Register the stream and return the guard plus the shutdown receiver
    /// that `POST /api/clients/{id}/disconnect` fires (the HTTP body stream
    /// ends when it receives).
    pub async fn register(
        registry: Arc<SessionRegistry>,
        info: HttpStreamSessionInfo,
    ) -> (Self, mpsc::Receiver<()>) {
        let id = registry.allocate_id();
        let addr = info.addr.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
        let shutdown_rx = registry.register(id, addr, info.protocol).await;

        registry.update_tuner(id, info.tuner_path).await;
        registry.update_channel(id, info.channel_info).await;
        registry.update_channel_name(id, info.channel_name).await;
        registry.update_channel_ids(id, info.nid, info.sid).await;
        registry.update_stream_class(id, info.stream_class).await;
        registry.update_streaming(id, true).await;

        info!(
            "[Session {}] {} stream started from {}",
            id,
            info.protocol.as_str(),
            addr
        );

        (Self { registry, id, stats: Arc::new(HttpStreamStats::new()) }, shutdown_rx)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Account for a chunk sent to the client, flushing to the registry at
    /// most once a second (see [`HttpStreamStats`]).
    pub fn record_sent(&self, len: usize) {
        let Some((packets_sent, mbps)) = self.stats.record(len) else {
            return;
        };

        let registry = Arc::clone(&self.registry);
        let id = self.id;
        tokio::spawn(async move {
            // Signal level and the loss counters stay at their defaults:
            // an HTTP stream reads an already-shared broadcast, so the
            // per-client drop accounting the BNDP session does (its own
            // bounded write queue) has no equivalent here.
            registry
                .update_stats(id, 0.0, packets_sent, 0, 0, 0, mbps, 0, 0, 0, Vec::new())
                .await;
            registry
                .push_metrics_sample(id, chrono::Utc::now().timestamp_millis(), mbps, 0.0, 0.0)
                .await;
        });
    }
}

impl Drop for HttpStreamSession {
    fn drop(&mut self) {
        let registry = Arc::clone(&self.registry);
        let id = self.id;
        tokio::spawn(async move {
            registry.unregister(id).await;
            info!("[Session {}] http stream ended", id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:12345".parse().unwrap()
    }

    fn session_info(protocol: SessionProtocol) -> HttpStreamSessionInfo {
        HttpStreamSessionInfo {
            protocol,
            addr: Some(addr()),
            tuner_path: Some("Test.dll".to_string()),
            channel_name: Some("ＴＯＫＹＯ　ＭＸ１".to_string()),
            channel_info: Some("GR 16".to_string()),
            nid: Some(32391),
            sid: Some(23608),
            stream_class: StreamClass::View,
        }
    }

    #[tokio::test]
    async fn registers_and_unregisters_with_channel_details() {
        let registry = Arc::new(SessionRegistry::new());

        let (session, _shutdown_rx) =
            HttpStreamSession::register(Arc::clone(&registry), session_info(SessionProtocol::Mirakurun)).await;
        let id = session.id();

        let sessions = registry.get_all().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].protocol, SessionProtocol::Mirakurun);
        assert_eq!(sessions[0].channel_name.as_deref(), Some("ＴＯＫＹＯ　ＭＸ１"));
        assert_eq!(sessions[0].channel_nid, Some(32391));
        assert_eq!(sessions[0].tuner_path.as_deref(), Some("Test.dll"));
        assert!(sessions[0].is_streaming);

        drop(session);
        // Drop unregisters from a detached task.
        for _ in 0..100 {
            if registry.count().await == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("session was not unregistered after the guard was dropped");
    }

    #[tokio::test]
    async fn ids_do_not_collide_between_transports() {
        let registry = Arc::new(SessionRegistry::new());
        let bndp_id = registry.allocate_id();
        let _bndp = registry.register(bndp_id, addr(), SessionProtocol::Bndp).await;

        let (http, _rx) = HttpStreamSession::register(Arc::clone(&registry), session_info(SessionProtocol::Http)).await;
        assert_ne!(http.id(), bndp_id);
        assert_eq!(registry.count().await, 2);
    }

    #[test]
    fn stats_flush_at_most_once_per_second() {
        let stats = HttpStreamStats::new();
        // The first chunks land inside the initial second and report nothing.
        assert!(stats.record(TS_PACKET_SIZE).is_none());

        // Pretend a second has already elapsed since the last flush.
        stats.last_flush_ms.store(0, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let (packets, mbps) = stats.record(TS_PACKET_SIZE).expect("a flush is due");
        assert_eq!(packets, 2);
        assert!(mbps > 0.0);
    }
}
