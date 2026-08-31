//! Client session handling.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use log::{debug, error, info, trace, warn};
use tokio::io::AsyncReadExt;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::{broadcast, mpsc};

use recisdb_protocol::{
    broadcast_region::classify_nid, decode_client_message, decode_header, encode_server_message,
    BandType, ClientChannelInfo, ClientMessage, ErrorCode, ServerMessage, StreamClass, HEADER_SIZE,
    PROTOCOL_VERSION,
};

use crate::server::listener::DatabaseHandle;
use crate::server::prefill::{default_bitrate_bps, prefill_target_bytes, PrefillBuffer};
use crate::ts_analyzer::service_filter::TsServiceFilter;
use crate::tuner::channel_key::ChannelKeySpec;
use crate::tuner::encoder_pool::{
    self, EncodeKey, EncoderPool, EncoderPoolError, EncoderRuntimeConfig, SharedEncoder,
};
use crate::tuner::quality_scorer::QualityScorer;
use crate::tuner::shared::{ReaderState, StopReason};
use crate::tuner::{
    ts_analyzer::TsPacketAnalyzer, ChannelKey, SharedTuner, SlotPermit, TunerPool,
    TunerSubscription, WarmTunerHandle,
};
use crate::web::SessionRegistry;

/// Session state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    /// Initial state, waiting for hello.
    Initial,
    /// Handshake complete, ready to accept commands.
    Ready,
    /// Tuner is open.
    TunerOpen,
    /// Streaming TS data.
    Streaming,
    /// Session is closing.
    Closing,
}

use crate::server::client_view::ChannelEntry;
use crate::server::session_capacity::{cleanup_unused_tuner_after_switch, driver_max_instances};
use crate::server::session_channel_candidates::collect_group_channel_candidates;
use crate::server::session_runtime::{
    load_prefill_runtime_config, load_ts_queue_runtime_config, load_tsreplace_runtime_config,
    resolve_encode_sids, PrefillRuntimeConfig, TsreplaceRuntimeConfig,
};
use crate::server::session_space_cache::{
    clear_caches as clear_session_caches, current_or_default_tuner_path,
    ensure_channel_map_with_region as ensure_channel_map_with_region_cached,
    ensure_space_list as ensure_space_list_cached,
    get_space_list_with_names as get_space_list_with_names_cached,
    map_space_idx_to_actual_with_region as map_space_idx_to_actual_with_region_cached,
};
use crate::server::session_tuner_handoff::handoff_current_tuner;
use crate::tuner::acquire::{acquire, AcquireError, AcquireRequest};

/// Weight of a new sample in the measured-bitrate EWMA.
///
/// The rate is sampled once a second; 0.3 settles within a few seconds while
/// still ignoring one-off spikes. Used to size the prefill and TS write
/// buffers in *seconds* rather than in bytes guessed from the band.
const BITRATE_EWMA_ALPHA: f64 = 0.3;

/// Capacity of the per-session control message write buffer.
///
/// Control messages (SetChannelAck, HelloAck, etc.) are small and
/// infrequent. 64 slots is more than sufficient.
const CTRL_WRITE_BUFFER_CAPACITY: usize = 64;

/// How long a session may stay in `Streaming` with no TS source attached
/// before it is disconnected.
///
/// A failed channel switch can leave the session subscribed to nothing: the
/// pre-switch cleanup drops `ts_receiver`/`current_tuner`, `acquire` then
/// fails, and `try_restore_previous_channel` cannot bring the old reader
/// back (it was stopped or already reaped from the pool). Without this the
/// run loop simply parks on the 100 ms idle tick forever — a session that
/// reports "streaming" on the dashboard, holds a priority/exclusive claim in
/// the client view, and never delivers a single packet (observed in
/// production: 76 minutes, `packets_sent = 0`, `signal_level = 0`).
/// Disconnecting hands the decision back to the client, which reconnects and
/// re-selects instead of staring at a dead stream.
const NO_TS_SOURCE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// One idle tick of a `Streaming` session that has no TS source attached.
///
/// Starts the clock on the first such tick and reports whether the grace
/// period has run out. Split out of the run loop so the deadline itself is
/// testable without a live socket and tuner.
fn no_ts_source_deadline_passed(
    since: &mut Option<std::time::Instant>,
    now: std::time::Instant,
    grace: std::time::Duration,
) -> bool {
    let start = *since.get_or_insert(now);
    now.saturating_duration_since(start) >= grace
}

/// How long a session with no tuner open may sit idle before it is dropped.
///
/// The client DLL keeps its TCP connection up until `Release()` — `CloseTuner`
/// alone does not close it (`bondriver-proxy-client` `bondriver/exports.rs`) —
/// and it reconnects by itself on the next `OpenTuner`, so dropping an idle
/// connection costs the client nothing. Leaving them up costs a session row
/// each on the dashboard forever (observed in production: one connected for
/// 3.6 days, never having selected a channel). Sessions that *do* hold a tuner
/// are exempt: keeping a tuner open without streaming is legitimate.
const IDLE_NO_TUNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

use crate::server::session_backpressure::{
    send_ts_frame, should_auto_promote_to_record, TsFrameSendOutcome, RECORD_OVERFLOW_TIMEOUT,
    RECORD_PRIORITY_THRESHOLD,
};
use crate::server::ts_queue::{budget_bytes, TsWriteQueue};

/// A client session.
pub struct Session {
    /// Unique session ID.
    id: u64,
    /// Client address.
    #[allow(dead_code)]
    addr: SocketAddr,
    /// Read half of the TCP socket (write half is in the writer task).
    socket_reader: OwnedReadHalf,
    /// Sender for TS data frames (pre-encoded wire bytes) to the writer task.
    /// `try_send` is used to avoid blocking the select loop; when the buffer
    /// is full, oldest entries are drained to stay close to real-time.
    ts_write_queue: TsWriteQueue,
    /// Sender for control messages (pre-encoded wire bytes) to the writer task.
    /// Control messages have priority in the writer task.
    ctrl_write_tx: mpsc::Sender<Bytes>,
    /// Handle to the writer task for clean shutdown.
    writer_handle: Option<tokio::task::JoinHandle<()>>,
    /// Read buffer.
    read_buf: BytesMut,
    /// Current session state.
    state: SessionState,
    /// Reference to the tuner pool.
    tuner_pool: Arc<TunerPool>,
    /// Reference to the database.
    database: DatabaseHandle,
    /// Currently open tuner.
    current_tuner: Option<Arc<SharedTuner>>,
    /// Watches `current_tuner`'s reader lifecycle so the run loop learns of
    /// an eviction or driver failure the moment it happens (P4). Refreshed
    /// by `finalize_tuner_switch`.
    reader_state_rx: Option<tokio::sync::watch::Receiver<ReaderState>>,
    /// Warm tuner handle for pre-opened BonDriver.
    warm_tuner: Option<WarmTunerHandle>,
    /// Warm tuner path.
    warm_tuner_path: Option<String>,
    /// Current tuner path.
    current_tuner_path: Option<String>,
    tuner_claim_priority: i32,
    tuner_claim_exclusive: bool,
    /// Default tuner path.
    default_tuner: Option<String>,
    /// Current group name (if opened with group).
    current_group_name: Option<String>,
    /// Group drivers (paths for all drivers in the group).
    group_driver_paths: Vec<String>,
    /// TS data receiver (when streaming). RAII: dropping releases the
    /// tracked subscription automatically (see `TunerSubscription`).
    ts_receiver: Option<TunerSubscription>,
    /// Since when this session has been `Streaming` without anything to read
    /// from (no `ts_receiver`, no shared encoder). Normally `None`: every
    /// successful selection re-subscribes. It becomes `Some` only when a
    /// channel switch tore the old subscription down and could not put a new
    /// one in its place — see `NO_TS_SOURCE_GRACE`.
    no_ts_source_since: Option<std::time::Instant>,
    // Session struct に追加
    ts_bytes_sent: u64,
    ts_msgs_sent: u64,
    last_ts_log: std::time::Instant,
    /// region_key ("関東"/"BS"/...) -> クライアントに列挙するチャンネル一覧。
    /// EnumChannelName はチャンネル数ぶん呼ばれるので、リージョンごとに
    /// 1回だけDBスキャンする (clear_caches でクリア)。
    channel_map_cache: HashMap<String, Vec<ChannelEntry>>,
    // ★追加: 仮想space_idx(0..N-1) -> (actual_space, display_name, region_key) のマップをチューナごとにキャッシュ
    // 例: [(0, "地デジ", "宮城"), (0, "地デジ", "福島"), (1, "BS", "BS"), (2, "CS", "CS")]
    // region_key はチャンネルフィルタリング用、display_name は EnumTuningSpace 表示用
    space_list_cache: HashMap<String, Vec<(u32, String, String)>>,
    /// Session registry for web dashboard.
    session_registry: Arc<SessionRegistry>,
    /// Current channel info string (for history).
    current_channel_info: Option<String>,
    /// Current channel name (for history).
    current_channel_name: Option<String>,
    /// Shutdown receiver for remote disconnect.
    shutdown_rx: mpsc::Receiver<()>,
    /// TS packet analyzer for this session.
    ts_quality_analyzer: TsPacketAnalyzer,
    /// Carry buffer for outgoing TS alignment (188-byte boundary).
    ts_send_carry: Vec<u8>,
    /// Carry buffer for TS packet alignment (188-byte boundary).
    ts_quality_carry: Vec<u8>,
    /// Accumulated TS quality counters.
    packets_dropped: u64,
    packets_scrambled: u64,
    packets_error: u64,
    bytes_since_last: u64,
    interval_packets_total: u64,
    interval_packets_dropped: u64,
    /// Loss-source breakdown (STREAMING_DESIGN.md §3.1 / §8).
    /// Chunks skipped due to broadcast::Receiver lag. Unit is broadcast
    /// chunks (≤256KB each, ~1394 TS packets), NOT TS packets — do not
    /// add this into `packets_dropped`.
    loss_broadcast_lag_chunks: u64,
    /// Frames dropped because the per-session TS write mpsc buffer
    /// (`TS_WRITE_BUFFER_CAPACITY`) was full.
    loss_ts_queue_chunks: u64,
    /// tsreplace input-queue stall events (Full or Closed).
    loss_encoder_stall_events: u64,
    /// Session start time.
    session_started_at: std::time::Instant,
    /// Signal sampling for average.
    signal_samples: u64,
    signal_level_sum: f64,
    /// Session history DB ID.
    session_history_id: Option<i64>,
    /// Disconnect reason.
    disconnect_reason: Option<String>,
    /// Current BonDriver ID (if resolved).
    current_bon_driver_id: Option<i64>,
    /// Last time we flushed metrics to DB.
    last_db_flush: std::time::Instant,
    /// Previously flushed counters (for computing deltas).
    flushed_packets: u64,
    flushed_dropped: u64,
    flushed_scrambled: u64,
    flushed_error: u64,
    /// Shared encoder pool (tsreplace) reference.
    encoder_pool: Arc<EncoderPool>,
    /// Currently joined shared encoder (tsreplace enabled and running).
    /// The encoder process chain is owned by the pool, not the session.
    current_encoder: Option<Arc<SharedEncoder>>,
    /// Receiver of the shared encoder's output broadcast.
    encoder_output_rx: Option<broadcast::Receiver<Bytes>>,
    /// Fallback to raw TS when the shared encoder fails/stalls/saturates.
    tsreplace_passthrough_on_error: bool,
    /// Whether this session uses single-service TS filtering.
    single_service_filter_enabled: bool,
    /// Per-session TS service filter (active when single_service_filter_enabled
    /// is true and a channel is tuned).
    ts_service_filter: Option<TsServiceFilter>,
    /// Current NID (set after channel selection).
    current_nid: Option<u16>,
    /// Current TSID (set after channel selection).
    current_tsid: Option<u16>,
    /// Current SID (set after channel selection).
    current_sid: Option<u16>,
    /// Stream reliability class (STREAMING_DESIGN.md §2). Set from
    /// `Hello.stream_class`, defaults to `View`; may be auto-promoted to
    /// `Record` by `maybe_promote_stream_class` based on effective priority.
    stream_class: StreamClass,
    /// Fixed-duration prefill / jitter buffer (STREAMING_DESIGN.md §4 P3).
    /// Sits between `send_ts_data`'s 188-byte alignment and `send_ts_frame`'s
    /// class-specific backpressure policy: while filling, wire frames are
    /// queued here instead of being handed to the writer task.
    prefill_buffer: PrefillBuffer,
    /// EWMA of the bitrate actually observed on this session, in bits per
    /// second, or `None` until the first sample lands.
    ///
    /// Both the prefill target and the TS write budget are expressed in
    /// seconds; translating seconds into bytes needs a rate. The per-band
    /// constants in `prefill::default_bitrate_bps` are a starting guess only —
    /// a single-service-filtered stream, or a transcoded one, runs at a small
    /// fraction of the full multiplex, and sizing a buffer for 18 Mbps when the
    /// link actually carries 2 Mbps overshoots by 9x.
    measured_bitrate_bps: Option<u64>,
}

impl Session {
    /// Capacity constant exposed for `handle_connection` in listener.rs.
    pub const CTRL_WRITE_BUFFER_CAPACITY: usize = CTRL_WRITE_BUFFER_CAPACITY;

    /// Create a new session.
    pub fn new(
        id: u64,
        addr: SocketAddr,
        socket_reader: OwnedReadHalf,
        ts_write_queue: TsWriteQueue,
        ctrl_write_tx: mpsc::Sender<Bytes>,
        writer_handle: tokio::task::JoinHandle<()>,
        tuner_pool: Arc<TunerPool>,
        encoder_pool: Arc<EncoderPool>,
        database: DatabaseHandle,
        default_tuner: Option<String>,
        session_registry: Arc<SessionRegistry>,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Self {
        Self {
            id,
            addr,
            socket_reader,
            ts_write_queue,
            ctrl_write_tx,
            writer_handle: Some(writer_handle),
            read_buf: BytesMut::with_capacity(65536),
            state: SessionState::Initial,
            tuner_pool,
            database,
            current_tuner: None,
            reader_state_rx: None,
            warm_tuner: None,
            warm_tuner_path: None,
            current_tuner_path: None,
            tuner_claim_priority: 0,
            tuner_claim_exclusive: false,
            default_tuner,
            current_group_name: None,
            group_driver_paths: Vec::new(),
            ts_receiver: None,
            no_ts_source_since: None,
            ts_bytes_sent: 0,
            ts_msgs_sent: 0,
            last_ts_log: std::time::Instant::now(),
            channel_map_cache: HashMap::new(),
            space_list_cache: HashMap::new(),
            session_registry,
            current_channel_info: None,
            current_channel_name: None,
            shutdown_rx,
            ts_quality_analyzer: TsPacketAnalyzer::new(),
            ts_send_carry: Vec::with_capacity(188 * 8),
            ts_quality_carry: Vec::with_capacity(188 * 8),
            packets_dropped: 0,
            packets_scrambled: 0,
            packets_error: 0,
            bytes_since_last: 0,
            interval_packets_total: 0,
            interval_packets_dropped: 0,
            loss_broadcast_lag_chunks: 0,
            loss_ts_queue_chunks: 0,
            loss_encoder_stall_events: 0,
            session_started_at: std::time::Instant::now(),
            signal_samples: 0,
            signal_level_sum: 0.0,
            session_history_id: None,
            disconnect_reason: None,
            current_bon_driver_id: None,
            last_db_flush: std::time::Instant::now(),
            flushed_packets: 0,
            flushed_dropped: 0,
            flushed_scrambled: 0,
            flushed_error: 0,
            encoder_pool,
            current_encoder: None,
            encoder_output_rx: None,
            tsreplace_passthrough_on_error: true,
            single_service_filter_enabled: false,
            ts_service_filter: None,
            current_nid: None,
            current_tsid: None,
            current_sid: None,
            stream_class: StreamClass::View,
            prefill_buffer: PrefillBuffer::new(),
            measured_bitrate_bps: None,
        }
    }

    /// Promote this session to `StreamClass::Record` if `effective_priority`
    /// crosses the recording threshold (STREAMING_DESIGN.md §2). Never
    /// downgrades — an explicit RECORD from `Hello.stream_class` sticks.
    async fn maybe_promote_stream_class(&mut self, effective_priority: i32) {
        if self.stream_class != StreamClass::Record
            && should_auto_promote_to_record(effective_priority)
        {
            info!(
                "[Session {}] Auto-promoting stream class to RECORD (effective priority {} >= {})",
                self.id, effective_priority, RECORD_PRIORITY_THRESHOLD
            );
            self.stream_class = StreamClass::Record;
            self.session_registry
                .update_stream_class(self.id, self.stream_class)
                .await;
        }
    }

    async fn load_tsreplace_runtime_config(&self) -> TsreplaceRuntimeConfig {
        // BNDP sessions read `tsreplace_config` ONLY; the browser-preview
        // pipeline has its own `preview_encoder_config` (web/stream.rs).
        load_tsreplace_runtime_config(&self.database, self.id).await
    }

    /// Load the fixed-duration prefill/jitter buffer settings from
    /// `tuner_config` (STREAMING_DESIGN.md §4.4 P3). Read directly from the
    /// DB on each call (same pattern as `load_tsreplace_runtime_config`)
    /// rather than cached, so dashboard changes take effect on the next
    /// `StartStream`/channel switch without a server restart.
    async fn load_prefill_runtime_config(&self) -> PrefillRuntimeConfig {
        load_prefill_runtime_config(&self.database, self.id).await
    }

    /// (Re)start the prefill/jitter buffer for the current channel and
    /// stream class (STREAMING_DESIGN.md §4.3). Called on successful
    /// `StartStream` and on completion of a channel switch while already
    /// streaming.
    ///
    /// Sizing (§4.2): `target_bytes = bitrate_bps/8 * prefill_ms/1000 *
    /// safety_factor`, where `bitrate_bps` is a per-band static default
    /// (`current_nid` is classified via `BandType::from_nid`; unresolved NID
    /// — e.g. legacy v1 `SetChannel` clients — falls back to the "unknown"
    /// default). `prefill_ms == 0` fully bypasses prefill for this
    /// class/config (STREAMING_DESIGN.md §4.3).
    async fn reset_prefill_buffer(&mut self) {
        let cfg = self.load_prefill_runtime_config().await;
        let prefill_ms = match self.stream_class {
            StreamClass::View => cfg.view_ms,
            StreamClass::Preview => cfg.preview_ms,
            StreamClass::Record => cfg.record_ms,
        };
        let bitrate_bps = self.sizing_bitrate_bps();
        let target_bytes = prefill_target_bytes(bitrate_bps, prefill_ms, cfg.safety_factor);

        debug!(
            "[Session {}] Prefill buffer reset: class={:?} bitrate_bps={} (measured={:?}) prefill_ms={} safety_factor={} target_bytes={}",
            self.id, self.stream_class, bitrate_bps, self.measured_bitrate_bps,
            prefill_ms, cfg.safety_factor, target_bytes
        );

        self.prefill_buffer.reset(target_bytes);
        self.resize_ts_write_budget().await;
    }

    /// Bitrate to size the time-based buffers with: the measured rate once one
    /// exists, otherwise the per-band default.
    ///
    /// `current_nid` is classified via `BandType::from_nid`; an unresolved NID
    /// (legacy v1 `SetChannel` clients) falls back to the "unknown" default.
    fn sizing_bitrate_bps(&self) -> u64 {
        self.measured_bitrate_bps
            .unwrap_or_else(|| default_bitrate_bps(self.current_nid.map(BandType::from_nid)))
    }

    /// Re-size the TS write queue's byte budget from the configured per-class
    /// duration and the current bitrate estimate (STREAMING_DESIGN.md §3.2).
    ///
    /// Sizing the queue in seconds rather than in frames is what makes one
    /// configuration behave the same across deployments: a frame is one
    /// broadcast chunk, and chunk size depends on how much the reader pulled
    /// from the driver in one go (up to 256 KB direct, typically 64 KB through
    /// another proxy) — so a fixed frame count bought wildly different amounts
    /// of slack on a cascaded link than on a local one.
    async fn resize_ts_write_budget(&mut self) {
        let cfg = load_ts_queue_runtime_config(&self.database, self.id).await;
        let queue_ms = match self.stream_class {
            StreamClass::View => cfg.view_ms,
            StreamClass::Preview => cfg.preview_ms,
            StreamClass::Record => cfg.record_ms,
        };
        let bitrate_bps = self.sizing_bitrate_bps();
        let budget = budget_bytes(bitrate_bps, queue_ms);

        debug!(
            "[Session {}] TS write budget: class={:?} bitrate_bps={} queue_ms={} budget_bytes={} (queued={})",
            self.id, self.stream_class, bitrate_bps, queue_ms, budget,
            self.ts_write_queue.shared().queued()
        );

        self.ts_write_queue.shared().set_budget(budget);
    }

    /// Fold a freshly measured bitrate into the EWMA and re-size the buffers
    /// when the estimate has moved enough to matter.
    ///
    /// Called from the once-a-second stats path. Re-sizing is throttled by a
    /// relative threshold so a stream whose rate jitters by a few percent does
    /// not churn the budget every second.
    async fn observe_bitrate(&mut self, bitrate_mbps: f64) {
        if !bitrate_mbps.is_finite() || bitrate_mbps <= 0.0 {
            return;
        }
        let sample_bps = (bitrate_mbps * 1_000_000.0) as u64;

        let (updated, changed_enough) = match self.measured_bitrate_bps {
            None => (sample_bps, true),
            Some(prev) => {
                let ewma = (prev as f64) * (1.0 - BITRATE_EWMA_ALPHA)
                    + (sample_bps as f64) * BITRATE_EWMA_ALPHA;
                let ewma = ewma.max(1.0) as u64;
                let delta = ewma.abs_diff(prev) as f64 / prev.max(1) as f64;
                (ewma, delta >= 0.25)
            }
        };
        self.measured_bitrate_bps = Some(updated);

        if changed_enough {
            self.resize_ts_write_budget().await;
        }
    }

    /// Detach from the shared encoder (if any).
    ///
    /// The encoder chain itself is owned by the [`EncoderPool`]; releasing
    /// only drops this session's subscription. When the last subscriber
    /// leaves, the pool stops the chain after a short idle grace period, so
    /// zapping back re-joins the still-warm encoder instead of paying the
    /// QSV init cost again (STREAMING_DESIGN.md §5.2 (C)).
    async fn stop_tsreplace_pipeline(&mut self) {
        self.encoder_output_rx = None;
        if let Some(encoder) = self.current_encoder.take() {
            let key = encoder.key.clone();
            self.encoder_pool.release(&key, &encoder).await;
        }
    }

    /// Resolve which SIDs to encode for the current channel.
    ///
    /// - single-service mode: encode only the current SID.
    /// - full-TS mode: encode all SIDs in the NID+TSID group.
    /// - If NID/TSID are unknown: returns empty (no --service injection).
    async fn resolve_encode_sids(&self) -> Vec<u16> {
        resolve_encode_sids(
            &self.database,
            self.id,
            self.single_service_filter_enabled,
            self.current_sid,
            self.current_nid,
            self.current_tsid,
        )
        .await
    }

    /// Attach this session to a shared encoder (tsreplace) for the current
    /// channel, creating one via the [`EncoderPool`] if needed.
    ///
    /// The per-session process pipeline that used to live here (single
    /// process + multi-SID OS-pipe chain) has moved to
    /// `crate::tuner::encoder_pool`; sessions now only subscribe to the
    /// shared encoder's output broadcast (STREAMING_DESIGN.md §5 P4).
    ///
    /// Returns `Ok(())` on success, on tsreplace-disabled, and on pool
    /// saturation (the latter falls back to raw TS passthrough with an info
    /// log — saturation is an expected admission-control outcome, not an
    /// error). Returns `Err` only when spawning a new encoder chain failed;
    /// callers then apply the `passthrough_on_error` policy.
    async fn start_tsreplace_pipeline(&mut self) -> std::io::Result<()> {
        self.stop_tsreplace_pipeline().await;

        let cfg = self.load_tsreplace_runtime_config().await;
        self.tsreplace_passthrough_on_error = cfg.passthrough_on_error;

        if !cfg.enabled {
            return Ok(());
        }

        let Some(tuner) = self.current_tuner.clone() else {
            // No tuner yet — nothing to encode; raw path handles it.
            return Ok(());
        };

        // Apply the configured concurrency cap. Affects this and subsequent
        // encoder creations only; already-running encoders keep their slots.
        self.encoder_pool
            .set_max_concurrent(cfg.max_concurrent_encoders.max(1) as usize)
            .await;

        // If the user's argument template already carries --service, no
        // per-SID auto-injection happens; normalize the SID set to empty so
        // every such session on this channel shares a single encoder.
        let sids = if encoder_pool::args_contain_service_option(&cfg.arguments) {
            Vec::new()
        } else {
            self.resolve_encode_sids().await
        };

        let runtime = EncoderRuntimeConfig {
            command_path: cfg.command_path,
            arguments: cfg.arguments,
            read_timeout_ms: cfg.read_timeout_ms,
            preprocessor_path: cfg.preprocessor_path,
            preprocessor_arguments: cfg.preprocessor_arguments,
        };
        let generation = encoder_pool::config_generation(&runtime);
        let key = EncodeKey::new(tuner.key.clone(), sids, generation);

        match self.encoder_pool.get_or_create(key, tuner, runtime).await {
            Ok(encoder) => {
                self.encoder_output_rx = Some(encoder.subscribe());
                info!(
                    "[Session {}] joined shared encoder for {:?} (sids={:?}, subscribers={})",
                    self.id,
                    encoder.key.channel_key,
                    encoder.key.sids,
                    encoder.subscriber_count()
                );
                self.current_encoder = Some(encoder);
                Ok(())
            }
            Err(EncoderPoolError::Saturated) => {
                // Admission control: no encoder slot free and no running
                // encoder to join. Fall back to raw TS passthrough
                // (STREAMING_DESIGN.md §5.2 (B)).
                info!(
                    "[Session {}] encoder pool saturated ({} rejections total), falling back to raw TS passthrough",
                    self.id,
                    self.encoder_pool.saturated_count()
                );
                Ok(())
            }
            Err(EncoderPoolError::SpawnFailed(e)) => {
                Err(std::io::Error::new(std::io::ErrorKind::Other, e))
            }
        }
    }

    async fn restart_tsreplace_pipeline_if_streaming(&mut self) {
        if self.state != SessionState::Streaming {
            return;
        }

        if let Err(e) = self.start_tsreplace_pipeline().await {
            if self.tsreplace_passthrough_on_error {
                warn!(
                    "[Session {}] tsreplace restart failed on channel switch, fallback to raw TS: {}",
                    self.id, e
                );
                self.stop_tsreplace_pipeline().await;
            } else {
                warn!(
                    "[Session {}] tsreplace restart failed on channel switch: {}",
                    self.id, e
                );
            }
        }
    }

    /// Get a reference to the database.
    #[allow(dead_code)]
    pub fn database(&self) -> &DatabaseHandle {
        &self.database
    }

    async fn refresh_current_bon_driver_id(&mut self) {
        if let Some(path) = &self.current_tuner_path {
            let db = self.database.lock().await;
            self.current_bon_driver_id =
                db.get_bon_driver_by_path(path).ok().flatten().map(|d| d.id);
        } else {
            self.current_bon_driver_id = None;
        }
    }

    async fn set_selected_tuner_path(&mut self, path: &str) {
        self.current_tuner_path = Some(path.to_string());
        self.refresh_current_bon_driver_id().await;
    }

    async fn set_selected_tuner_path_and_registry(&mut self, path: &str) {
        self.set_selected_tuner_path(path).await;
        self.session_registry
            .update_tuner(self.id, Some(path.to_string()))
            .await;
    }

    async fn stop_warm_tuner(&mut self) {
        if let Some(warm) = self.warm_tuner.take() {
            warm.shutdown().await;
        }
        self.warm_tuner_path = None;
    }

    async fn maybe_start_warm_tuner(&mut self, tuner_path: &str) {
        let config = self.tuner_pool.config().await;
        if !config.prewarm_enabled {
            return;
        }

        // ★ Don't open the driver a second time if it is already being used by
        // an active reader.  Some BonDriver DLLs maintain shared global state
        // (e.g. a singleton IBonDriver pointer set by CreateBonDriver()), so a
        // second OpenTuner() call from the warm-tuner thread can overwrite that
        // pointer and destroy the first reader's IBonDriver, causing the running
        // stream to cut out immediately.
        let already_running = {
            let keys = self.tuner_pool.keys().await;
            let mut found = false;
            for k in &keys {
                if k.tuner_path == tuner_path {
                    if let Some(t) = self.tuner_pool.get(k).await {
                        // Slot occupancy: prewarming a DLL that another task
                        // is already opening is exactly the double-open this
                        // guard exists to prevent.
                        if t.occupies_slot() {
                            found = true;
                            break;
                        }
                    }
                }
            }
            found
        };
        if already_running {
            debug!(
                "[Session {}] Skipping warm tuner for {} – driver already has a running reader",
                self.id, tuner_path
            );
            return;
        }

        // docs/TUNER_PIPELINE_REDESIGN.md P1b §5: prewarming now reserves a
        // real driver slot for the entire time it sits warm, replacing the
        // `occupies_slot()` scan above as the *enforcement* for capacity —
        // that scan only ever caught an already-pool-visible reader, never
        // another warm handle (§2.1-4's "warm tuner is invisible to capacity
        // accounting" gap). Skip prewarming outright if the driver has no
        // spare slot; a failed prewarm is just a missed optimization, not a
        // user-visible error, so there is no fallback path to design here.
        let max_instances = driver_max_instances(&self.database, tuner_path).await;
        let Some(permit) = self
            .tuner_pool
            .acquire_slot(tuner_path, max_instances)
            .await
        else {
            debug!(
                "[Session {}] Skipping warm tuner for {} – driver at capacity ({} instance(s))",
                self.id, tuner_path, max_instances
            );
            return;
        };

        self.stop_warm_tuner().await;

        let warm =
            WarmTunerHandle::spawn(tuner_path.to_string(), config.prewarm_timeout_secs, permit);
        self.warm_tuner_path = Some(tuner_path.to_string());
        self.warm_tuner = Some(warm);
    }

    /// After a channel switch failure, attempt to restore the previous channel so the
    /// client (TVTest, etc.) keeps receiving TS data instead of being cut off.
    ///
    /// The old tuner may still be alive in the pool when `keep_alive_secs > 0` (default 60 s).
    /// If it is still running we cancel the idle-close timer and re-subscribe.
    ///
    /// When the restore does not happen (no old key, gone from the pool,
    /// already stopped) and this session was streaming, it is now left with
    /// no TS source at all. That is not recoverable from here — the run
    /// loop's `NO_TS_SOURCE_GRACE` watchdog disconnects it shortly after, so
    /// the client can reconnect instead of holding a dead stream open.
    async fn try_restore_previous_channel(&mut self, old_tuner_key: &Option<ChannelKey>) {
        let streaming = self.state == SessionState::Streaming;
        if streaming && self.ts_receiver.is_none() && self.current_encoder.is_none() {
            // Start the clock here rather than on the next idle tick, so the
            // grace period is measured from the failure itself.
            self.no_ts_source_since
                .get_or_insert_with(std::time::Instant::now);
        }
        let Some(ref old_key) = old_tuner_key else {
            return;
        };
        let Some(old_tuner) = self.tuner_pool.get(old_key).await else {
            warn!("[Session {}] Channel switch failed but old tuner {:?} is no longer in pool; cannot restore",
                  self.id, old_key);
            return;
        };
        if !old_tuner.is_running() {
            warn!("[Session {}] Channel switch failed but old tuner {:?} has already stopped; cannot restore",
                  self.id, old_key);
            return;
        }
        info!(
            "[Session {}] Channel switch failed — restoring previous channel {:?}",
            self.id, old_key
        );
        // Cancel any pending idle-close so the tuner stays alive.
        self.tuner_pool.cancel_idle_close(old_key).await;
        self.current_tuner = Some(old_tuner.clone());
        // If we were (or are still) streaming, re-subscribe so TS data flows again.
        if self.state == SessionState::Streaming && self.ts_receiver.is_none() {
            self.ts_receiver = Some(old_tuner.subscribe_with_claim_class(
                self.tuner_claim_priority,
                self.tuner_claim_exclusive,
                crate::tuner::shared::TunerUsage::from(self.stream_class),
            ));
        }
    }

    /// Get channel map for a specific space and region (for virtual space filtering).
    /// Cached per region_key: TVTest calls EnumChannelName once per channel,
    /// so without the cache every call would re-scan the whole channels table
    /// under the DB mutex. Cleared by `clear_caches` (every OpenTuner).
    async fn ensure_channel_map_with_region(
        &mut self,
        _space: u32,
        region_name: &str,
    ) -> Vec<ChannelEntry> {
        ensure_channel_map_with_region_cached(
            &self.database,
            self.id,
            &self.group_driver_paths,
            &self.current_or_default_tuner_path(),
            &mut self.channel_map_cache,
            region_name,
        )
        .await
    }

    fn clear_caches(&mut self) {
        clear_session_caches(&mut self.channel_map_cache, &mut self.space_list_cache);
    }

    fn current_or_default_tuner_path(&self) -> String {
        current_or_default_tuner_path(&self.current_tuner_path, &self.default_tuner)
    }

    /// チューナに紐づく「実スペース一覧」を DB から構築してキャッシュする
    async fn ensure_space_list(&mut self) -> Vec<u32> {
        ensure_space_list_cached(
            &self.database,
            self.id,
            &self.group_driver_paths,
            self.current_group_name.as_deref(),
            &self.current_or_default_tuner_path(),
            &mut self.space_list_cache,
        )
        .await
    }

    /// Map virtual space index to (actual_space, region_key) for filtering.
    /// Returns the region_key (e.g., "宮城", "BS", "CS") used for channel matching,
    /// NOT the display name (which may differ, e.g., "地デジ").
    async fn map_space_idx_to_actual_with_region(
        &mut self,
        space_idx: u32,
    ) -> Option<(u32, String)> {
        map_space_idx_to_actual_with_region_cached(
            &self.group_driver_paths,
            self.current_group_name.as_deref(),
            &self.current_or_default_tuner_path(),
            &self.space_list_cache,
            space_idx,
        )
    }

    /// Get space list with names (for internal use).
    /// Returns Vec<(actual_space, display_name, region_key)>.
    async fn get_space_list_with_names(&mut self) -> Vec<(u32, String, String)> {
        get_space_list_with_names_cached(
            &self.group_driver_paths,
            self.current_group_name.as_deref(),
            &self.current_or_default_tuner_path(),
            &self.space_list_cache,
        )
    }

    /// Run the session, processing messages until disconnection.
    /// Run the session to completion, releasing its tuner whichever way it
    /// ends.
    ///
    /// The loop propagates socket errors with `?`, and a client that simply
    /// goes away produces one (`Connection reset by peer`) rather than a
    /// clean EOF. That used to return straight out of `run`, skipping
    /// `cleanup` — so the session's reader kept running with nobody
    /// subscribed and no keep-alive close scheduled. Since P1b that also
    /// means its slot permit is never released: the driver stays occupied
    /// for the life of the process, invisible to everyone (an entry with no
    /// subscribers *and* no pending idle-close is not reclaimable by design,
    /// because that shape normally means "being set up right now").
    pub async fn run(&mut self) -> std::io::Result<()> {
        let result = self.run_loop().await;
        self.cleanup().await;
        result
    }

    async fn run_loop(&mut self) -> std::io::Result<()> {
        // Insert session start record
        let started_at = chrono::Utc::now().timestamp();
        if let Ok(db) = self.database.lock().await.insert_session_start(
            self.id,
            &self.addr.to_string(),
            self.current_tuner_path.as_deref(),
            self.current_channel_info.as_deref(),
            self.current_channel_name.as_deref(),
            started_at,
        ) {
            self.session_history_id = Some(db);
        } else {
            warn!(
                "[Session {}] Failed to insert session history start",
                self.id
            );
        }

        // Detects a tuner reader that stops out from under us: displaced to
        // free a slot for a higher-priority request, a DLL crash, a hardware
        // error. Without it, `broadcast::Receiver::recv()` blocks forever
        // when the reader dies but the `SharedTuner` Arc is still alive,
        // leaving the session hanging with no data and no error.
        //
        // P4: this used to be a 2-second poll of `is_running()`, so a
        // displaced viewer sat on a dead stream for up to two seconds and
        // was then dropped with a generic "reader_stopped". Watching the
        // state channel wakes the session on the transition itself and
        // carries `StopReason` with it, so the disconnect says *why*.

        loop {
            // Process any complete messages in the buffer first
            if let Some(msg) = self.try_decode_message()? {
                if !self.handle_message(msg).await? {
                    break;
                }
                continue;
            }

            // If streaming, we need to handle both incoming messages and TS data
            // Only handle TS data if we are actually streaming
            if self.state == SessionState::Streaming {
                // A source is attached again (or still): forget the deadline.
                if self.ts_receiver.is_some() || self.current_encoder.is_some() {
                    self.no_ts_source_since = None;
                }

                // Create futures for socket read and TS receive
                let mut tmp_buf = [0u8; 4096];

                tokio::select! {
                    // NOTE: `biased` is intentionally NOT used here.
                    // In multi-hop proxy chains, biased polling caused TS data
                    // and socket reads to be starved by higher-priority branches,
                    // leading to command processing delays and cascading
                    // backpressure.  Fair (random) polling ensures all branches
                    // make progress.

                    // Remote shutdown request
                    _ = self.shutdown_rx.recv() => {
                        self.disconnect_reason = Some("remote_shutdown".to_string());
                        break;
                    }

                    // Our tuner's reader changed state. Anything that is not
                    // `Running` while we are streaming means the stream is
                    // over — react immediately rather than on a poll tick.
                    changed = async {
                        match self.reader_state_rx.as_mut() {
                            Some(rx) => rx.changed().await.is_ok(),
                            None => std::future::pending::<bool>().await,
                        }
                    } => {
                        if !changed {
                            // Sender dropped: the SharedTuner is gone.
                            self.reader_state_rx = None;
                        } else if let Some(tuner) = self.current_tuner.clone() {
                            if !tuner.is_running() {
                                let reason = tuner.stop_reason();
                                match reason {
                                    StopReason::Evicted => warn!(
                                        "[Session {}] Tuner {:?} was displaced to free a slot for a higher-priority request; disconnecting",
                                        self.id, tuner.key
                                    ),
                                    StopReason::ReaderFailed => warn!(
                                        "[Session {}] Tuner {:?} reader failed (driver or hardware error); disconnecting",
                                        self.id, tuner.key
                                    ),
                                    _ => warn!(
                                        "[Session {}] Tuner {:?} stopped externally (state={:?}); disconnecting",
                                        self.id, tuner.key, tuner.state()
                                    ),
                                }
                                self.disconnect_reason = Some(reason.as_str().to_string());
                                break;
                            }
                        }
                    }

                    // Check for incoming socket data (client commands).
                    // Prioritized above tsreplace/TS data so that StopStream,
                    // SetChannel etc. are handled promptly even under load.
                    result = self.socket_reader.read(&mut tmp_buf) => {
                        let n = result?;
                        if n == 0 {
                            self.disconnect_reason = Some("client_disconnect".to_string());
                            break; // Connection closed
                        }
                        self.read_buf.extend_from_slice(&tmp_buf[..n]);
                    }

                    // Encoded output from the shared encoder (tsreplace).
                    // The encoder chain is fed by the EncoderPool's own task
                    // from the tuner broadcast; this session only consumes
                    // the encoder's output broadcast.
                    encoded_result = async {
                        if let Some(rx) = &mut self.encoder_output_rx {
                            Some(rx.recv().await)
                        } else {
                            std::future::pending::<Option<Result<Bytes, broadcast::error::RecvError>>>().await
                        }
                    } => {
                        match encoded_result {
                            Some(Ok(data)) => {
                                if self.send_ts_data(data).await? {
                                    break;
                                }
                            }
                            Some(Err(broadcast::error::RecvError::Lagged(count))) => {
                                // STREAMING_DESIGN.md §3.2 / §12-1: a RECORD
                                // session must not silently lose data. Falling
                                // behind a broadcast channel (even the encoder
                                // output one) means it can't keep up at all —
                                // disconnect instead of resyncing and
                                // continuing (the VIEW/PREVIEW recovery path).
                                if self.stream_class == StreamClass::Record {
                                    error!("[Session {}] RECORD session lagged on encoder output ({} chunks skipped), disconnecting", self.id, count);
                                    self.disconnect_reason = Some("record_broadcast_lag".to_string());
                                    break;
                                }
                                warn!("[Session {}] Encoder output receiver lagged, skipped {} chunks — recovering", self.id, count);
                                self.loss_broadcast_lag_chunks += count;
                                // Same recovery as tuner-broadcast lag: clear
                                // carry buffers so the next chunk re-aligns.
                                self.ts_send_carry.clear();
                                self.ts_quality_carry.clear();
                                // The gap is already counted as
                                // loss_broadcast_lag_chunks; drop the CC
                                // baseline so the resync isn't double-counted
                                // as one packets_dropped per PID.
                                self.ts_quality_analyzer.mark_discontinuity();
                            }
                            Some(Err(broadcast::error::RecvError::Closed)) => {
                                // The shared encoder stopped: watchdog stall,
                                // chain EOF/crash, or pool idle-close raced
                                // with us. Counted as an encoder stall event
                                // (P1 loss-source counter — previously wired
                                // to the input-queue Full/Closed cases).
                                warn!("[Session {}] Shared encoder output closed", self.id);
                                self.loss_encoder_stall_events += 1;
                                if self.tsreplace_passthrough_on_error {
                                    self.stop_tsreplace_pipeline().await;
                                } else {
                                    self.disconnect_reason = Some("tsreplace_output_closed".to_string());
                                    break;
                                }
                            }
                            None => {}
                        }
                    }

                    // Check for incoming TS data.
                    //
                    // While a shared encoder is active the session's output
                    // comes from the encoder branch above, so this branch is
                    // parked entirely (P3 §2.2-12) instead of waking once per
                    // chunk only to drop it. The `ts_receiver` subscription
                    // itself is kept: it is what makes the tuner's keep-alive
                    // / idle-close accounting session-driven. An unpolled
                    // broadcast receiver simply falls behind — the ring holds
                    // one copy of each chunk regardless of how many receivers
                    // there are, so nothing accumulates per session.
                    ts_result = async {
                        match (&mut self.ts_receiver, self.current_encoder.is_some()) {
                            (_, true) => std::future::pending::<Option<Result<Bytes, broadcast::error::RecvError>>>().await,
                            (Some(rx), false) => Some(rx.recv().await),
                            (None, false) => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                None
                            }
                        }
                    } => {
                        match ts_result {
                            Some(Ok(data)) => {
                                if self.send_ts_data(data).await? {
                                    break;
                                }
                            }
                            Some(Err(broadcast::error::RecvError::Lagged(count))) => {
                                // STREAMING_DESIGN.md §3.2 / §12-1: RECORD must
                                // not silently lose data. Lagging behind the
                                // tuner's own broadcast is worse than a full
                                // write-queue — it means the session can't
                                // keep up with the source at all — so
                                // disconnect rather than resync-and-continue.
                                if self.stream_class == StreamClass::Record {
                                    error!("[Session {}] RECORD session lagged on tuner broadcast ({} chunks skipped), disconnecting", self.id, count);
                                    self.disconnect_reason = Some("record_broadcast_lag".to_string());
                                    break;
                                }
                                warn!("[Session {}] Broadcast receiver lagged, skipped {} chunks — recovering", self.id, count);
                                // `count` is a number of broadcast chunks (each up to
                                // 256KB / ~1394 TS packets), NOT a TS packet count.
                                // Track it separately rather than folding it into
                                // `packets_dropped` (see STREAMING_DESIGN.md §3.1).
                                self.loss_broadcast_lag_chunks += count;
                                // Recovery: clear the TS carry buffers so we don't
                                // send partial/stale packets after the gap.  The
                                // next received chunk will start a fresh alignment.
                                self.ts_send_carry.clear();
                                self.ts_quality_carry.clear();
                                // The gap is already counted as
                                // loss_broadcast_lag_chunks; drop the CC
                                // baseline so the resync isn't double-counted
                                // as one packets_dropped per PID.
                                self.ts_quality_analyzer.mark_discontinuity();
                            }
                            Some(Err(broadcast::error::RecvError::Closed)) => {
                                info!("[Session {}] Broadcast channel closed", self.id);
                                self.disconnect_reason = Some("broadcast_closed".to_string());
                                break;
                            }
                            None => {
                                // Idle tick from the "no TS source" arm above.
                                // Streaming with nothing attached is a lost
                                // tuner, not a quiet moment: give the restore
                                // paths a short grace period, then disconnect
                                // instead of parking here forever
                                // (`NO_TS_SOURCE_GRACE`).
                                if no_ts_source_deadline_passed(
                                    &mut self.no_ts_source_since,
                                    std::time::Instant::now(),
                                    NO_TS_SOURCE_GRACE,
                                ) {
                                    error!(
                                        "[Session {}] Streaming with no TS source for {:?} (channel switch left this session without a tuner); disconnecting",
                                        self.id, NO_TS_SOURCE_GRACE
                                    );
                                    self.disconnect_reason = Some("tuner_lost".to_string());
                                    break;
                                }
                            }
                        }
                    }
                }
            } else {
                // Not streaming, just wait for messages or shutdown.
                //
                // A session that has not opened a tuner is also bounded in
                // time (`IDLE_NO_TUNER_TIMEOUT`): TCP keepalive
                // (`listener.rs`) catches a peer that is gone, but not one
                // that is merely holding the socket open with nothing behind
                // it. A `TunerOpen` session is deliberately excluded — a host
                // may legitimately keep a tuner open without streaming.
                let idle_limit = if self.current_tuner.is_none() {
                    Some(IDLE_NO_TUNER_TIMEOUT)
                } else {
                    None
                };
                let socket = &mut self.socket_reader;
                let read_buf = &mut self.read_buf;
                let shutdown_rx = &mut self.shutdown_rx;

                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        self.disconnect_reason = Some("remote_shutdown".to_string());
                        break;
                    }
                    result = async {
                        match idle_limit {
                            Some(limit) => tokio::time::timeout(
                                limit,
                                Self::read_message_with(socket, read_buf, self.id),
                            )
                            .await
                            .map_err(|_| ()),
                            None => Ok(Self::read_message_with(socket, read_buf, self.id).await),
                        }
                    } => {
                        let Ok(result) = result else {
                            info!(
                                "[Session {}] Idle for {:?} with no tuner open; disconnecting",
                                self.id, IDLE_NO_TUNER_TIMEOUT
                            );
                            self.disconnect_reason = Some("idle_timeout".to_string());
                            break;
                        };
                        match result? {
                            Some(msg) => {
                                if !self.handle_message(msg).await? {
                                    break;
                                }
                            }
                            None => {
                                self.disconnect_reason = Some("client_disconnect".to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Try to decode a complete message from the buffer.
    fn try_decode_message(&mut self) -> std::io::Result<Option<ClientMessage>> {
        if self.read_buf.len() < HEADER_SIZE {
            return Ok(None);
        }

        match decode_header(&self.read_buf) {
            Ok(Some(header)) => {
                let total_len = HEADER_SIZE + header.payload_len as usize;
                if self.read_buf.len() >= total_len {
                    // We have a complete frame
                    let _ = self.read_buf.split_to(HEADER_SIZE);
                    let payload = self.read_buf.split_to(header.payload_len as usize);

                    match decode_client_message(header.message_type, Bytes::from(payload.to_vec()))
                    {
                        Ok(msg) => {
                            debug!("[Session {}] Decoded message: {:?}", self.id, msg);
                            Ok(Some(msg))
                        }
                        Err(e) => {
                            error!("[Session {}] Failed to decode message: {}", self.id, e);
                            Ok(None)
                        }
                    }
                } else {
                    Ok(None) // Need more data
                }
            }
            Ok(None) => Ok(None), // Need more data
            Err(e) => {
                error!("[Session {}] Protocol error: {}", self.id, e);
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            }
        }
    }

    /// Read and decode a client message (borrowed socket/buffer).
    async fn read_message_with(
        socket: &mut OwnedReadHalf,
        read_buf: &mut BytesMut,
        session_id: u64,
    ) -> std::io::Result<Option<ClientMessage>> {
        loop {
            // Try to decode a header from the buffer
            if read_buf.len() >= HEADER_SIZE {
                match decode_header(read_buf) {
                    Ok(Some(header)) => {
                        let total_len = HEADER_SIZE + header.payload_len as usize;
                        if read_buf.len() >= total_len {
                            // We have a complete frame
                            let _ = read_buf.split_to(HEADER_SIZE);
                            let payload = read_buf.split_to(header.payload_len as usize);

                            match decode_client_message(
                                header.message_type,
                                Bytes::from(payload.to_vec()),
                            ) {
                                Ok(msg) => {
                                    trace!("[Session {}] Received: {:?}", session_id, msg);
                                    return Ok(Some(msg));
                                }
                                Err(e) => {
                                    error!(
                                        "[Session {}] Failed to decode message: {}",
                                        session_id, e
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        // Need more data
                    }
                    Err(e) => {
                        error!("[Session {}] Protocol error: {}", session_id, e);
                        return Ok(None);
                    }
                }
            }

            // Read more data from socket
            let mut tmp_buf = [0u8; 4096];
            let n = socket.read(&mut tmp_buf).await?;
            if n == 0 {
                return Ok(None); // Connection closed
            }
            read_buf.extend_from_slice(&tmp_buf[..n]);
        }
    }

    /// Handle a client message. Returns false to close the session.
    async fn handle_message(&mut self, msg: ClientMessage) -> std::io::Result<bool> {
        match msg {
            ClientMessage::Hello {
                version,
                stream_class,
            } => {
                self.handle_hello(version, stream_class).await?;
            }
            ClientMessage::Ping => {
                self.send_message(ServerMessage::Pong).await?;
            }
            ClientMessage::OpenTuner { tuner_path } => {
                self.handle_open_tuner(tuner_path).await?;
            }
            ClientMessage::OpenTunerWithGroup { group_name } => {
                // Reuse OpenTuner path resolution (group_name is supported there).
                self.handle_open_tuner(group_name).await?;
            }
            ClientMessage::CloseTuner => {
                self.handle_close_tuner().await?;
            }
            ClientMessage::SetChannel {
                channel,
                priority,
                exclusive,
            } => {
                self.handle_set_channel(channel, priority, exclusive)
                    .await?;
            }
            ClientMessage::SetChannelSpace {
                space,
                channel,
                priority,
                exclusive,
            } => {
                self.handle_set_channel_space(space, channel, priority, exclusive)
                    .await?;
            }
            ClientMessage::SetChannelSpaceInGroup {
                group_name,
                space_idx,
                channel,
                priority,
                exclusive,
            } => {
                // Group mode is handled by `handle_set_channel_space` via current group context.
                // Keep the explicit group open path for compatibility.
                if self.current_group_name.as_deref() != Some(group_name.as_str()) {
                    self.handle_open_tuner(group_name).await?;
                }
                self.handle_set_channel_space(space_idx, channel, priority, exclusive)
                    .await?;
            }
            ClientMessage::GetSignalLevel => {
                self.handle_get_signal_level().await?;
            }
            ClientMessage::EnumTuningSpace { space } => {
                self.handle_enum_tuning_space(space).await?;
            }
            ClientMessage::EnumChannelName { space, channel } => {
                self.handle_enum_channel_name(space, channel).await?;
            }
            ClientMessage::StartStream => {
                self.handle_start_stream().await?;
            }
            ClientMessage::StopStream => {
                self.handle_stop_stream().await?;
            }
            ClientMessage::PurgeStream => {
                self.handle_purge_stream().await?;
            }
            ClientMessage::SetLnbPower { enable } => {
                self.handle_set_lnb_power(enable).await?;
            }
            ClientMessage::SelectLogicalChannel { nid, tsid, sid } => {
                self.handle_select_logical_channel(nid, tsid, sid).await?;
            }
            ClientMessage::GetChannelList { filter } => {
                self.handle_get_channel_list(filter).await?;
            }
            ClientMessage::SetServiceFilter { single_service } => {
                self.handle_set_service_filter(single_service).await?;
            }
        }
        Ok(true)
    }

    /// Handle Hello message.
    ///
    /// docs/DESIGN.md §3 compat policy: `MessageType` values never change and
    /// payload growth happens via `Hello.version` negotiation. Protocol v2
    /// added `stream_class`; older (v1) clients simply never send it (the
    /// codec defaults them to `StreamClass::View`), so a v1 client is still
    /// accepted here — only truly unknown/future versions are rejected.
    async fn handle_hello(
        &mut self,
        version: u16,
        stream_class: StreamClass,
    ) -> std::io::Result<()> {
        info!(
            "[Session {}] Client hello, version {}, stream_class {:?}",
            self.id, version, stream_class
        );

        let success = version >= 1 && version <= PROTOCOL_VERSION;
        if success {
            self.state = SessionState::Ready;
            self.stream_class = stream_class;
            self.session_registry
                .update_stream_class(self.id, self.stream_class)
                .await;
        }

        self.send_message(ServerMessage::HelloAck {
            version: PROTOCOL_VERSION,
            success,
        })
        .await
    }

    /// Handle OpenTuner message.
    async fn handle_open_tuner(&mut self, tuner_path: String) -> std::io::Result<()> {
        if self.state != SessionState::Ready {
            return self
                .send_error(ErrorCode::InvalidState, "Not in ready state")
                .await;
        }

        let path = if tuner_path.is_empty() {
            match &self.default_tuner {
                Some(p) => p.clone(),
                None => {
                    return self.fail_open_tuner(ErrorCode::InvalidParameter).await;
                }
            }
        } else {
            tuner_path
        };

        // ★ Resolve: DLL path -> group name -> display_name -> first driver.
        // Steps 1-3 are the shared `Database::resolve_tuner_target` (also
        // used by /api/client-view, so the dashboard's guide can never
        // disagree with this resolution). Step 4 (first-available fallback)
        // is session-only leniency for misconfigured clients.
        let (resolved_path, is_group) = {
            let db = self.database.lock().await;

            match db.resolve_tuner_target(&path) {
                Ok(Some((driver_paths, true))) => {
                    debug!(
                        "[Session {}] Tuner '{}' matched as group_name (drivers: {})",
                        self.id,
                        path,
                        driver_paths.len()
                    );
                    (path.clone(), true)
                }
                Ok(Some((driver_paths, false))) => {
                    debug!(
                        "[Session {}] Tuner '{}' resolved to DLL: {}",
                        self.id, path, driver_paths[0]
                    );
                    (driver_paths.into_iter().next().unwrap(), false)
                }
                Ok(None) => {
                    // 4. Use first available driver
                    warn!(
                        "[Session {}] Tuner '{}' not found, trying first available driver",
                        self.id, path
                    );
                    match db.get_all_bon_drivers() {
                        Ok(drivers) => match drivers.first() {
                            Some(driver) => {
                                warn!(
                                    "[Session {}] Using driver: {} (path: {})",
                                    self.id,
                                    driver.driver_name.as_ref().unwrap_or(&driver.dll_path),
                                    driver.dll_path
                                );
                                (driver.dll_path.clone(), false)
                            }
                            None => {
                                error!("[Session {}] No drivers found in database at all", self.id);
                                drop(db);
                                return self.fail_open_tuner(ErrorCode::InvalidParameter).await;
                            }
                        },
                        Err(e) => {
                            error!("[Session {}] Failed to query drivers: {}", self.id, e);
                            drop(db);
                            return self.fail_open_tuner(ErrorCode::InvalidParameter).await;
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "[Session {}] Database error resolving tuner: {}",
                        self.id, e
                    );
                    drop(db);
                    return self.fail_open_tuner(ErrorCode::TunerOpenFailed).await;
                }
            }
        }; // db is dropped here

        info!(
            "[Session {}] Opening tuner: {} (group: {})",
            self.id, path, is_group
        );

        // If group, load all drivers in the group
        if is_group {
            let db = self.database.lock().await;
            match db.get_group_drivers(&path) {
                Ok(drivers) => {
                    self.group_driver_paths = drivers.iter().map(|d| d.dll_path.clone()).collect();
                    self.current_group_name = Some(path.clone());
                    info!(
                        "[Session {}] Loaded group '{}' with {} drivers: {:?}",
                        self.id,
                        path,
                        self.group_driver_paths.len(),
                        self.group_driver_paths
                    );
                }
                Err(e) => {
                    error!("[Session {}] Failed to load group drivers: {}", self.id, e);
                    drop(db);
                    return self.fail_open_tuner(ErrorCode::TunerOpenFailed).await;
                }
            }
        } else {
            self.set_selected_tuner_path(&resolved_path).await;
            self.current_group_name = None;
            self.group_driver_paths.clear();
            self.maybe_start_warm_tuner(&resolved_path).await;
        }

        if is_group {
            self.stop_warm_tuner().await;
        }

        self.clear_caches();

        // ★ Initialize space list cache (for proper virtual space handling)
        self.ensure_space_list().await;

        self.state = SessionState::TunerOpen;

        // Update session registry
        self.session_registry
            .update_tuner(self.id, Some(path))
            .await;

        self.send_message(ServerMessage::OpenTunerAck {
            success: true,
            error_code: 0,
            bondriver_version: 2,
        })
        .await
    }

    /// Handle CloseTuner message.
    async fn handle_close_tuner(&mut self) -> std::io::Result<()> {
        info!("[Session {}] Closing tuner", self.id);

        self.cleanup().await;
        self.state = SessionState::Ready;
        self.clear_caches();

        self.send_message(ServerMessage::CloseTunerAck { success: true })
            .await
    }

    /// Handle SetChannel message (IBonDriver v1 style).
    async fn handle_set_channel(
        &mut self,
        channel: u8,
        priority: i32,
        exclusive: bool,
    ) -> std::io::Result<()> {
        if self.state != SessionState::TunerOpen && self.state != SessionState::Streaming {
            return self
                .send_error(ErrorCode::InvalidState, "Tuner not open")
                .await;
        }

        self.session_registry
            .update_client_controls(self.id, Some(priority), Some(exclusive))
            .await;
        let (effective_priority_opt, effective_exclusive) = self
            .session_registry
            .get_effective_controls(self.id)
            .await
            .unwrap_or((Some(priority), exclusive));
        let effective_priority = effective_priority_opt.unwrap_or(priority);
        self.tuner_claim_priority = effective_priority;
        self.tuner_claim_exclusive = effective_exclusive;

        let tuner_path = match &self.current_tuner_path {
            Some(p) => p.clone(),
            None => {
                return self
                    .send_message(ServerMessage::SetChannelAck {
                        success: false,
                        error_code: ErrorCode::InvalidState.into(),
                    })
                    .await;
            }
        };

        info!(
            "[Session {}] SetChannel: {} on {}",
            self.id, channel, tuner_path
        );

        // Priority and exclusivity are independent rank components. An
        // exclusive request with no explicit priority inherits the DB default
        // instead of being rewritten to i32::MAX. The same canonical values
        // are stored for the eventual incumbent subscription below.
        let channel_priority_for_class = if effective_priority > 0 {
            effective_priority
        } else {
            let db = self.database.lock().await;
            db.get_channel_priority(&tuner_path, 0, channel as u32)
                .unwrap_or(Some(0))
                .unwrap_or(0)
        };
        self.tuner_claim_priority = channel_priority_for_class;
        self.tuner_claim_exclusive = effective_exclusive;
        self.maybe_promote_stream_class(channel_priority_for_class)
            .await;

        let key = ChannelKey::simple(&tuner_path, channel);
        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());

        // Permit handoff, same as the v2 path: extract the current tuner's
        // slot before anything can stop it, so a same-DLL switch on a
        // `max_instances = 1` driver keeps its reservation instead of
        // releasing and re-acquiring (docs/TUNER_PIPELINE_REDESIGN.md P1b §4).
        let inherited_permit: Option<SlotPermit> = self.current_tuner.as_ref().and_then(|old| {
            if old.key.tuner_path != tuner_path {
                return None;
            }
            let sub_count = old.subscriber_count();
            let will_free = (sub_count == 1 && self.ts_receiver.is_some())
                || (sub_count == 0 && self.ts_receiver.is_none());
            if will_free {
                old.take_slot_permit()
            } else {
                None
            }
        });

        // Rejoining the reader we are already on? Then it must not be torn
        // down by the pre-switch cleanup below (see the same guard in
        // `handle_set_channel_space`).
        let will_rejoin_current = self
            .current_tuner
            .as_ref()
            .is_some_and(|cur| cur.key == key && !cur.needs_reader_start());

        if inherited_permit.is_some() && !will_rejoin_current {
            self.take_and_cleanup_current_tuner_for_switch(&tuner_path, "v1 cleanup:")
                .await;
        }

        let own_key_will_free_slot = inherited_permit.is_some();
        let warm = self.warm_tuner.take();
        let request = AcquireRequest {
            candidates: vec![key.clone()],
            priority: channel_priority_for_class,
            // P2b-3: v1 carries a real exclusive flag, so it gets the same
            // policy as v2 — an exclusive request wins a priority tie.
            exclusive: effective_exclusive,
            bondriver_version: 2,
            carried_permit: inherited_permit,
            warm,
            own_key: old_tuner_key.clone(),
            own_key_will_free_slot,
            client_host: self.addr.ip().to_string(),
        };

        let outcome = match acquire(&self.tuner_pool, &self.database, request).await {
            Ok(outcome) => outcome,
            Err(e) => {
                // `OpenCooldown` fires at the same cadence as the client's
                // own reconnect loop (up to ~13/s during the 2026-08 flood
                // that motivated tuner::open_backoff) — logging every one at
                // ERROR here would just move the log flood from acquire.rs
                // to session.rs. acquire.rs already logs the real cause once
                // per minute per driver; this is only a symptom of that.
                if matches!(
                    e,
                    AcquireError::OpenCooldown { .. } | AcquireError::Warming { .. }
                ) {
                    debug!(
                        "[Session {}] SetChannel: failed to acquire a tuner: {}",
                        self.id, e
                    );
                } else {
                    error!(
                        "[Session {}] SetChannel: failed to acquire a tuner: {}",
                        self.id, e
                    );
                }
                return self.fail_set_channel(&old_tuner_key).await;
            }
        };

        self.absorb_acquire_leftovers(outcome.unused_permit.is_some(), outcome.unused_warm);
        if let Some(permit) = outcome.unused_permit {
            self.return_unused_permit(permit).await;
        }

        self.tuner_pool.cancel_idle_close(&outcome.key).await;

        let cleanup_old = handoff_current_tuner(
            self.id,
            &mut self.ts_receiver,
            &mut self.current_tuner,
            outcome.tuner.clone(),
            self.tuner_claim_priority,
            self.tuner_claim_exclusive,
            crate::tuner::shared::TunerUsage::from(self.stream_class),
            self.state == SessionState::Streaming,
            "SetChannel:",
        )
        .await;
        if let Some(old) = cleanup_old {
            cleanup_unused_tuner_after_switch(
                &self.database,
                &self.tuner_pool,
                self.id,
                old,
                Some(outcome.key.tuner_path.as_str()),
                false,
                "SetChannel cleanup:",
            )
            .await;
        }

        self.finish_set_channel_success(&outcome.tuner).await
    }

    async fn take_and_cleanup_current_tuner_for_switch(
        &mut self,
        cleanup_path: &str,
        log_prefix: &str,
    ) {
        if let Some(tuner) = self.current_tuner.take() {
            if self.ts_receiver.take().is_some() {
                debug!(
                    "[Session {}] {} unsubscribed from old tuner, remaining subscribers: {}",
                    self.id,
                    log_prefix,
                    tuner.subscriber_count()
                );
            }

            if tuner.subscriber_count() == 0 {
                // SetChannelSpace / SetChannel(v1): a same-DLL switch on a
                // multi-instance DLL is allowed to idle-close instead of a
                // synchronous stop, so other subscribers on that DLL keep
                // streaming and the slot can be warm-reused. Only actual
                // capacity pressure forces a synchronous stop here.
                cleanup_unused_tuner_after_switch(
                    &self.database,
                    &self.tuner_pool,
                    self.id,
                    tuner,
                    Some(cleanup_path),
                    false,
                    log_prefix,
                )
                .await;
            }
        }
    }

    async fn finalize_tuner_switch(&mut self, tuner: &Arc<SharedTuner>) {
        tuner.notify_channel_change();
        // Re-point the reader-state watch at whatever tuner we just settled
        // on, so the run loop notices *this* reader dying (P4). Every
        // successful selection funnels through here.
        self.reader_state_rx = self.current_tuner.as_ref().map(|t| t.subscribe_state());
        self.ts_quality_analyzer.reset();
        self.restart_tsreplace_pipeline_if_streaming().await;
    }

    async fn fail_open_tuner(&mut self, error_code: ErrorCode) -> std::io::Result<()> {
        self.send_message(ServerMessage::OpenTunerAck {
            success: false,
            error_code: error_code.into(),
            bondriver_version: 0,
        })
        .await
    }

    async fn finish_set_channel_success(
        &mut self,
        tuner: &Arc<SharedTuner>,
    ) -> std::io::Result<()> {
        self.finalize_tuner_switch(tuner).await;
        self.send_message(ServerMessage::SetChannelAck {
            success: true,
            error_code: 0,
        })
        .await
    }

    async fn fail_set_channel(
        &mut self,
        old_tuner_key: &Option<ChannelKey>,
    ) -> std::io::Result<()> {
        self.try_restore_previous_channel(old_tuner_key).await;
        self.send_message(ServerMessage::SetChannelAck {
            success: false,
            error_code: ErrorCode::ChannelSetFailed.into(),
        })
        .await
    }

    async fn fail_set_channel_space(
        &mut self,
        old_tuner_key: &Option<ChannelKey>,
    ) -> std::io::Result<()> {
        self.try_restore_previous_channel(old_tuner_key).await;
        self.send_message(ServerMessage::SetChannelSpaceAck {
            success: false,
            error_code: ErrorCode::ChannelSetFailed.into(),
        })
        .await
    }

    async fn fail_logical_channel_selection(&mut self) -> std::io::Result<()> {
        self.send_message(ServerMessage::SelectLogicalChannelAck {
            success: false,
            error_code: ErrorCode::ChannelSetFailed.into(),
            tuner_id: None,
            space: None,
            channel: None,
        })
        .await
    }

    async fn finish_set_channel_space_success(
        &mut self,
        tuner: &Arc<SharedTuner>,
        tuner_path: &str,
        actual_space: u32,
        actual_bon_channel: u32,
    ) -> std::io::Result<()> {
        self.finalize_tuner_switch(tuner).await;
        self.apply_channel_metadata(tuner_path, actual_space, actual_bon_channel)
            .await;
        info!(
            "[Session {}] Successfully set channel, sending SetChannelSpaceAck success=true",
            self.id
        );
        self.send_message(ServerMessage::SetChannelSpaceAck {
            success: true,
            error_code: 0,
        })
        .await
    }

    async fn finish_logical_channel_selection_success(
        &mut self,
        tuner: &Arc<SharedTuner>,
        candidate_idx: usize,
        tuner_id: &str,
        space: u32,
        channel: u32,
    ) -> std::io::Result<()> {
        self.finalize_tuner_switch(tuner).await;
        if self.state == SessionState::Ready {
            self.state = SessionState::TunerOpen;
        }
        info!(
            "[Session {}] Logical channel selected (candidate {}): tuner={}, space={}, channel={}",
            self.id, candidate_idx, tuner_id, space, channel
        );
        self.set_selected_tuner_path_and_registry(tuner_id).await;
        self.apply_channel_metadata(tuner_id, space, channel).await;
        self.send_message(ServerMessage::SelectLogicalChannelAck {
            success: true,
            error_code: 0,
            tuner_id: Some(tuner_id.to_string()),
            space: Some(space),
            channel: Some(channel),
        })
        .await
    }

    /// Common post-selection bookkeeping shared by every successful
    /// `SetChannelSpace` path (direct start, existing-tuner reuse, and the
    /// three capacity/priority fallback branches). Looks the channel up by
    /// its physical `(path, space, channel)`, updates the session registry
    /// (channel info/name/NID+SID), applies the single-service filter, and
    /// records `current_channel_*`. Does NOT send the Ack — each caller keeps
    /// control of its own reply/return so the surrounding control flow stays
    /// explicit. Extracted to kill five near-identical copies (see
    /// docs/SYSTEM_REVIEW_2026-07.md H2).
    async fn apply_channel_metadata(
        &mut self,
        path: &str,
        actual_space: u32,
        actual_bon_channel: u32,
    ) {
        // A successful channel change means an entirely new stream/lineup: the
        // old PIDs and their CC baselines are gone. Fully reset the analyzer so
        // the first packets of the new lineup re-baseline instead of each
        // counting a spurious drop. This helper is the single shared hook that
        // every successful SetChannelSpace path funnels through, so it fires
        // exactly once per switch.
        self.ts_quality_analyzer.reset();

        let channel_info = format!("Space {}, Ch {}", actual_space, actual_bon_channel);
        self.session_registry
            .update_channel(self.id, Some(channel_info.clone()))
            .await;
        self.current_channel_info = Some(channel_info);

        let (channel_name, ch_nid, ch_tsid, ch_sid) = {
            let db = self.database.lock().await;
            match db.get_channel_by_physical(path, actual_space, actual_bon_channel) {
                Ok(Some(rec)) => (
                    rec.channel_name.or(rec.raw_name),
                    Some(rec.nid),
                    Some(rec.tsid),
                    Some(rec.sid),
                ),
                _ => (None, None, None, None),
            }
        };
        self.session_registry
            .update_channel_name(self.id, channel_name.clone())
            .await;
        self.session_registry
            .update_channel_ids(self.id, ch_nid, ch_sid)
            .await;
        self.update_service_filter_for_sid(ch_nid, ch_tsid, ch_sid)
            .await;
        self.current_channel_name = channel_name;
    }

    /// Handle SetChannelSpace message (IBonDriver v2 style).
    async fn handle_set_channel_space(
        &mut self,
        space: u32,
        channel: u32,
        priority: i32,
        exclusive: bool,
    ) -> std::io::Result<()> {
        info!("[Session {}] HandleSetChannelSpace called: space={}, channel={}, priority={}, exclusive={}", 
              self.id, space, channel, priority, exclusive);

        self.session_registry
            .update_client_controls(self.id, Some(priority), Some(exclusive))
            .await;
        let (effective_priority, effective_exclusive) = self
            .session_registry
            .get_effective_controls(self.id)
            .await
            .unwrap_or((Some(priority), exclusive));
        // Preliminary claim from the wire/dashboard controls. It is refined
        // once below (DB default when no explicit priority) and then never
        // recomputed: acquire, the incumbent subscription and any later
        // handoff all read `tuner_claim_{priority,exclusive}`.
        let requested_priority = effective_priority.unwrap_or(priority);
        let requested_exclusive = effective_exclusive;
        self.tuner_claim_priority = requested_priority;
        self.tuner_claim_exclusive = requested_exclusive;

        if self.state != SessionState::TunerOpen && self.state != SessionState::Streaming {
            error!(
                "[Session {}] SetChannelSpace: Tuner not open (state: {:?})",
                self.id, self.state
            );
            return self
                .send_error(ErrorCode::InvalidState, "Tuner not open")
                .await;
        }

        // ★space は「仮想 space_idx」なので、実 space に変換する
        let Some((actual_space, region_name)) =
            self.map_space_idx_to_actual_with_region(space).await
        else {
            error!(
                "[Session {}] SetChannelSpace: Failed to map space_idx {} to actual space",
                self.id, space
            );
            return self
                .send_message(ServerMessage::SetChannelSpaceAck {
                    success: false,
                    error_code: ErrorCode::InvalidParameter.into(),
                })
                .await;
        };

        // Get region-filtered channel map
        let map = self
            .ensure_channel_map_with_region(actual_space, &region_name)
            .await;
        debug!("[Session {}] SetChannelSpace: Checking channel map for space {} (region: {}): {} channels total", 
               self.id, actual_space, region_name, map.len());

        let Some(entry) = map.get(channel as usize) else {
            error!("[Session {}] SetChannelSpace: Channel index {} not found in space {} region {} (map size: {})", 
                   self.id, channel, actual_space, region_name, map.len());
            return self
                .send_message(ServerMessage::SetChannelSpaceAck {
                    success: false,
                    error_code: ErrorCode::InvalidParameter.into(),
                })
                .await;
        };

        // ★ In group mode, find which driver has this channel (matching by NID+TSID)
        // NID+TSID matching allows different BonDrivers to use different bon_channel values
        // for the same logical channel (same NID+TSID).
        //
        // Candidate discovery only. The winner is chosen once, by
        // `tuner::policy::decide` inside `acquire`, from this complete list —
        // there is no longer any caller-side preselection
        // (docs/TUNER_PIPELINE_REDESIGN.md §2.2-14).
        let group_candidates: Vec<(String, u32, u32)> = if self.group_driver_paths.is_empty() {
            Vec::new()
        } else {
            collect_group_channel_candidates(
                &self.database,
                self.id,
                &self.group_driver_paths,
                entry.nid,
                entry.tsid,
            )
            .await
        };

        // ★ Capture the current session's tuner key BEFORE driver selection.
        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());

        let (tuner_path, actual_space, actual_bon_channel) = if !self.group_driver_paths.is_empty()
        {
            let Some((path, driver_space, driver_bon_channel)) = group_candidates.first().cloned()
            else {
                error!(
                    "[Session {}] SetChannelSpace: Channel NID=0x{:04X} TSID=0x{:04X} not found in any group driver",
                    self.id, entry.nid, entry.tsid
                );
                return self
                    .send_message(ServerMessage::SetChannelSpaceAck {
                        success: false,
                        error_code: ErrorCode::InvalidParameter.into(),
                    })
                    .await;
            };
            // This is only a representative used for DB-default priority and
            // same-DLL permit handoff. The actual winner is chosen once, by
            // acquire()/policy, from the complete candidate set below.
            (path, driver_space, driver_bon_channel)
        } else {
            match &self.current_tuner_path {
                Some(p) => (p.clone(), entry.bon_space, entry.bon_channel),
                None => {
                    error!(
                        "[Session {}] SetChannelSpace: current_tuner_path is None",
                        self.id
                    );
                    return self
                        .send_message(ServerMessage::SetChannelSpaceAck {
                            success: false,
                            error_code: ErrorCode::InvalidState.into(),
                        })
                        .await;
                }
            }
        };

        info!(
            "[Session {}] SetChannelSpace: space_idx={}, actual_space={}, idx={} -> bon_channel={} (NID=0x{:04X} TSID=0x{:04X}) on {} (priority={}, exclusive={})",
            self.id, space, actual_space, channel, actual_bon_channel, entry.nid, entry.tsid, tuner_path, priority, exclusive
        );

        // docs/TUNER_PIPELINE_REDESIGN.md P1b §4: "自分のスロットは自分のもの"
        // — if this session is the sole subscriber of its current tuner and
        // the newly selected target is on the *same* DLL, take that tuner's
        // slot permit directly rather than letting it be released (via
        // whatever `take_and_cleanup_current_tuner_for_switch` below decides
        // — sync stop or idle-close) and separately trying to `acquire_slot`
        // a new one for the target. On a `max_instances=1` driver, releasing
        // first and re-acquiring second would race this session against any
        // other task that happens to ask for that same DLL in between; this
        // permit handoff makes the switch atomic from the driver's capacity
        // point of view instead. This replaces the old `old_tuner_will_free_slot`
        // boolean, which only ever adjusted a diagnostic instance *count* —
        // it never actually reserved anything, so the race it was meant to
        // paper over was still there.
        //
        // Must run before any of the reuse/eviction/cleanup calls below that
        // can `take()` or stop `self.current_tuner` — extracting the permit
        // here (rather than after) is what makes it safe regardless of which
        // branch this request ends up taking; if this request turns out to
        // reuse an existing tuner instead of creating one, the extracted
        // permit is simply dropped (released back) when this function
        // returns without ever being consumed.
        let inherited_permit: Option<SlotPermit> = self.current_tuner.as_ref().and_then(|old| {
            if old.key.tuner_path != tuner_path {
                return None;
            }
            let sub_count = old.subscriber_count();
            let will_free =
                // Streaming: sole broadcast subscriber → slot freed after unsubscribe
                (sub_count == 1 && self.ts_receiver.is_some()) ||
                // TunerOpen: no broadcast subscription yet → slot freed immediately
                (sub_count == 0 && self.ts_receiver.is_none());
            if will_free {
                old.take_slot_permit()
            } else {
                None
            }
        });

        // Use explicit client priority, otherwise the DB default. Exclusive
        // remains a separate tie-breaker and must never be encoded into the
        // numeric priority. Store exactly the rank handed to acquire so the
        // incumbent subscription cannot later appear weaker than its request.
        let channel_priority = if requested_priority > 0 {
            requested_priority
        } else {
            let db = self.database.lock().await;
            db.get_channel_priority(&tuner_path, actual_space, actual_bon_channel)
                .unwrap_or(Some(0))
                .unwrap_or(0)
        };
        self.tuner_claim_priority = channel_priority;
        self.tuner_claim_exclusive = requested_exclusive;

        // STREAMING_DESIGN.md §2: high-priority (recording-grade) selection
        // auto-promotes this session to RECORD, regardless of what the
        // client declared in Hello.
        self.maybe_promote_stream_class(channel_priority).await;

        // Candidate list handed to `acquire`: the driver selected above
        // first, then its group siblings carrying the same (NID, TSID).
        // `decide` re-sorts them, so this is a set, not a ranking — the
        // siblings are what used to be the separate `fallback_candidates`
        // chain (`try_fallback_drivers`).
        let primary_key = ChannelKey::space_channel(&tuner_path, actual_space, actual_bon_channel);
        let mut candidates: Vec<ChannelKey> = vec![primary_key.clone()];
        for (path, space, ch) in group_candidates.iter() {
            let key = ChannelKey::space_channel(path, *space, *ch);
            if !candidates.contains(&key) {
                candidates.push(key);
            }
        }

        // Will `acquire` join a reader this session is already on? If so the
        // old tuner must survive untouched — stopping it here (the
        // "free the DLL slot before opening" step below) would tear down the
        // very reader the request is about to reuse. This reproduces the old
        // ordering, where `try_reuse_existing_set_channel_space_tuner` ran
        // *before* the cleanup and returned early on a hit.
        let will_rejoin_current = match self.current_tuner.as_ref() {
            Some(cur) => candidates.contains(&cur.key) && !cur.needs_reader_start(),
            None => false,
        };

        // Free the DLL slot before opening, but only when this request would
        // otherwise open a second reader on a device the old one still holds:
        // same DLL, and this session is its sole subscriber (exactly the
        // condition that produced `inherited_permit`). Single-open character
        // devices (px4 family) return EALREADY otherwise. When the target is
        // a different DLL, the old tuner is cleaned up *after* the switch
        // instead (below), so a failed switch can still fall back to it.
        if inherited_permit.is_some() && !will_rejoin_current {
            self.take_and_cleanup_current_tuner_for_switch(&tuner_path, "channel switch cleanup:")
                .await;
        }

        let own_key_will_free_slot = inherited_permit.is_some();
        let warm = self.warm_tuner.take();
        let request = AcquireRequest {
            candidates,
            priority: channel_priority,
            exclusive: requested_exclusive,
            bondriver_version: 2,
            carried_permit: inherited_permit,
            warm,
            own_key: old_tuner_key.clone(),
            own_key_will_free_slot,
            client_host: self.addr.ip().to_string(),
        };

        let outcome = match acquire(&self.tuner_pool, &self.database, request).await {
            Ok(outcome) => outcome,
            Err(e) => {
                // See the matching comment in SetChannel above: OpenCooldown
                // is a symptom of a reconnect loop, not a new fact worth an
                // ERROR line here — acquire.rs already logged the underlying
                // failure, rate-limited, once per minute per driver.
                if matches!(
                    e,
                    AcquireError::OpenCooldown { .. } | AcquireError::Warming { .. }
                ) {
                    debug!(
                        "[Session {}] SetChannelSpace: failed to acquire a tuner: {}",
                        self.id, e
                    );
                } else {
                    error!(
                        "[Session {}] SetChannelSpace: failed to acquire a tuner: {}",
                        self.id, e
                    );
                }
                return self.fail_set_channel_space(&old_tuner_key).await;
            }
        };

        self.absorb_acquire_leftovers(outcome.unused_permit.is_some(), outcome.unused_warm);
        if let Some(permit) = outcome.unused_permit {
            self.return_unused_permit(permit).await;
        }

        let (chosen_space, chosen_channel) = match outcome.key.channel {
            ChannelKeySpec::SpaceChannel { space, channel } => (space, channel),
            ChannelKeySpec::Simple(c) => (0, c as u32),
        };
        let chosen_path = outcome.key.tuner_path.clone();

        self.tuner_pool.cancel_idle_close(&outcome.key).await;
        self.set_selected_tuner_path_and_registry(&chosen_path)
            .await;

        let cleanup_old = handoff_current_tuner(
            self.id,
            &mut self.ts_receiver,
            &mut self.current_tuner,
            outcome.tuner.clone(),
            self.tuner_claim_priority,
            self.tuner_claim_exclusive,
            crate::tuner::shared::TunerUsage::from(self.stream_class),
            self.state == SessionState::Streaming,
            "SetChannelSpace:",
        )
        .await;
        if let Some(old) = cleanup_old {
            // `force_stop_same_dll = false`: a multi-instance DLL may keep the
            // old reader warm for a zapping-back client (DESIGN.md §4.3),
            // unlike SelectLogicalChannel's group members.
            cleanup_unused_tuner_after_switch(
                &self.database,
                &self.tuner_pool,
                self.id,
                old,
                Some(chosen_path.as_str()),
                false,
                "SetChannelSpace cleanup:",
            )
            .await;
        }

        self.finish_set_channel_space_success(
            &outcome.tuner,
            &chosen_path,
            chosen_space,
            chosen_channel,
        )
        .await
    }

    /// Put back the warm handle `acquire` did not consume, and forget the
    /// stored path when it did.
    fn absorb_acquire_leftovers(
        &mut self,
        _had_permit: bool,
        unused_warm: Option<WarmTunerHandle>,
    ) {
        match unused_warm {
            Some(warm) => {
                self.warm_tuner_path = Some(warm.path().to_string());
                self.warm_tuner = Some(warm);
            }
            None => {
                self.warm_tuner = None;
                self.warm_tuner_path = None;
            }
        }
    }

    /// Hand an unconsumed slot permit back to the tuner it came from, or
    /// release it.
    ///
    /// `acquire` returns the `carried_permit` untouched when it settled on a
    /// different DLL or on a reuse. The old reader is still running in that
    /// case, so simply dropping the permit would let the pool believe that
    /// slot is free while the device is still open.
    async fn return_unused_permit(&mut self, permit: SlotPermit) {
        if let Some(cur) = self.current_tuner.as_ref() {
            if cur.key.tuner_path == permit.dll_path() && cur.occupies_slot() {
                cur.set_slot_permit(permit);
                return;
            }
        }
        // Nothing left to attribute it to: dropping releases it.
        drop(permit);
    }

    async fn handle_get_signal_level(&mut self) -> std::io::Result<()> {
        let signal_level = self
            .current_tuner
            .as_ref()
            .map(|t| t.signal_level())
            .unwrap_or(0.0);

        self.send_message(ServerMessage::GetSignalLevelAck { signal_level })
            .await
    }

    /// Handle EnumTuningSpace message.
    async fn handle_enum_tuning_space(&mut self, space: u32) -> std::io::Result<()> {
        debug!("[Session {}] EnumTuningSpace: space_idx={}", self.id, space);

        // Get space list with names
        let space_list = self.get_space_list_with_names().await;

        if space >= space_list.len() as u32 {
            // No more spaces, end enumeration
            return self
                .send_message(ServerMessage::EnumTuningSpaceAck { name: None })
                .await;
        }

        let (actual_space, name, _region_key) = &space_list[space as usize];

        debug!(
            "[Session {}] EnumTuningSpace: space_idx={} actual_space={} name={:?}",
            self.id, space, actual_space, name
        );

        self.send_message(ServerMessage::EnumTuningSpaceAck {
            name: Some(name.clone()),
        })
        .await
    }

    /// Handle EnumChannelName message.
    async fn handle_enum_channel_name(&mut self, space: u32, channel: u32) -> std::io::Result<()> {
        debug!(
            "[Session {}] EnumChannelName: space={}, channel={}",
            self.id, space, channel
        );

        let Some((actual_space, region_name)) =
            self.map_space_idx_to_actual_with_region(space).await
        else {
            return self
                .send_message(ServerMessage::EnumChannelNameAck { name: None })
                .await;
        };

        let map = self
            .ensure_channel_map_with_region(actual_space, &region_name)
            .await;
        let name = map.get(channel as usize).map(|e| e.name.clone());

        debug!("[Session {}] EnumChannelName: space_idx={} actual_space={} region={} channel={} name={:?}",
            self.id, space, actual_space, region_name, channel, name);

        self.send_message(ServerMessage::EnumChannelNameAck { name })
            .await
    }

    /// Handle StartStream message.
    async fn handle_start_stream(&mut self) -> std::io::Result<()> {
        if self.state != SessionState::TunerOpen {
            return self
                .send_error(ErrorCode::InvalidState, "Tuner not open")
                .await;
        }

        let tuner = match &self.current_tuner {
            Some(t) => t.clone(),
            None => {
                return self
                    .send_message(ServerMessage::StartStreamAck {
                        success: false,
                        error_code: ErrorCode::InvalidState.into(),
                    })
                    .await;
            }
        };

        info!("[Session {}] Starting stream", self.id);

        // ★ Cancel idle-close BEFORE subscribing.
        // If the idle-close timer fires between cancel and subscribe, the task will see
        // has_subscribers()==0 and might stop the reader.  Canceling first minimises
        // that window; the has_subscribers() double-check inside the idle-close task
        // (Bug F fix) provides the final backstop.
        self.tuner_pool.cancel_idle_close(&tuner.key).await;

        // Subscribe to the tuner's broadcast channel
        let rx = tuner.subscribe_with_claim_class(
            self.tuner_claim_priority,
            self.tuner_claim_exclusive,
            crate::tuner::shared::TunerUsage::from(self.stream_class),
        );
        self.ts_receiver = Some(rx);
        self.state = SessionState::Streaming;

        if let Err(e) = self.start_tsreplace_pipeline().await {
            if self.tsreplace_passthrough_on_error {
                warn!(
                    "[Session {}] tsreplace unavailable, fallback to raw TS: {}",
                    self.id, e
                );
                self.stop_tsreplace_pipeline().await;
            } else {
                self.ts_receiver = None;
                self.state = SessionState::TunerOpen;
                return self
                    .send_message(ServerMessage::StartStreamAck {
                        success: false,
                        error_code: ErrorCode::TunerOpenFailed.into(),
                    })
                    .await;
            }
        }

        // STREAMING_DESIGN.md §4.3: start the prefill/jitter buffer now that
        // streaming has actually begun.
        self.reset_prefill_buffer().await;

        // Update session registry
        self.session_registry.update_streaming(self.id, true).await;

        self.send_message(ServerMessage::StartStreamAck {
            success: true,
            error_code: 0,
        })
        .await
    }

    /// Handle StopStream message.
    async fn handle_stop_stream(&mut self) -> std::io::Result<()> {
        info!("[Session {}] Stopping stream", self.id);

        // Release the tracked subscription — only if we actually have an
        // active one. `TunerSubscription`'s `Drop` performs the decrement
        // that used to be a manual, guarded `tuner.unsubscribe()` call (the
        // guard existed to avoid double-releasing on a redundant StopStream;
        // RAII makes that structurally impossible instead — there is simply
        // nothing to drop a second time).
        if let Some(sub) = self.ts_receiver.take() {
            drop(sub);
            if let Some(tuner) = &self.current_tuner {
                // ★ Check if this was the last subscriber
                // If so, automatically stop the reader
                if tuner.subscriber_count() == 0 {
                    info!("[Session {}] No more subscribers after StopStream, scheduling keep-alive close for {:?}", self.id, tuner.key);
                    self.tuner_pool
                        .schedule_idle_close(tuner.key.clone(), Arc::clone(tuner))
                        .await;
                }
            }
        }
        self.stop_tsreplace_pipeline().await;
        self.state = SessionState::TunerOpen;

        // STREAMING_DESIGN.md §4.3: discard the prefill/jitter buffer state.
        // `StartStream` performs its own `reset()` when streaming resumes, so
        // it does not matter whether this leaves `PrefillBuffer` filling or
        // passthrough — only the queued frames need to go.
        self.prefill_buffer.clear();

        // Update session registry
        self.session_registry.update_streaming(self.id, false).await;

        self.send_message(ServerMessage::StopStreamAck { success: true })
            .await
    }

    /// Handle PurgeStream message.
    async fn handle_purge_stream(&mut self) -> std::io::Result<()> {
        debug!("[Session {}] Purging stream buffer", self.id);

        // Drain the receiver
        if let Some(rx) = &mut self.ts_receiver {
            while rx.try_recv().is_ok() {}
        }

        // STREAMING_DESIGN.md §4.3: also drop any frames queued in the
        // prefill/jitter buffer without changing its filling/passthrough state.
        self.prefill_buffer.clear();

        self.send_message(ServerMessage::PurgeStreamAck { success: true })
            .await
    }

    /// Handle SetLnbPower message.
    async fn handle_set_lnb_power(&mut self, enable: bool) -> std::io::Result<()> {
        info!("[Session {}] SetLnbPower: {}", self.id, enable);

        // TODO: Implement actual LNB power control
        self.send_message(ServerMessage::SetLnbPowerAck {
            success: true,
            error_code: 0,
        })
        .await
    }

    /// Handle SetServiceFilter message.
    async fn handle_set_service_filter(&mut self, single_service: bool) -> std::io::Result<()> {
        info!(
            "[Session {}] SetServiceFilter: single_service={}",
            self.id, single_service
        );
        self.single_service_filter_enabled = single_service;
        if !single_service {
            // Disable filtering
            self.ts_service_filter = None;
        }
        self.send_message(ServerMessage::SetServiceFilterAck { success: true })
            .await
    }

    /// Update the per-session TS service filter based on the resolved SID.
    ///
    /// Called after channel selection resolves the target SID from the database.
    /// If single-service filtering is enabled, creates or updates the filter;
    /// otherwise this is a no-op for the filter.
    /// Always updates current NID/TSID/SID tracking for tsreplace SID injection.
    ///
    /// Also the STREAMING_DESIGN.md §4.3 hook for "channel switch completed
    /// while streaming": every caller reaches this function exactly when the
    /// session's notion of "current channel" changes, so it resets the
    /// prefill/jitter buffer for the new channel's band-based bitrate
    /// default (no-op if not currently `Streaming` — `StartStream` performs
    /// its own reset when the stream actually begins).
    async fn update_service_filter_for_sid(
        &mut self,
        nid: Option<u16>,
        tsid: Option<u16>,
        sid: Option<u16>,
    ) {
        // Always update NID/TSID/SID tracking (used by tsreplace pipeline)
        self.current_nid = nid;
        self.current_tsid = tsid;
        self.current_sid = sid;

        if self.state == SessionState::Streaming {
            self.reset_prefill_buffer().await;
        }

        if !self.single_service_filter_enabled {
            return;
        }

        match sid {
            Some(sid_val) => {
                if let Some(ref mut filter) = self.ts_service_filter {
                    if filter.target_sid() != sid_val {
                        debug!(
                            "[Session {}] Service filter: SID changed 0x{:04X} -> 0x{:04X}",
                            self.id,
                            filter.target_sid(),
                            sid_val
                        );
                        filter.set_target_sid(sid_val);
                    } else {
                        // Same SID but channel re-selected, reset to re-acquire PAT/PMT
                        filter.reset();
                    }
                } else {
                    debug!(
                        "[Session {}] Service filter: creating filter for SID 0x{:04X}",
                        self.id, sid_val
                    );
                    self.ts_service_filter = Some(TsServiceFilter::new(sid_val));
                }
            }
            None => {
                warn!(
                    "[Session {}] Service filter: SID not found in DB, disabling filter for this channel",
                    self.id
                );
                self.ts_service_filter = None;
            }
        }
    }

    /// Handle SelectLogicalChannel message.
    ///
    /// One logical request is one acquire transaction. Every physical route
    /// for the requested (NID, TSID) is presented together so policy can pick
    /// a free/healthy sibling before it ever considers eviction.
    async fn handle_select_logical_channel(
        &mut self,
        nid: u16,
        tsid: u16,
        sid: Option<u16>,
    ) -> std::io::Result<()> {
        if self.state != SessionState::Ready
            && self.state != SessionState::TunerOpen
            && self.state != SessionState::Streaming
        {
            return self
                .send_error(ErrorCode::InvalidState, "Not in ready state")
                .await;
        }

        info!(
            "[Session {}] SelectLogicalChannel: nid={}, tsid={}, sid={:?}",
            self.id, nid, tsid, sid
        );

        let channels = {
            let db = self.database.lock().await;
            match db.get_channels_by_nid_tsid_ordered(nid, tsid, sid) {
                Ok(chs) => chs,
                Err(e) => {
                    drop(db);
                    error!("[Session {}] Failed to query channels: {}", self.id, e);
                    return self.fail_logical_channel_selection().await;
                }
            }
        };
        if channels.is_empty() {
            return self.fail_logical_channel_selection().await;
        }

        let mut candidates = Vec::new();
        for channel_with_driver in &channels {
            let record = &channel_with_driver.channel;
            let key = ChannelKey::space_channel(
                &channel_with_driver.bon_driver_path,
                record.bon_space.unwrap_or(0),
                record.bon_channel.unwrap_or(0),
            );
            if !candidates.contains(&key) {
                candidates.push(key);
            }
        }
        if candidates.is_empty() {
            return self.fail_logical_channel_selection().await;
        }

        // SelectLogicalChannel has no wire-level priority/exclusive fields, so
        // its canonical claim is the configured DB priority of the logical
        // channel and non-exclusive. This same claim is used by arbitration
        // and by the subscription installed after handoff.
        let logical_priority = channels.first().map(|c| c.channel.priority).unwrap_or(0);
        self.tuner_claim_priority = logical_priority;
        self.tuner_claim_exclusive = false;
        self.maybe_promote_stream_class(logical_priority).await;

        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());
        let old_tuner_for_permit = self.current_tuner.clone();
        let carried_permit: Option<SlotPermit> = old_tuner_for_permit.as_ref().and_then(|old| {
            let sub_count = old.subscriber_count();
            let will_free = (sub_count == 1 && self.ts_receiver.is_some())
                || (sub_count == 0 && self.ts_receiver.is_none());
            if will_free {
                old.take_slot_permit()
            } else {
                None
            }
        });
        let own_key_will_free_slot = carried_permit.is_some();
        let warm = self.warm_tuner.take();

        let request = AcquireRequest {
            candidates,
            priority: logical_priority,
            exclusive: false,
            bondriver_version: 2,
            carried_permit,
            warm,
            own_key: old_tuner_key.clone(),
            own_key_will_free_slot,
            client_host: self.addr.ip().to_string(),
        };

        let outcome = match acquire(&self.tuner_pool, &self.database, request).await {
            Ok(outcome) => outcome,
            Err(e) => {
                if let Some(old) = old_tuner_for_permit.as_ref() {
                    debug!(
                        "[Session {}] SelectLogicalChannel: acquire failed with previous tuner {:?}: {}",
                        self.id, old.key, e
                    );
                } else {
                    debug!(
                        "[Session {}] SelectLogicalChannel: acquire failed: {}",
                        self.id, e
                    );
                }
                self.try_restore_previous_channel(&old_tuner_key).await;
                return self.fail_logical_channel_selection().await;
            }
        };

        self.absorb_acquire_leftovers(outcome.unused_permit.is_some(), outcome.unused_warm);
        if let Some(permit) = outcome.unused_permit {
            self.return_unused_permit(permit).await;
        }

        let chosen_path = outcome.key.tuner_path.clone();
        let (chosen_space, chosen_channel) = match outcome.key.channel {
            ChannelKeySpec::SpaceChannel { space, channel } => (space, channel),
            ChannelKeySpec::Simple(c) => (0, c as u32),
        };
        let candidate_idx = channels
            .iter()
            .position(|c| {
                c.bon_driver_path == chosen_path
                    && c.channel.bon_space.unwrap_or(0) == chosen_space
                    && c.channel.bon_channel.unwrap_or(0) == chosen_channel
            })
            .unwrap_or(0);

        self.tuner_pool.cancel_idle_close(&outcome.key).await;
        let cleanup_old = handoff_current_tuner(
            self.id,
            &mut self.ts_receiver,
            &mut self.current_tuner,
            outcome.tuner.clone(),
            self.tuner_claim_priority,
            self.tuner_claim_exclusive,
            crate::tuner::shared::TunerUsage::from(self.stream_class),
            self.state == SessionState::Streaming,
            "SelectLogicalChannel:",
        )
        .await;
        if let Some(old) = cleanup_old {
            cleanup_unused_tuner_after_switch(
                &self.database,
                &self.tuner_pool,
                self.id,
                old,
                Some(chosen_path.as_str()),
                true,
                "SelectLogicalChannel cleanup:",
            )
            .await;
        }

        self.finish_logical_channel_selection_success(
            &outcome.tuner,
            candidate_idx,
            &chosen_path,
            chosen_space,
            chosen_channel,
        )
        .await
    }

    /// Handle GetChannelList message.
    async fn handle_get_channel_list(
        &mut self,
        filter: Option<recisdb_protocol::ChannelFilter>,
    ) -> std::io::Result<()> {
        info!("[Session {}] GetChannelList: filter={:?}", self.id, filter);

        // Query channels from database
        let all_channels = {
            let db = self.database.lock().await;
            match db.get_all_channels_with_drivers() {
                Ok(chs) => chs,
                Err(e) => {
                    drop(db);
                    error!("[Session {}] Failed to query channels: {}", self.id, e);
                    return self
                        .send_message(ServerMessage::GetChannelListAck {
                            channels: vec![],
                            timestamp: chrono::Utc::now().timestamp(),
                        })
                        .await;
                }
            }
        };

        // Convert to ClientChannelInfo and apply filters
        let mut channels: Vec<ClientChannelInfo> = all_channels
            .into_iter()
            .filter(|(ch, _bd)| {
                if let Some(ref f) = filter {
                    // Filter by NID
                    if let Some(nid) = f.nid {
                        if ch.nid as u16 != nid {
                            return false;
                        }
                    }
                    // Filter by TSID
                    if let Some(tsid) = f.tsid {
                        if ch.tsid as u16 != tsid {
                            return false;
                        }
                    }
                    // Filter by enabled
                    if f.enabled_only && !ch.is_enabled {
                        return false;
                    }
                    // Broadcast type filter using NID classification
                    if let Some(bt) = f.broadcast_type {
                        let (classified_type, _region) = classify_nid(ch.nid as u16);
                        if classified_type != bt {
                            return false;
                        }
                    }
                }
                true
            })
            .map(|(ch, bd)| ClientChannelInfo {
                nid: ch.nid as u16,
                sid: ch.sid as u16,
                tsid: ch.tsid as u16,
                channel_name: ch.service_name.clone().unwrap_or_default(),
                network_name: ch.ts_name.clone(),
                service_type: ch.service_type.map(|s| s as u8).unwrap_or(0x01),
                // Wire format is u8; CS110 keys are the 3-digit channel
                // number (= SID) and don't fit — send None instead of a
                // truncated value.
                remote_control_key: ch.remote_control_key.and_then(|k| u8::try_from(k).ok()),
                space_name: bd.map(|b| b.dll_path.clone()).unwrap_or_default(),
                channel_display_name: ch.service_name.unwrap_or_default(),
                priority: ch.priority,
            })
            .collect();

        // Sort by priority (descending)
        channels.sort_by(|a, b| b.priority.cmp(&a.priority));

        let timestamp = chrono::Utc::now().timestamp();

        info!(
            "[Session {}] Returning {} channels",
            self.id,
            channels.len()
        );

        self.send_message(ServerMessage::GetChannelListAck {
            channels,
            timestamp,
        })
        .await
    }

    /// Send TS data to the client.
    ///
    /// Returns `Ok(true)` if the caller must disconnect the session (RECORD
    /// write-queue overflow, STREAMING_DESIGN.md §3.2/§12-1) — the run()
    /// select loop is expected to `break` in that case so `cleanup()` still
    /// runs and the reason is recorded in `session_history`.
    async fn send_ts_data(&mut self, data: Bytes) -> std::io::Result<bool> {
        // ---- 1) Align outgoing TS to 188-byte packets ----
        //
        // Fast path (P3 §2.2-9): a chunk that is already 188-aligned, starts
        // on a sync byte, and arrives with nothing left over from last time
        // needs no realignment at all — which is the steady state for a
        // healthy stream. Taking it avoids two full copies of every chunk
        // (into `ts_send_carry`, then back out of it) for every session.
        // Anything irregular falls through to the carry-buffer path below,
        // which is unchanged.
        if is_aligned_passthrough(self.ts_send_carry.is_empty(), &data) {
            return self.send_aligned_ts_data(data).await;
        }

        self.ts_send_carry.extend_from_slice(&data);

        // Best-effort resync if head is not sync byte (0x47)
        if !self.ts_send_carry.is_empty() && self.ts_send_carry[0] != 0x47 {
            let mut sync_pos: Option<usize> = None;
            for i in 0..self.ts_send_carry.len() {
                if self.ts_send_carry[i] != 0x47 {
                    continue;
                }

                let ok_188 =
                    i + 188 < self.ts_send_carry.len() && self.ts_send_carry[i + 188] == 0x47;
                let ok_376 =
                    i + 376 < self.ts_send_carry.len() && self.ts_send_carry[i + 376] == 0x47;
                if ok_188 || ok_376 {
                    sync_pos = Some(i);
                    break;
                }
            }

            if let Some(pos) = sync_pos {
                if pos > 0 {
                    self.ts_send_carry.drain(0..pos);
                }
            } else if self.ts_send_carry.len() > 188 * 4 {
                // Keep a small tail and wait for next chunk to find sync sequence.
                let keep = 188 * 4;
                let drop_len = self.ts_send_carry.len() - keep;
                self.ts_send_carry.drain(0..drop_len);
            }
        }

        let send_len = self.ts_send_carry.len() - (self.ts_send_carry.len() % 188);
        if send_len < 188 {
            // wait for enough bytes to form at least one TS packet
            return Ok(false);
        }

        let send_data = Bytes::copy_from_slice(&self.ts_send_carry[..send_len]);
        self.ts_send_carry.drain(0..send_len);

        self.send_aligned_ts_data(send_data).await
    }

    /// Everything `send_ts_data` does once the payload is known to be a whole
    /// number of 188-byte TS packets: service filtering, quality accounting,
    /// stats, and handing the frame to the writer.
    ///
    /// Split out so the aligned fast path and the carry-buffer path share it
    /// verbatim rather than duplicating ~120 lines of accounting.
    async fn send_aligned_ts_data(&mut self, send_data: Bytes) -> std::io::Result<bool> {
        // ---- 2) Apply single-service filter if enabled ----
        let send_data = if let Some(ref mut filter) = self.ts_service_filter {
            let filtered = filter.filter(&send_data);
            if filtered.is_empty() {
                return Ok(false);
            }
            Bytes::from(filtered)
        } else {
            send_data
        };

        self.ts_msgs_sent += 1;
        self.ts_bytes_sent += send_data.len() as u64;
        self.bytes_since_last += send_data.len() as u64;

        // Analyze TS quality for this session.
        // Encoder/pipe output chunks are not guaranteed to be aligned on 188-byte TS boundaries,
        // so we keep carry and resync by sync byte before feeding analyzer.
        self.ts_quality_carry.extend_from_slice(&send_data);

        // Best-effort resync if head is not sync byte (0x47)
        if !self.ts_quality_carry.is_empty() && self.ts_quality_carry[0] != 0x47 {
            let mut sync_pos: Option<usize> = None;
            for i in 0..self.ts_quality_carry.len() {
                if self.ts_quality_carry[i] != 0x47 {
                    continue;
                }

                let ok_188 =
                    i + 188 < self.ts_quality_carry.len() && self.ts_quality_carry[i + 188] == 0x47;
                let ok_376 =
                    i + 376 < self.ts_quality_carry.len() && self.ts_quality_carry[i + 376] == 0x47;
                if ok_188 || ok_376 {
                    sync_pos = Some(i);
                    break;
                }
            }

            if let Some(pos) = sync_pos {
                if pos > 0 {
                    self.ts_quality_carry.drain(0..pos);
                    // Bytes were discarded mid-stream: the CC baseline per PID
                    // no longer matches the next packet. Resync without
                    // counting the unavoidable CC jumps as drops (same
                    // rationale as the broadcast-Lagged handling).
                    self.ts_quality_analyzer.mark_discontinuity();
                }
            } else if self.ts_quality_carry.len() > 188 * 4 {
                // Keep a small tail and wait for next chunk to find sync sequence.
                let keep = 188 * 4;
                let drop_len = self.ts_quality_carry.len() - keep;
                self.ts_quality_carry.drain(0..drop_len);
                self.ts_quality_analyzer.mark_discontinuity();
            }
        }

        let mut delta = crate::tuner::ts_analyzer::TsStreamQualityDelta::default();
        let full_len = self.ts_quality_carry.len() - (self.ts_quality_carry.len() % 188);
        if full_len >= 188 {
            delta = self
                .ts_quality_analyzer
                .analyze(&self.ts_quality_carry[..full_len]);
            self.ts_quality_carry.drain(0..full_len);
        }

        self.packets_dropped += delta.packets_dropped;
        self.packets_scrambled += delta.packets_scrambled;
        self.packets_error += delta.packets_error;
        self.interval_packets_total += delta.packets_total;
        self.interval_packets_dropped += delta.packets_dropped;

        if self.last_ts_log.elapsed().as_secs_f32() >= 1.0 {
            // 視聴中のセッション1本につき毎秒1行。同じ数字はダッシュボードの
            // クライアント一覧(直下でsession registryへ反映している)から常に
            // 見えるので、ログは DEBUG で十分。INFO のままだと視聴が続く限り
            // 積み上がる(本番ログ2026-08-10: 79358行/日)。
            debug!(
                "[Session {}] TsData sending: msgs={} bytes={}",
                self.id, self.ts_msgs_sent, self.ts_bytes_sent
            );
            let elapsed = self.last_ts_log.elapsed().as_secs_f64().max(0.001);
            self.last_ts_log = std::time::Instant::now();

            // Update session registry with signal and packet stats
            if let Some(tuner) = &self.current_tuner {
                let signal_level = tuner.signal_level();
                // Use bytes sent to this client (not tuner's received packets)
                let packets_sent = self.ts_bytes_sent / 188; // TS packet size

                let bitrate_mbps = (self.bytes_since_last as f64 * 8.0) / 1_000_000.0 / elapsed;
                let packet_loss_rate = if self.interval_packets_total > 0 {
                    (self.interval_packets_dropped as f64 / self.interval_packets_total as f64)
                        * 100.0
                } else {
                    0.0
                };

                let top_loss_pids = self.ts_quality_analyzer.top_loss_pids(10);

                self.session_registry
                    .update_stats(
                        self.id,
                        signal_level,
                        packets_sent,
                        self.packets_dropped,
                        self.packets_scrambled,
                        self.packets_error,
                        bitrate_mbps,
                        self.loss_broadcast_lag_chunks,
                        self.loss_ts_queue_chunks,
                        self.loss_encoder_stall_events,
                        top_loss_pids,
                    )
                    .await;

                // STREAMING_DESIGN.md §4 P3: surface prefill/jitter buffer
                // status on the same 1-second cadence as the other stats.
                self.session_registry
                    .update_prefilling(self.id, self.prefill_buffer.is_filling())
                    .await;

                let timestamp_ms = chrono::Utc::now().timestamp_millis();
                self.session_registry
                    .push_metrics_sample(
                        self.id,
                        timestamp_ms,
                        bitrate_mbps,
                        packet_loss_rate,
                        signal_level,
                    )
                    .await;

                self.signal_samples += 1;
                self.signal_level_sum += signal_level as f64;

                // Feed the measurement back into the time-based buffer sizing
                // (STREAMING_DESIGN.md §4.2): a single-service-filtered or
                // transcoded stream runs well below the per-band default, and
                // over a WAN link the difference decides whether "8 seconds of
                // buffer" means 8 seconds or 1.
                self.observe_bitrate(bitrate_mbps).await;

                self.bytes_since_last = 0;
                self.interval_packets_total = 0;
                self.interval_packets_dropped = 0;

                // Periodic DB flush (every 30 seconds)
                if self.last_db_flush.elapsed().as_secs() >= 30 {
                    self.flush_metrics_to_db().await;
                    self.last_db_flush = std::time::Instant::now();
                }
            }
        }

        self.send_ts_data_raw(send_data).await
    }

    /// Send raw TS data directly to the client via the writer task.
    ///
    /// The frame is built in-place using the same wire format (BNDP header +
    /// payload) so the client's fast-path TsData decoder works unchanged.
    ///
    /// STREAMING_DESIGN.md §4.3: the wire frame is first routed through the
    /// prefill/jitter buffer. While it is filling, the frame is queued here
    /// and never reaches `send_ts_frame` below — it does not go through the
    /// class-specific backpressure policy at all (a RECORD session's
    /// no-loss overflow timer, in particular, only starts counting once
    /// prefill has released, since nothing has actually been asked to leave
    /// the queue yet). Once the target is reached, the whole queue flushes
    /// through the loop below in one shot.
    ///
    /// Applies the class-specific backpressure policy from
    /// `send_ts_frame` (STREAMING_DESIGN.md §3.2):
    /// - VIEW/PREVIEW: `try_send`, drop the frame on Full so the select loop
    ///   is never blocked by network backpressure. The 256-slot buffer holds
    ///   ~15–25 s of TS data at typical bitrates, so only prolonged network
    ///   congestion causes drops.
    /// - RECORD: blocking `send` bounded by `RECORD_OVERFLOW_TIMEOUT`. This
    ///   deliberately stalls the select loop (so control-message handling is
    ///   delayed too) for as long as `RECORD_OVERFLOW_TIMEOUT` — accepted
    ///   because a RECORD client that cannot drain 10 s of buffered data is
    ///   already beyond saving; disconnecting is the correct outcome
    ///   (STREAMING_DESIGN.md §12-1).
    ///
    /// Returns `Ok(true)` when the caller should disconnect the session
    /// (RECORD overflow timeout).
    async fn send_ts_data_raw(&mut self, data: Bytes) -> std::io::Result<bool> {
        use bytes::BufMut;
        use recisdb_protocol::{MessageType, MAGIC};

        let payload_len = data.len() as u32;
        let mut frame = BytesMut::with_capacity(10 + data.len());
        frame.put_slice(&MAGIC);
        frame.put_u32_le(payload_len);
        frame.put_u16_le(MessageType::TsData.into());
        frame.put_slice(&data);

        let frame = frame.freeze();

        let frames = match self.prefill_buffer.push(frame) {
            Some(frames) => frames,
            // Still filling: queued, nothing to send yet.
            None => return Ok(false),
        };

        for frame in frames {
            let data_len = frame.len();

            match send_ts_frame(
                &self.ts_write_queue,
                frame,
                self.stream_class,
                RECORD_OVERFLOW_TIMEOUT,
            )
            .await
            {
                TsFrameSendOutcome::Sent => {}
                TsFrameSendOutcome::DroppedFull => {
                    // The write buffer is full — the writer task can't keep up
                    // with the network.  Drop this frame to keep the select
                    // loop responsive.  The buffer holds ~15–25 s of data, so
                    // reaching this point implies prolonged network congestion.
                    //
                    // Clear carry buffers so the next frame starts with a clean
                    // 188-byte alignment (same recovery as broadcast Lagged).
                    self.ts_send_carry.clear();
                    self.ts_quality_carry.clear();
                    // The gap is accounted for as loss_ts_queue_chunks (and the
                    // single packets_dropped below); drop the CC baseline so the
                    // resync isn't additionally counted as one drop per PID.
                    self.ts_quality_analyzer.mark_discontinuity();
                    self.packets_dropped += 1;
                    self.loss_ts_queue_chunks += 1;

                    // Log once per second to avoid flooding.
                    static LAST_WARN: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let prev = LAST_WARN.load(std::sync::atomic::Ordering::Relaxed);
                    if now_ms.saturating_sub(prev) >= 1000 {
                        LAST_WARN.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                        warn!(
                            "[Session {}] Write buffer full, dropped TS frame ({} bytes). \
                             Total dropped: {} (loss_ts_queue_chunks={})",
                            self.id, data_len, self.packets_dropped, self.loss_ts_queue_chunks
                        );
                    }
                }
                TsFrameSendOutcome::RecordOverflowTimeout => {
                    // STREAMING_DESIGN.md §12-1: RECORD never silently drops.
                    // The write buffer stayed full for the entire overflow
                    // timeout — the client/network cannot keep up, so the "no
                    // loss" guarantee can no longer be honored. Disconnect with
                    // a recorded reason instead of violating it silently.
                    error!(
                        "[Session {}] RECORD write buffer overflow (stalled >{:?}), disconnecting",
                        self.id, RECORD_OVERFLOW_TIMEOUT
                    );
                    self.disconnect_reason = Some("record_queue_overflow".to_string());
                    return Ok(true);
                }
                TsFrameSendOutcome::WriterClosed => {
                    // Writer task died — signal disconnect.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "writer task closed",
                    ));
                }
            }
        }

        Ok(false)
    }

    /// Send a server message to the client via the writer task.
    ///
    /// Control messages are sent on a separate priority channel so they
    /// are not delayed behind a large queue of TS data frames.
    async fn send_message(&mut self, msg: ServerMessage) -> std::io::Result<()> {
        trace!("[Session {}] Sending: {:?}", self.id, msg);

        let encoded = encode_server_message(&msg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        self.ctrl_write_tx
            .send(encoded)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer task closed"))
    }

    /// Send an error message to the client.
    async fn send_error(&mut self, code: ErrorCode, message: &str) -> std::io::Result<()> {
        self.send_message(ServerMessage::Error {
            error_code: code.into(),
            message: message.to_string(),
        })
        .await
    }

    /// Flush current session metrics to DB (periodic update during streaming).
    async fn flush_metrics_to_db(&mut self) {
        let duration_secs = self.session_started_at.elapsed().as_secs() as i64;
        let average_signal = if self.signal_samples > 0 {
            Some(self.signal_level_sum / self.signal_samples as f64)
        } else {
            None
        };
        let average_bitrate_mbps = if duration_secs > 0 {
            Some((self.ts_bytes_sent as f64 * 8.0) / 1_000_000.0 / duration_secs as f64)
        } else {
            None
        };

        let current_packets = self.ts_bytes_sent / 188;

        // Update session history progress
        if let Some(history_id) = self.session_history_id {
            let db = self.database.lock().await;
            if let Err(e) = db.update_session_progress(
                history_id,
                duration_secs,
                current_packets,
                self.packets_dropped,
                self.packets_scrambled,
                self.packets_error,
                self.ts_bytes_sent,
                average_bitrate_mbps,
                average_signal,
                self.current_tuner_path.as_deref(),
                self.current_channel_info.as_deref(),
                self.current_channel_name.as_deref(),
            ) {
                warn!(
                    "[Session {}] Failed to flush session progress to DB: {}",
                    self.id, e
                );
            }
        }

        // Update driver quality stats (delta-based, no session count increment)
        if let Some(driver_id) = self.current_bon_driver_id {
            let delta_packets = current_packets - self.flushed_packets;
            let delta_dropped = self.packets_dropped - self.flushed_dropped;
            let delta_scrambled = self.packets_scrambled - self.flushed_scrambled;
            let delta_error = self.packets_error - self.flushed_error;

            let db = self.database.lock().await;
            if let Err(e) = QualityScorer::update_stats_delta(
                &db,
                driver_id,
                delta_packets,
                delta_dropped,
                delta_scrambled,
                delta_error,
                current_packets,
                self.packets_dropped,
                self.packets_error,
                false,
            ) {
                warn!(
                    "[Session {}] Failed to flush driver quality stats to DB: {}",
                    self.id, e
                );
            }

            // Update flushed counters
            self.flushed_packets = current_packets;
            self.flushed_dropped = self.packets_dropped;
            self.flushed_scrambled = self.packets_scrambled;
            self.flushed_error = self.packets_error;
        }

        debug!(
            "[Session {}] Flushed metrics to DB (duration={}s, dropped={}, scrambled={}, error={})",
            self.id,
            duration_secs,
            self.packets_dropped,
            self.packets_scrambled,
            self.packets_error
        );
    }

    /// Clean up session resources.
    async fn cleanup(&mut self) {
        // Shut down the writer task:  dropping the senders signals the writer
        // to drain remaining data and exit.  We then wait for it to finish so
        // that the client receives any final control messages (e.g. error).
        // Use a bounded clone so we can explicitly drop and await.
        drop(std::mem::replace(
            &mut self.ts_write_queue,
            TsWriteQueue::new(1).0,
        ));
        drop(std::mem::replace(
            &mut self.ctrl_write_tx,
            mpsc::channel(1).0,
        ));
        if let Some(handle) = self.writer_handle.take() {
            // Give the writer a few seconds to flush remaining data.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
        }

        self.stop_warm_tuner().await;
        // Release the tracked subscription (if any) first — dropping it
        // performs the decrement that used to be a manual `tuner.unsubscribe()`
        // call gated on `ts_receiver.is_some()`.
        self.ts_receiver = None;
        if self.current_tuner.is_none() {
            // Nothing to hand back. If a reader is still running for this
            // session's last channel, nobody will schedule its keep-alive
            // close and its slot never comes back — so this branch being
            // taken while a reader is alive is a leak, and worth seeing.
            info!("[Session {}] cleanup: no current tuner to release", self.id);
        }
        if let Some(tuner) = self.current_tuner.take() {
            // ★ Always check if we should stop the reader
            // This handles the case where StopStream was called before disconnect
            // (the subscription was already released above, but tuner may
            // still have no subscribers)
            if tuner.subscriber_count() == 0 {
                info!(
                    "[Session {}] No more subscribers, scheduling keep-alive close for {:?}",
                    self.id, tuner.key
                );
                self.tuner_pool
                    .schedule_idle_close(tuner.key.clone(), Arc::clone(&tuner))
                    .await;
            } else {
                info!(
                    "[Session {}] Leaving {:?} to its remaining {} subscriber(s)",
                    self.id,
                    tuner.key,
                    tuner.subscriber_count()
                );
            }
        }
        self.stop_tsreplace_pipeline().await;
        let final_tuner_path = self.current_tuner_path.clone();
        self.current_tuner_path = None;

        // Update session history and driver quality stats
        if self.disconnect_reason.is_none() {
            self.disconnect_reason = Some("client_disconnect".to_string());
        }

        let duration_secs = self.session_started_at.elapsed().as_secs() as i64;
        let average_signal = if self.signal_samples > 0 {
            Some(self.signal_level_sum / self.signal_samples as f64)
        } else {
            None
        };

        let average_bitrate_mbps = if duration_secs > 0 {
            Some((self.ts_bytes_sent as f64 * 8.0) / 1_000_000.0 / duration_secs as f64)
        } else {
            None
        };

        if let Some(history_id) = self.session_history_id {
            let ended_at = chrono::Utc::now().timestamp();
            let top_loss_pids = self.ts_quality_analyzer.top_loss_pids(10);
            let loss_summary = serde_json::json!({
                "broadcast_lag_chunks": self.loss_broadcast_lag_chunks,
                "ts_queue_chunks": self.loss_ts_queue_chunks,
                "encoder_stall_events": self.loss_encoder_stall_events,
                "top_pids": top_loss_pids,
            })
            .to_string();
            let db = self.database.lock().await;
            if let Err(e) = db.update_session_end(
                history_id,
                ended_at,
                duration_secs,
                self.ts_bytes_sent / 188,
                self.packets_dropped,
                self.packets_scrambled,
                self.packets_error,
                self.ts_bytes_sent,
                average_bitrate_mbps,
                average_signal,
                self.disconnect_reason.as_deref(),
                final_tuner_path.as_deref(),
                self.current_channel_info.as_deref(),
                self.current_channel_name.as_deref(),
                Some(&loss_summary),
                Some(self.stream_class.as_str()),
            ) {
                warn!(
                    "[Session {}] Failed to update session history: {}",
                    self.id, e
                );
            }
        }

        if let Some(driver_id) = self.current_bon_driver_id {
            let current_packets = self.ts_bytes_sent / 188;
            let delta_packets = current_packets - self.flushed_packets;
            let delta_dropped = self.packets_dropped - self.flushed_dropped;
            let delta_scrambled = self.packets_scrambled - self.flushed_scrambled;
            let delta_error = self.packets_error - self.flushed_error;

            let db = self.database.lock().await;
            if let Err(e) = QualityScorer::update_stats_delta(
                &db,
                driver_id,
                delta_packets,
                delta_dropped,
                delta_scrambled,
                delta_error,
                current_packets,
                self.packets_dropped,
                self.packets_error,
                true, // increment session count at session end
            ) {
                warn!(
                    "[Session {}] Failed to update driver quality stats: {}",
                    self.id, e
                );
            }
        }

        // Update session registry
        self.session_registry.update_tuner(self.id, None).await;
        self.session_registry.update_streaming(self.id, false).await;
        self.session_registry.update_channel(self.id, None).await;
    }
}

/// Whether `data` can go straight out without passing through the
/// realignment carry buffer (P3 §2.2-9).
///
/// True only when there is nothing pending from the previous chunk and the
/// chunk itself is a whole number of TS packets starting on a sync byte —
/// the steady state of a healthy stream. Anything else (a partial packet, a
/// chunk that starts mid-packet, or leftover bytes still waiting to be
/// re-synced) has to go through the carry buffer, so that the packet
/// boundaries the client sees stay correct across chunk edges.
fn is_aligned_passthrough(carry_is_empty: bool, data: &[u8]) -> bool {
    carry_is_empty && !data.is_empty() && data.len() % 188 == 0 && data[0] == 0x47
}

impl Drop for Session {
    fn drop(&mut self) {
        debug!("[Session {}] Session dropped", self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- STREAMING_DESIGN.md §2: auto-promotion threshold ----

    #[test]
    fn record_priority_threshold_promotes_only_at_or_above_200() {
        assert!(!should_auto_promote_to_record(0));
        assert!(!should_auto_promote_to_record(10)); // 視聴 目安
        assert!(!should_auto_promote_to_record(199));
        assert!(should_auto_promote_to_record(200)); // 録画(通常) 目安
        assert!(should_auto_promote_to_record(255)); // 録画(排他) 目安
        assert!(should_auto_promote_to_record(i32::MAX));
    }

    // ---- NO_TS_SOURCE_GRACE: a Streaming session with nothing attached ----

    #[test]
    fn first_tick_without_a_ts_source_starts_the_clock_but_does_not_disconnect() {
        let now = std::time::Instant::now();
        let mut since = None;
        assert!(!no_ts_source_deadline_passed(
            &mut since,
            now,
            NO_TS_SOURCE_GRACE
        ));
        assert_eq!(since, Some(now), "the clock starts on the first idle tick");
    }

    #[test]
    fn the_clock_keeps_its_original_start_across_ticks_and_expires() {
        let start = std::time::Instant::now();
        let mut since = None;
        no_ts_source_deadline_passed(&mut since, start, NO_TS_SOURCE_GRACE);

        // Still inside the grace period: the start must not be pushed forward.
        assert!(!no_ts_source_deadline_passed(
            &mut since,
            start + NO_TS_SOURCE_GRACE / 2,
            NO_TS_SOURCE_GRACE
        ));
        assert_eq!(since, Some(start));

        assert!(no_ts_source_deadline_passed(
            &mut since,
            start + NO_TS_SOURCE_GRACE,
            NO_TS_SOURCE_GRACE
        ));
    }

    /// The run loop clears `no_ts_source_since` as soon as a source is
    /// attached again, so a later failure gets a full grace period rather
    /// than an instant disconnect.
    #[test]
    fn a_reattached_source_resets_the_clock() {
        let start = std::time::Instant::now();
        let mut since = Some(start);
        since = None; // what the run loop does once ts_receiver/encoder is back

        assert!(!no_ts_source_deadline_passed(
            &mut since,
            start + NO_TS_SOURCE_GRACE * 2,
            NO_TS_SOURCE_GRACE
        ));
    }

    // ---- P3 §2.2-9: outgoing alignment fast path ----

    #[test]
    fn aligned_chunk_with_empty_carry_skips_the_realignment_buffer() {
        let mut chunk = vec![0u8; 188 * 3];
        chunk[0] = 0x47;
        chunk[188] = 0x47;
        chunk[376] = 0x47;
        assert!(is_aligned_passthrough(true, &chunk));
    }

    #[test]
    fn leftover_carry_forces_the_slow_path() {
        let mut chunk = vec![0u8; 188];
        chunk[0] = 0x47;
        assert!(
            !is_aligned_passthrough(false, &chunk),
            "bytes pending from the previous chunk must be prepended, not bypassed"
        );
    }

    #[test]
    fn misaligned_or_unsynced_chunks_force_the_slow_path() {
        let mut partial = vec![0u8; 200]; // not a whole number of packets
        partial[0] = 0x47;
        assert!(!is_aligned_passthrough(true, &partial));

        let unsynced = vec![0u8; 188]; // right length, wrong first byte
        assert!(!is_aligned_passthrough(true, &unsynced));

        assert!(!is_aligned_passthrough(true, &[]));
    }

    // ---- STREAMING_DESIGN.md §3.2: class-specific TS send backpressure ----

    /// Not the production `RECORD_OVERFLOW_TIMEOUT` (10 s) — short enough to
    /// keep the timeout test fast while still exercising the real code path.
    const TEST_RECORD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(20);

    /// Build a queue whose byte budget admits exactly one 5-byte frame, so the
    /// "full" branch is reached by *bytes* rather than by slot count.
    fn tiny_queue() -> (TsWriteQueue, mpsc::Receiver<Bytes>) {
        let (queue, rx, _shared) = TsWriteQueue::new(5);
        (queue, rx)
    }

    #[tokio::test]
    async fn view_class_drops_frame_when_write_budget_is_exhausted() {
        let (queue, mut rx) = tiny_queue();
        assert_eq!(
            send_ts_frame(
                &queue,
                Bytes::from_static(b"first"),
                StreamClass::View,
                TEST_RECORD_TIMEOUT
            )
            .await,
            TsFrameSendOutcome::Sent
        );

        let outcome = send_ts_frame(
            &queue,
            Bytes::from_static(b"second"),
            StreamClass::View,
            TEST_RECORD_TIMEOUT,
        )
        .await;
        assert_eq!(outcome, TsFrameSendOutcome::DroppedFull);

        // Only the original frame is queued; the second was dropped, not enqueued.
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"first"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn preview_class_drops_frame_when_write_budget_is_exhausted() {
        let (queue, mut rx) = tiny_queue();
        send_ts_frame(
            &queue,
            Bytes::from_static(b"first"),
            StreamClass::Preview,
            TEST_RECORD_TIMEOUT,
        )
        .await;

        let outcome = send_ts_frame(
            &queue,
            Bytes::from_static(b"second"),
            StreamClass::Preview,
            TEST_RECORD_TIMEOUT,
        )
        .await;
        assert_eq!(outcome, TsFrameSendOutcome::DroppedFull);
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"first"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn record_class_times_out_when_the_queue_never_drains() {
        let (queue, _rx) = tiny_queue();
        send_ts_frame(
            &queue,
            Bytes::from_static(b"first"),
            StreamClass::Record,
            TEST_RECORD_TIMEOUT,
        )
        .await;

        // RECORD must never drop — it blocks up to the timeout, then
        // reports the overflow instead of silently losing data
        // (STREAMING_DESIGN.md §12-1).
        let outcome = send_ts_frame(
            &queue,
            Bytes::from_static(b"second"),
            StreamClass::Record,
            TEST_RECORD_TIMEOUT,
        )
        .await;
        assert_eq!(outcome, TsFrameSendOutcome::RecordOverflowTimeout);
    }

    #[tokio::test]
    async fn record_class_sends_once_the_queue_drains() {
        let (queue, mut rx) = tiny_queue();
        send_ts_frame(
            &queue,
            Bytes::from_static(b"first"),
            StreamClass::Record,
            TEST_RECORD_TIMEOUT,
        )
        .await;

        // Simulate the writer task draining the first frame while the sender
        // waits: the RECORD send must resume and deliver, not time out.
        let shared = Arc::clone(queue.shared());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            shared.release(5);
        });

        let outcome = send_ts_frame(
            &queue,
            Bytes::from_static(b"secnd"),
            StreamClass::Record,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome, TsFrameSendOutcome::Sent);
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"first"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"secnd"));
    }

    #[tokio::test]
    async fn writer_closed_is_detected_for_view_and_record() {
        let (queue, rx, _shared) = TsWriteQueue::new(1024);
        drop(rx);
        assert_eq!(
            send_ts_frame(
                &queue,
                Bytes::from_static(b"x"),
                StreamClass::View,
                TEST_RECORD_TIMEOUT
            )
            .await,
            TsFrameSendOutcome::WriterClosed
        );
        assert_eq!(
            send_ts_frame(
                &queue,
                Bytes::from_static(b"y"),
                StreamClass::Record,
                TEST_RECORD_TIMEOUT
            )
            .await,
            TsFrameSendOutcome::WriterClosed
        );
    }

    /// A failed hand-off must not leave its bytes counted against the budget —
    /// a leak here throttles the session down to a standstill over time.
    #[tokio::test]
    async fn a_failed_handoff_returns_its_bytes_to_the_budget() {
        let (queue, rx, shared) = TsWriteQueue::new(1024);
        drop(rx);
        assert_eq!(
            send_ts_frame(
                &queue,
                Bytes::from_static(b"x"),
                StreamClass::View,
                TEST_RECORD_TIMEOUT
            )
            .await,
            TsFrameSendOutcome::WriterClosed
        );
        assert_eq!(
            shared.queued(),
            0,
            "reservation must be released on failure"
        );
    }

    /// The point of the seconds-based budget: the same setting must mean the
    /// same amount of time regardless of how big the upstream's chunks are.
    #[test]
    fn budget_is_a_duration_not_a_frame_count() {
        // 8 s of an 18 Mbps multiplex vs. 8 s of a 2 Mbps transcode.
        assert_eq!(budget_bytes(18_000_000, 8_000), 18_000_000);
        assert_eq!(budget_bytes(2_000_000, 8_000), 2_000_000);

        // Frame size does not enter into it: 64 KB chunks and 256 KB chunks
        // both get 8 seconds' worth.
        let full = budget_bytes(18_000_000, 8_000);
        assert_eq!(full / (64 * 1024), 274, "≈274 frames at 64KB");
        assert_eq!(full / (256 * 1024), 68, "≈68 frames at 256KB");
    }
}
