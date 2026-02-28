//! Client session handling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use bytes::{Bytes, BytesMut};
use log::{debug, error, info, trace, warn};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc};

use recisdb_protocol::{
    broadcast_region::{classify_nid, TerrestrialRegion},
    decode_client_message, decode_header, encode_server_message, ClientChannelInfo,
    ClientMessage, ErrorCode, ServerMessage, HEADER_SIZE, PROTOCOL_VERSION,
};

use crate::server::listener::DatabaseHandle;
use crate::tuner::{ChannelKey, SharedTuner, TunerPool, WarmTunerHandle, ts_analyzer::TsPacketAnalyzer};
use crate::tuner::quality_scorer::QualityScorer;
use crate::tuner::channel_key::ChannelKeySpec;
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

#[derive(Debug, Clone)]
struct TsreplaceRuntimeConfig {
    enabled: bool,
    command_path: String,
    arguments: String,
    read_timeout_ms: u64,
    passthrough_on_error: bool,
}

fn fallback_space_label(actual_space: u32) -> String {
    // 最小実装: よくある割当の想定
    // 必要なら後で NID/分類でより正確に推定する
    match actual_space {
        0 => "GR".to_string(),
        1 => "BS/CS".to_string(),
        2 => "BS".to_string(),
        3 => "CS".to_string(),
        _ => format!("Space{}", actual_space),
    }
}

#[derive(Clone, Debug)]
struct ChannelEntry {
    bon_channel: u32,     // 実際の物理チャンネル番号 (代表ドライバのもの)
    name: String,         // 表示名
    nid: u16,             // Network ID (NID+TSIDでの一意識別用)
    tsid: u16,            // Transport Stream ID
}

/// Multiple driver mappings for a single virtual channel
#[derive(Clone, Debug)]
struct VirtualChannelMapping {
    driver_path: String,  // BonDriver DLL path
    actual_space: u32,    // Physical space on this driver
    actual_channel: u32,  // Physical channel on this driver
}


/// A client session.
pub struct Session {
    /// Unique session ID.
    id: u64,
    /// Client address.
    #[allow(dead_code)]
    addr: SocketAddr,
    /// TCP socket.
    socket: TcpStream,
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
    /// Warm tuner handle for pre-opened BonDriver.
    warm_tuner: Option<WarmTunerHandle>,
    /// Warm tuner path.
    warm_tuner_path: Option<String>,
    /// Current tuner path.
    current_tuner_path: Option<String>,
    /// Default tuner path.
    default_tuner: Option<String>,
    /// Current group name (if opened with group).
    current_group_name: Option<String>,
    /// Group drivers (paths for all drivers in the group).
    group_driver_paths: Vec<String>,
    /// TS data receiver (when streaming).
    ts_receiver: Option<broadcast::Receiver<Bytes>>,
    // Session struct に追加
    ts_bytes_sent: u64,
    ts_msgs_sent: u64,
    last_ts_log: std::time::Instant,
    channel_map_cache: HashMap<u32, Vec<ChannelEntry>>,
    // ★追加: 仮想space_idx(0..N-1) -> (actual_space, display_name, region_key) のマップをチューナごとにキャッシュ
    // 例: [(0, "地デジ", "宮城"), (0, "地デジ", "福島"), (1, "BS", "BS"), (2, "CS", "CS")]
    // region_key はチャンネルフィルタリング用、display_name は EnumTuningSpace 表示用
    space_list_cache: HashMap<String, Vec<(u32, String, String)>>,
    // ★追加: 仮想チャンネル (NID, TSID) -> 複数のドライバー/スペース/チャンネル マッピング
    // 同じNID+TSIDが複数のドライバーに存在する場合、すべてのマッピングを保持
    virtual_channel_mappings: HashMap<String, HashMap<(u16, u16), Vec<VirtualChannelMapping>>>,
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
    /// tsreplace stdin input channel.
    tsreplace_input_tx: Option<mpsc::Sender<Bytes>>,
    /// tsreplace stdout output channel.
    tsreplace_output_rx: Option<mpsc::Receiver<Bytes>>,
    /// tsreplace process handle.
    tsreplace_child: Option<Child>,
    /// tsreplace output stall timeout.
    tsreplace_read_timeout: std::time::Duration,
    /// Fallback to raw TS when tsreplace fails/stalls.
    tsreplace_passthrough_on_error: bool,
    /// Last time encoded output was received.
    tsreplace_last_output_at: std::time::Instant,
}

impl Session {
    /// Create a new session.
    pub fn new(
        id: u64,
        addr: SocketAddr,
        socket: TcpStream,
        tuner_pool: Arc<TunerPool>,
        database: DatabaseHandle,
        default_tuner: Option<String>,
        session_registry: Arc<SessionRegistry>,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Self {
        Self {
            id,
            addr,
            socket,
            read_buf: BytesMut::with_capacity(65536),
            state: SessionState::Initial,
            tuner_pool,
            database,
            current_tuner: None,
            warm_tuner: None,
            warm_tuner_path: None,
            current_tuner_path: None,
            default_tuner,
            current_group_name: None,
            group_driver_paths: Vec::new(),
            ts_receiver: None,
            ts_bytes_sent: 0,
            ts_msgs_sent: 0,
            last_ts_log: std::time::Instant::now(),
            channel_map_cache: HashMap::new(),
            space_list_cache: HashMap::new(),
            virtual_channel_mappings: HashMap::new(),
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
            tsreplace_input_tx: None,
            tsreplace_output_rx: None,
            tsreplace_child: None,
            tsreplace_read_timeout: std::time::Duration::from_millis(10_000),
            tsreplace_passthrough_on_error: true,
            tsreplace_last_output_at: std::time::Instant::now(),
        }
    }

    async fn load_tsreplace_runtime_config(&self) -> TsreplaceRuntimeConfig {
        let db = self.database.lock().await;
        match db.get_tsreplace_config() {
            Ok((enabled, command_path, arguments, read_timeout_ms, passthrough_on_error)) => TsreplaceRuntimeConfig {
                enabled,
                command_path,
                arguments,
                read_timeout_ms,
                passthrough_on_error,
            },
            Err(e) => {
                warn!("[Session {}] Failed to load tsreplace config: {}", self.id, e);
                TsreplaceRuntimeConfig {
                    enabled: false,
                    command_path: "tsreplace".to_string(),
                    arguments: String::new(),
                    read_timeout_ms: 10_000,
                    passthrough_on_error: true,
                }
            }
        }
    }

    async fn stop_tsreplace_pipeline(&mut self) {
        self.tsreplace_input_tx = None;
        self.tsreplace_output_rx = None;

        if let Some(mut child) = self.tsreplace_child.take() {
            if let Err(e) = child.start_kill() {
                debug!("[Session {}] tsreplace kill skipped: {}", self.id, e);
            }
            let _ = child.wait().await;
        }
    }

    async fn start_tsreplace_pipeline(&mut self) -> std::io::Result<()> {
        self.stop_tsreplace_pipeline().await;

        let cfg = self.load_tsreplace_runtime_config().await;
        self.tsreplace_passthrough_on_error = cfg.passthrough_on_error;
        self.tsreplace_read_timeout = std::time::Duration::from_millis(cfg.read_timeout_ms.max(1));

        if !cfg.enabled {
            return Ok(());
        }

        let mut cmd = Command::new(&cfg.command_path);
        for arg in cfg.arguments.split_whitespace() {
            cmd.arg(arg);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to spawn tsreplace '{}': {}", cfg.command_path, e),
            )
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "tsreplace stdin not available")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "tsreplace stdout not available")
        })?;

        let stderr = child.stderr.take();

        let (in_tx, mut in_rx) = mpsc::channel::<Bytes>(64);
        let (out_tx, out_rx) = mpsc::channel::<Bytes>(64);

        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(chunk) = in_rx.recv().await {
                if let Err(e) = stdin.write_all(&chunk).await {
                    warn!("[tsreplace] stdin write failed: {}", e);
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });

        tokio::spawn(async move {
            let mut stdout = stdout;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if out_tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("[tsreplace] stdout read failed: {}", e);
                        break;
                    }
                }
            }
        });

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => debug!("[tsreplace] {}", line),
                        Ok(None) => break,
                        Err(e) => {
                            warn!("[tsreplace] stderr read failed: {}", e);
                            break;
                        }
                    }
                }
            });
        }

        self.tsreplace_input_tx = Some(in_tx);
        self.tsreplace_output_rx = Some(out_rx);
        self.tsreplace_child = Some(child);
        self.tsreplace_last_output_at = std::time::Instant::now();

        info!(
            "[Session {}] tsreplace pipeline started: command='{}' args='{}'",
            self.id, cfg.command_path, cfg.arguments
        );

        Ok(())
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
            self.current_bon_driver_id = db.get_bon_driver_by_path(path).ok().flatten().map(|d| d.id);
        } else {
            self.current_bon_driver_id = None;
        }
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
                        if t.is_running() {
                            found = true;
                            break;
                        }
                    }
                }
            }
            found
        };
        if already_running {
            debug!("[Session {}] Skipping warm tuner for {} – driver already has a running reader",
                   self.id, tuner_path);
            return;
        }

        self.stop_warm_tuner().await;

        let warm = WarmTunerHandle::spawn(tuner_path.to_string(), config.prewarm_timeout_secs);
        self.warm_tuner_path = Some(tuner_path.to_string());
        self.warm_tuner = Some(warm);
    }

    /// After a channel switch failure, attempt to restore the previous channel so the
    /// client (TVTest, etc.) keeps receiving TS data instead of being cut off.
    ///
    /// The old tuner may still be alive in the pool when `keep_alive_secs > 0` (default 60 s).
    /// If it is still running we cancel the idle-close timer and re-subscribe.
    async fn try_restore_previous_channel(&mut self, old_tuner_key: &Option<ChannelKey>) {
        let Some(ref old_key) = old_tuner_key else { return };
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
        info!("[Session {}] Channel switch failed — restoring previous channel {:?}", self.id, old_key);
        // Cancel any pending idle-close so the tuner stays alive.
        self.tuner_pool.cancel_idle_close(old_key).await;
        self.current_tuner = Some(old_tuner.clone());
        // If we were (or are still) streaming, re-subscribe so TS data flows again.
        if self.state == SessionState::Streaming && self.ts_receiver.is_none() {
            self.ts_receiver = Some(old_tuner.subscribe());
        }
    }

    /// Try fallback drivers when the primary driver fails.
    /// `skip_paths` contains driver paths that have already been tried and should be skipped.
    /// Returns `Some((tuner, path))` on success, `None` if all fallback candidates fail.
    async fn try_fallback_drivers(
        &mut self,
        fallback_candidates: &[(String, u32, u32)],
        skip_paths: &[&str],
    ) -> Option<(Arc<SharedTuner>, String)> {
        for (fallback_path, fallback_space, fallback_bon_channel) in fallback_candidates.iter() {
            if skip_paths.iter().any(|s| s == fallback_path) {
                continue;
            }

            // Check whether this DLL has room for another instance.
            let fallback_key = ChannelKey::space_channel(fallback_path, *fallback_space, *fallback_bon_channel);
            let fb_max_instances = {
                let db = self.database.lock().await;
                db.get_max_instances_for_path(fallback_path).unwrap_or(1)
            };
            let guard_keys = self.tuner_pool.keys().await;
            let mut fb_running = 0i32;
            for gk in &guard_keys {
                if gk.tuner_path == *fallback_path && *gk != fallback_key {
                    if let Some(other) = self.tuner_pool.get(gk).await {
                        if other.is_running() {
                            fb_running += 1;
                        }
                    }
                }
            }
            // +1 because we would start a new instance on this DLL
            if (fb_running + 1) > fb_max_instances {
                // ★ Before giving up, try to evict subscriberless (idle) tuners
                // on this DLL.  These may be left by idle-close timers or from
                // sessions that switched away but whose old reader has not yet
                // timed out.  Freeing one slot lets us proceed.
                let mut freed = false;
                for gk in &guard_keys {
                    if gk.tuner_path == *fallback_path && *gk != fallback_key {
                        if let Some(other) = self.tuner_pool.get(gk).await {
                            if other.is_running() && !other.has_subscribers() {
                                info!("[Session {}] Evicting idle tuner {:?} to make room for fallback driver {}",
                                      self.id, gk, fallback_path);
                                self.tuner_pool.cancel_idle_close(gk).await;
                                other.stop_reader().await;
                                self.tuner_pool.remove(gk).await;
                                freed = true;
                                break;
                            }
                        }
                    }
                }
                if !freed {
                    debug!("[Session {}] Fallback {} skipped: at capacity ({}/{}), no idle instances to evict",
                           self.id, fallback_path, fb_running, fb_max_instances);
                    continue;
                }
                // Slot freed — proceed with this candidate
            }

            info!("[Session {}] Trying fallback driver: {} (space {}, ch {})", self.id, fallback_path, fallback_space, fallback_bon_channel);

            match self.tuner_pool.get_or_create(fallback_key.clone(), 2, || async { Ok(()) }).await {
                Ok(fb_tuner) => {
                    if fb_tuner.is_running() {
                        // Already running the same channel — reuse it directly
                        info!("[Session {}] Fallback driver {} already running same channel, reusing", self.id, fallback_path);
                        return Some((fb_tuner, fallback_path.clone()));
                    }
                    // Not running — start the reader
                    match self.start_reader_with_warm(
                        Arc::clone(&fb_tuner),
                        fallback_path.clone(),
                        *fallback_space,
                        *fallback_bon_channel,
                    ).await {
                        Ok(_) => {
                            info!("[Session {}] Successfully started BonDriver reader with fallback driver: {}", self.id, fallback_path);
                            return Some((fb_tuner, fallback_path.clone()));
                        }
                        Err(e) => {
                            warn!("[Session {}] Fallback driver {} reader start failed: {}", self.id, fallback_path, e);
                            // ★ Bug G fix: get_or_create inserted this tuner into the pool.
                            // Remove the orphaned (not-running, no-subscriber) entry so it
                            // doesn't persist indefinitely.
                            if !fb_tuner.is_running() && !fb_tuner.has_subscribers() {
                                self.tuner_pool.remove(&fallback_key).await;
                            }
                            continue;
                        }
                    }
                }
                Err(e) => {
                    warn!("[Session {}] Fallback driver {} creation failed: {}", self.id, fallback_path, e);
                    continue;
                }
            }
        }
        None
    }

    async fn start_reader_with_warm(
        &mut self,
        tuner: Arc<SharedTuner>,
        tuner_path: String,
        space: u32,
        channel: u32,
    ) -> std::io::Result<()> {
        let config = self.tuner_pool.config().await;
        let startup_config = crate::tuner::shared::ReaderStartupConfig::from(&config);
        if !config.prewarm_enabled {
            self.stop_warm_tuner().await;
            return tuner
                .start_bondriver_reader(tuner_path, space, channel, startup_config)
                .await;
        }

        if let Some(mut warm) = self.warm_tuner.take() {
            if self.warm_tuner_path.as_deref() == Some(tuner_path.as_str()) {
                match warm
                    .activate(
                        Arc::clone(&tuner),
                        tuner_path.clone(),
                        space,
                        channel,
                        startup_config,
                    )
                    .await
                {
                    Ok(()) => {
                        self.warm_tuner_path = None;
                        return Ok(());
                    }
                    Err(e) => {
                        warn!("[Session {}] Warm tuner activation failed: {}", self.id, e);
                        warm.shutdown().await;
                        self.warm_tuner_path = None;
                    }
                }
            } else {
                warm.shutdown().await;
                self.warm_tuner_path = None;
            }
        }

        tuner
            .start_bondriver_reader(tuner_path, space, channel, startup_config)
            .await
    }

    async fn build_channel_map_for_space(&self, tuner_path: &str, space: u32)
        -> Vec<ChannelEntry>
    {
        let db = self.database.lock().await;

        // driver id を引く
        let Some(driver) = db.get_bon_driver_by_path(tuner_path).ok().flatten() else {
            return vec![];
        };

        // get_all_channels_with_drivers で取得してフィルタ
        let all = match db.get_all_channels_with_drivers() {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        let mut uniq: BTreeMap<u32, (String, u16, u16)> = BTreeMap::new();

        for (ch, bd_opt) in all {
            let Some(bd) = bd_opt else { continue; };
            if bd.id != driver.id { continue; }
            if ch.space != space { continue; }
            if !ch.is_enabled { continue; }

            let name = ch.service_name
                .clone()
                .or(ch.ts_name.clone())
                .unwrap_or_else(|| format!("CH{}", ch.channel));

            uniq.entry(ch.channel).or_insert((name, ch.nid as u16, ch.tsid as u16));
        }

        uniq.into_iter()
            .map(|(bon_channel, (name, nid, tsid))| ChannelEntry { bon_channel, name, nid, tsid })
            .collect()
    }

    async fn ensure_channel_map(&mut self, space: u32) -> Vec<ChannelEntry> {
        if let Some(v) = self.channel_map_cache.get(&space) {
            trace!("[Session {}] ensure_channel_map: using cache for space {} (channels: {})", self.id, space, v.len());
            return v.clone();
        }

        let map = if !self.group_driver_paths.is_empty() {
            // Group mode: aggregate channels from all group drivers
            let db = self.database.lock().await;

            let all = match db.get_all_channels_with_drivers() {
                Ok(v) => v,
                Err(e) => {
                    debug!("[Session {}] ensure_channel_map: failed to get channels: {}", self.id, e);
                    Vec::new()
                },
            };

            let mut uniq: BTreeMap<u32, (String, u16, u16)> = BTreeMap::new();

            for (ch, bd_opt) in all {
                let Some(bd) = bd_opt else { continue; };
                // Check if this driver belongs to the group
                if !self.group_driver_paths.contains(&bd.dll_path) {
                    continue;
                }

                if ch.space != space { continue; }
                let bch = ch.channel;

                if !ch.is_enabled { continue; }

                let name = ch.service_name
                    .clone()
                    .or(ch.ts_name.clone())
                    .unwrap_or_else(|| format!("CH{}", bch));

                uniq.entry(bch).or_insert((name, ch.nid as u16, ch.tsid as u16));
            }

            uniq.into_iter()
                .map(|(bon_channel, (name, nid, tsid))| ChannelEntry { bon_channel, name, nid, tsid })
                .collect::<Vec<_>>()
        } else {
            // Single tuner mode
            let tuner_path = self
                .current_tuner_path
                .as_ref()
                .or(self.default_tuner.as_ref())
                .cloned()
                .unwrap_or_default();

            if tuner_path.is_empty() {
                debug!("[Session {}] ensure_channel_map: tuner_path is empty for space {}", self.id, space);
                self.channel_map_cache.insert(space, Vec::new());
                return Vec::new();
            }

            let db = self.database.lock().await;

            let all = match db.get_all_channels_with_drivers() {
                Ok(v) => v,
                Err(e) => {
                    debug!("[Session {}] ensure_channel_map: failed to get channels: {}", self.id, e);
                    Vec::new()
                },
            };

            let mut uniq: BTreeMap<u32, (String, u16, u16)> = BTreeMap::new();

            for (ch, bd_opt) in all {
                let Some(bd) = bd_opt else { continue; };
                if bd.dll_path != tuner_path { continue; }

                if ch.space != space { continue; }
                let bch = ch.channel;

                if !ch.is_enabled { continue; }

                let name = ch.service_name
                    .clone()
                    .or(ch.ts_name.clone())
                    .unwrap_or_else(|| format!("CH{}", bch));

                uniq.entry(bch).or_insert((name, ch.nid as u16, ch.tsid as u16));
            }

            uniq.into_iter()
                .map(|(bon_channel, (name, nid, tsid))| ChannelEntry { bon_channel, name, nid, tsid })
                .collect::<Vec<_>>()
        };

        debug!("[Session {}] ensure_channel_map: final channels for space {}: {} items", self.id, space, map.len());
        self.channel_map_cache.insert(space, map.clone());
        map
    }

    /// Get channel map for a specific space and region (for virtual space filtering).
    async fn ensure_channel_map_with_region(&mut self, _space: u32, region_name: &str) -> Vec<ChannelEntry> {
        let db = self.database.lock().await;

        let all = match db.get_all_channels_with_drivers() {
            Ok(v) => v,
            Err(e) => {
                debug!("[Session {}] ensure_channel_map_with_region: failed to get channels: {}", self.id, e);
                Vec::new()
            },
        };

        let tuner_path = if !self.group_driver_paths.is_empty() {
            None  // Group mode
        } else {
            Some(
                self.current_tuner_path
                    .as_ref()
                    .or(self.default_tuner.as_ref())
                    .cloned()
                    .unwrap_or_default()
            )
        };

        // NID+TSIDをキーにして重複排除（異なるBonDriverが同じNID+TSIDに違うbon_channelを使う場合の対策）
        let mut uniq: BTreeMap<(u16, u16), (u32, String)> = BTreeMap::new();

        for (ch, bd_opt) in all {
            let Some(bd) = bd_opt else { continue; };

            // Check driver path/group
            if let Some(path) = &tuner_path {
                if bd.dll_path != *path { continue; }
            } else {
                // Group mode
                if !self.group_driver_paths.contains(&bd.dll_path) {
                    continue;
                }
            }

            // Filter by region/broadcast type
            // For terrestrial, filter by TerrestrialRegion display_name (広域圏: "関東", "東北", etc.)
            // For BS/CS, filter by broadcast type string ("BS" or "CS")
            let ch_matches = {
                let (btype, region) = classify_nid(ch.nid as u16);
                match btype {
                    recisdb_protocol::types::BroadcastType::BS => region_name == "BS",
                    recisdb_protocol::types::BroadcastType::CS => region_name == "CS",
                    recisdb_protocol::types::BroadcastType::Terrestrial => {
                        let ch_region = region.map(|r| match r {
                            TerrestrialRegion::Unknown(_) => "Unknown",
                            _ => r.display_name(),
                        }).unwrap_or("Unknown");
                        ch_region == region_name
                    }
                }
            };

            if !ch_matches { continue; }
            if !ch.is_enabled { continue; }

            let nid_tsid = (ch.nid as u16, ch.tsid as u16);
            let bch = ch.channel;

            let name = ch.service_name
                .clone()
                .or(ch.ts_name.clone())
                .unwrap_or_else(|| format!("CH{}", bch));

            uniq.entry(nid_tsid).or_insert((bch, name));
        }

        uniq.into_iter()
            .map(|((nid, tsid), (bon_channel, name))| ChannelEntry { bon_channel, name, nid, tsid })
            .collect::<Vec<_>>()
    }

    fn clear_caches(&mut self) {
        self.channel_map_cache.clear();
        self.space_list_cache.clear();
        self.virtual_channel_mappings.clear();
    }

    fn current_or_default_tuner_path(&self) -> String {
        self.current_tuner_path
            .as_ref()
            .or(self.default_tuner.as_ref())
            .cloned()
            .unwrap_or_default()
    }

    /// チューナに紐づく「実スペース一覧」を DB から構築してキャッシュする
    async fn ensure_space_list(&mut self) -> Vec<u32> {
        // If group is set, get spaces from all group drivers
        if !self.group_driver_paths.is_empty() {
            let cache_key = format!("group_{}", self.current_group_name.as_ref().unwrap_or(&"unknown".to_string()));
            
            if let Some(v) = self.space_list_cache.get(&cache_key) {
                trace!("[Session {}] ensure_space_list: using cache for group {} (spaces: {:?})", 
                    self.id, self.current_group_name.as_ref().unwrap_or(&"unknown".to_string()), v);
                return v.iter().map(|(actual_space, _, _)| *actual_space).collect();
            }

            let db = self.database.lock().await;
            let all = match db.get_all_channels_with_drivers() {
                Ok(v) => v,
                Err(e) => {
                    debug!("[Session {}] ensure_space_list: failed to get channels: {}", self.id, e);
                    Vec::new()
                },
            };

            // Build unique (space, region) pairs based on NID + TSID to eliminate duplicates
            // But record ALL mappings (driver, space, channel) for each NID+TSID combination
            let mut nid_tsid_seen: BTreeSet<(u16, u16)> = BTreeSet::new();
            let mut region_seen: BTreeSet<String> = BTreeSet::new();  // For BS/CS deduplication
            let mut space_region_names: HashMap<String, (u32, String)> = HashMap::new();  // region_name -> (space, name)
            let mut nid_tsid_mappings: HashMap<(u16, u16), Vec<VirtualChannelMapping>> = HashMap::new();
            
            for (ch, bd_opt) in all {
                let Some(bd) = bd_opt else { continue; };
                // Check if this driver belongs to the group
                if !self.group_driver_paths.contains(&bd.dll_path) {
                    continue;
                }
                if !ch.is_enabled { continue; }
                
                let nid_tsid = (ch.nid as u16, ch.tsid as u16);
                
                // Record this mapping for this NID+TSID (allow multiples from different drivers)
                nid_tsid_mappings
                    .entry(nid_tsid)
                    .or_insert_with(Vec::new)
                    .push(VirtualChannelMapping {
                        driver_path: bd.dll_path.clone(),
                        actual_space: ch.space,
                        actual_channel: ch.channel as u32,
                    });
                
                // For display purposes, only register once per NID+TSID
                if nid_tsid_seen.contains(&nid_tsid) {
                    continue;
                }
                nid_tsid_seen.insert(nid_tsid);
                
                // Get region name: TerrestrialRegion display_name for terrestrial (広域圏), "BS"/"CS" for satellite
                let (btype, terrestrial_region) = classify_nid(ch.nid as u16);
                let is_terrestrial = matches!(btype, recisdb_protocol::types::BroadcastType::Terrestrial)
                    && terrestrial_region.as_ref().map_or(false, |r| !matches!(r, TerrestrialRegion::Unknown(_)));
                let region_name = match btype {
                    recisdb_protocol::types::BroadcastType::BS => "BS".to_string(),
                    recisdb_protocol::types::BroadcastType::CS => "CS".to_string(),
                    recisdb_protocol::types::BroadcastType::Terrestrial => {
                        terrestrial_region.as_ref().map(|r| match r {
                            TerrestrialRegion::Unknown(_) => "Unknown".to_string(),
                            _ => r.display_name().to_string(),
                        }).unwrap_or_else(|| "Unknown".to_string())
                    }
                };
                debug!("[Session {}] NID=0x{:04X} btype={:?} region={}", 
                    self.id, ch.nid, btype, region_name);

                
                // For all regions, only register once per region name (prevent duplicates)
                // This applies to both BS/CS and terrestrial
                if region_seen.contains(&region_name) {
                    debug!("[Session {}] Skipping duplicate region: {}", self.id, region_name);
                    continue;
                }
                region_seen.insert(region_name.clone());
                
                // Build display name based on region
                let name = if is_terrestrial {
                    format!("地デジ ({})", region_name)
                } else {
                    region_name.clone()
                };
                
                // For BS/CS, use the actual space from the first driver we see
                // For terrestrial, use actual space as-is
                // This ensures each region appears only once in the list
                space_region_names.insert(region_name, (ch.space, name));
            }

            // Build the final list with proper sorting
            // Order: 地上波 (terrestrial by region) -> BS -> CS
            // Tuple: (actual_space, display_name, region_key)
            let mut terrestrial_spaces: Vec<(u32, String, String)> = Vec::new();
            let mut bs_space: Option<(u32, String, String)> = None;
            let mut cs_space: Option<(u32, String, String)> = None;
            
            for (region, (space, name)) in space_region_names {
                if region == "BS" {
                    bs_space = Some((space, name, region));
                } else if region == "CS" {
                    cs_space = Some((space, name, region));
                } else {
                    terrestrial_spaces.push((space, name, region));
                }
            }
            
            // Sort terrestrial spaces by region key
            terrestrial_spaces.sort_by(|a, b| a.2.cmp(&b.2));
            
            // Build final list: terrestrial first, then BS, then CS
            let mut list: Vec<(u32, String, String)> = terrestrial_spaces;
            if let Some(bs) = bs_space {
                list.push(bs);
            }
            if let Some(cs) = cs_space {
                list.push(cs);
            }
            debug!("[Session {}] ensure_space_list: final spaces for group {}: {:?}", 
                self.id, self.current_group_name.as_ref().unwrap_or(&"unknown".to_string()), list);
            self.space_list_cache.insert(cache_key.clone(), list.clone());
            
            // Also cache the NID+TSID mappings
            let mut group_mappings = HashMap::new();
            for (nid_tsid, mappings) in nid_tsid_mappings {
                group_mappings.insert(nid_tsid, mappings);
            }
            self.virtual_channel_mappings.insert(cache_key, group_mappings);
            
            return list.iter().map(|(actual_space, _, _)| *actual_space).collect();
        }

        // Single tuner mode
        let tuner_path = self.current_or_default_tuner_path();
        if tuner_path.is_empty() {
            debug!("[Session {}] ensure_space_list: tuner_path is empty", self.id);
            return Vec::new();
        }
        if let Some(v) = self.space_list_cache.get(&tuner_path) {
            trace!("[Session {}] ensure_space_list: using cache for {} (spaces: {:?})", self.id, tuner_path, v);
            return v.iter().map(|(actual_space, _, _)| *actual_space).collect();
        }

        let db = self.database.lock().await;
        let all = match db.get_all_channels_with_drivers() {
            Ok(v) => v,
            Err(e) => {
                debug!("[Session {}] ensure_space_list: failed to get channels: {}", self.id, e);
                Vec::new()
            },
        };

        // Build unique (space, region) pairs based on NID + TSID to eliminate duplicates
        // But record ALL mappings (driver, space, channel) for each NID+TSID combination
        let mut nid_tsid_seen: BTreeSet<(u16, u16)> = BTreeSet::new();
        let mut region_seen: BTreeSet<String> = BTreeSet::new();  // For BS/CS deduplication
        let mut space_region_names: HashMap<String, (u32, String)> = HashMap::new();  // region_name -> (space, name)
        let mut nid_tsid_mappings: HashMap<(u16, u16), Vec<VirtualChannelMapping>> = HashMap::new();
        
        for (ch, bd_opt) in all {
            let Some(bd) = bd_opt else { continue; };
            if bd.dll_path != tuner_path { continue; }
            if !ch.is_enabled { continue; }
            
            let nid_tsid = (ch.nid as u16, ch.tsid as u16);
            
            // Record this mapping for this NID+TSID (allow multiples)
            nid_tsid_mappings
                .entry(nid_tsid)
                .or_insert_with(Vec::new)
                .push(VirtualChannelMapping {
                    driver_path: bd.dll_path.clone(),
                    actual_space: ch.space,
                    actual_channel: ch.channel as u32,
                });
            
            // For display purposes, only register once per NID+TSID
            if nid_tsid_seen.contains(&nid_tsid) {
                continue;
            }
            nid_tsid_seen.insert(nid_tsid);
            
            // Get region name: TerrestrialRegion display_name for terrestrial (広域圏), "BS"/"CS" for satellite
            let (btype, terrestrial_region) = classify_nid(ch.nid as u16);
            let is_terrestrial = matches!(btype, recisdb_protocol::types::BroadcastType::Terrestrial)
                && terrestrial_region.as_ref().map_or(false, |r| !matches!(r, TerrestrialRegion::Unknown(_)));
            let region_name = match btype {
                recisdb_protocol::types::BroadcastType::BS => "BS".to_string(),
                recisdb_protocol::types::BroadcastType::CS => "CS".to_string(),
                recisdb_protocol::types::BroadcastType::Terrestrial => {
                    terrestrial_region.as_ref().map(|r| match r {
                        TerrestrialRegion::Unknown(_) => "Unknown".to_string(),
                        _ => r.display_name().to_string(),
                    }).unwrap_or_else(|| "Unknown".to_string())
                }
            };
            debug!("[Session {}] NID=0x{:04X} btype={:?} region={}", 
                self.id, ch.nid, btype, region_name);
            
            // For all regions, only register once per region name (prevent duplicates)
            // This applies to both BS/CS and terrestrial
            if region_seen.contains(&region_name) {
                debug!("[Session {}] Skipping duplicate region: {}", self.id, region_name);
                continue;
            }
            region_seen.insert(region_name.clone());
            
            // Build display name based on region
            let name = if is_terrestrial {
                format!("地デジ ({})", region_name)
            } else {
                region_name.clone()
            };
            
            space_region_names.insert(region_name, (ch.space, name));
        }

        // Build the final list with proper sorting
        // Order: 地上波 (terrestrial by region) -> BS -> CS
        // Tuple: (actual_space, display_name, region_key)
        let mut terrestrial_spaces: Vec<(u32, String, String)> = Vec::new();
        let mut bs_space: Option<(u32, String, String)> = None;
        let mut cs_space: Option<(u32, String, String)> = None;
        
        for (region, (space, name)) in space_region_names {
            if region == "BS" {
                bs_space = Some((space, name, region));
            } else if region == "CS" {
                cs_space = Some((space, name, region));
            } else {
                terrestrial_spaces.push((space, name, region));
            }
        }
        
        // Sort terrestrial spaces by region key
        terrestrial_spaces.sort_by(|a, b| a.2.cmp(&b.2));
        
        // Build final list: terrestrial first, then BS, then CS
        let mut list: Vec<(u32, String, String)> = terrestrial_spaces;
        if let Some(bs) = bs_space {
            list.push(bs);
        }
        if let Some(cs) = cs_space {
            list.push(cs);
        }

        debug!("[Session {}] ensure_space_list: final spaces for {}: {:?}", self.id, tuner_path, list);
        
        // Cache both space list and NID+TSID mappings
        self.space_list_cache.insert(tuner_path.clone(), list.clone());
        self.virtual_channel_mappings.insert(tuner_path, nid_tsid_mappings);
        
        list.iter().map(|(actual_space, _, _)| *actual_space).collect()
    }

    /// TVTest が渡す仮想 space_idx を、DBの実 space へ変換
    async fn map_space_idx_to_actual(&mut self, space_idx: u32) -> Option<u32> {
        let list = self.get_space_list_with_names().await;
        list.get(space_idx as usize).map(|(actual_space, _, _)| *actual_space)
    }

    /// Map virtual space index to (actual_space, region_key) for filtering.
    /// Returns the region_key (e.g., "宮城", "BS", "CS") used for channel matching,
    /// NOT the display name (which may differ, e.g., "地デジ").
    async fn map_space_idx_to_actual_with_region(&mut self, space_idx: u32) -> Option<(u32, String)> {
        let list = self.get_space_list_with_names().await;
        list.get(space_idx as usize).map(|(actual_space, _display_name, region_key)| (*actual_space, region_key.clone()))
    }

    /// Get space list with names (for internal use).
    /// Returns Vec<(actual_space, display_name, region_key)>.
    async fn get_space_list_with_names(&mut self) -> Vec<(u32, String, String)> {
        // If group is set, get spaces from all group drivers
        if !self.group_driver_paths.is_empty() {
            let cache_key = format!("group_{}", self.current_group_name.as_ref().unwrap_or(&"unknown".to_string()));
            if let Some(v) = self.space_list_cache.get(&cache_key) {
                return v.clone();
            }
            return Vec::new();
        }

        // Single tuner mode
        let tuner_path = self.current_or_default_tuner_path();
        if tuner_path.is_empty() {
            return Vec::new();
        }
        if let Some(v) = self.space_list_cache.get(&tuner_path) {
            return v.clone();
        }
        Vec::new()
    }

    /// Run the session, processing messages until disconnection.
    pub async fn run(&mut self) -> std::io::Result<()> {
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
            warn!("[Session {}] Failed to insert session history start", self.id);
        }

        // Periodic timer to detect when the tuner reader stops externally
        // (exclusive eviction, DLL crash, hardware error, etc.).
        // Without this, broadcast::Receiver::recv() blocks forever when the
        // reader dies but the SharedTuner Arc is still alive, leaving the
        // session hanging with no data and no error.
        let mut reader_alive_check = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(2),
        );
        reader_alive_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                // Create futures for socket read and TS receive
                let mut tmp_buf = [0u8; 4096];

                tokio::select! {
                    biased;

                    // Remote shutdown request
                    _ = self.shutdown_rx.recv() => {
                        self.disconnect_reason = Some("remote_shutdown".to_string());
                        break;
                    }

                    // Periodic check: is the tuner reader still alive?
                    // This catches cases where another session's exclusive eviction,
                    // a BonDriver crash, or hardware failure stopped our reader.
                    _ = reader_alive_check.tick() => {
                        if let Some(tuner) = &self.current_tuner {
                            if !tuner.is_running() {
                                warn!("[Session {}] Tuner reader for {:?} stopped externally (is_running=false), disconnecting",
                                      self.id, tuner.key);
                                self.disconnect_reason = Some("reader_stopped".to_string());
                                break;
                            }
                        }
                    }

                    // Encoded output from tsreplace
                    encoded_result = async {
                        if let Some(rx) = &mut self.tsreplace_output_rx {
                            rx.recv().await
                        } else {
                            std::future::pending::<Option<Bytes>>().await
                        }
                    } => {
                        if let Some(data) = encoded_result {
                            self.tsreplace_last_output_at = std::time::Instant::now();
                            self.send_ts_data(data).await?;
                        } else if self.tsreplace_output_rx.is_some() {
                            warn!("[Session {}] tsreplace output channel closed", self.id);
                            if self.tsreplace_passthrough_on_error {
                                self.stop_tsreplace_pipeline().await;
                            } else {
                                self.disconnect_reason = Some("tsreplace_output_closed".to_string());
                                break;
                            }
                        }
                    }

                    // Check for incoming TS data
                    ts_result = async {
                        if let Some(rx) = &mut self.ts_receiver {
                            Some(rx.recv().await)
                        } else {
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            None
                        }
                    } => {
                        match ts_result {
                            Some(Ok(data)) => {
                                if let Some(tx) = &self.tsreplace_input_tx {
                                    match tx.try_send(data.clone()) {
                                        Ok(()) => {
                                            if self.tsreplace_passthrough_on_error
                                                && self.tsreplace_last_output_at.elapsed() > self.tsreplace_read_timeout
                                            {
                                                warn!(
                                                    "[Session {}] tsreplace stalled for {:?}, fallback to raw TS",
                                                    self.id,
                                                    self.tsreplace_last_output_at.elapsed()
                                                );
                                                self.stop_tsreplace_pipeline().await;
                                                self.send_ts_data(data).await?;
                                            }
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                            warn!(
                                                "[Session {}] tsreplace input queue full; treating as stall",
                                                self.id
                                            );
                                            if self.tsreplace_passthrough_on_error {
                                                self.stop_tsreplace_pipeline().await;
                                                self.send_ts_data(data).await?;
                                            } else {
                                                self.disconnect_reason = Some("tsreplace_input_backpressure".to_string());
                                                break;
                                            }
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                            warn!("[Session {}] tsreplace input channel closed", self.id);
                                            if self.tsreplace_passthrough_on_error {
                                                self.stop_tsreplace_pipeline().await;
                                                self.send_ts_data(data).await?;
                                            } else {
                                                self.disconnect_reason = Some("tsreplace_input_closed".to_string());
                                                break;
                                            }
                                        }
                                    }
                                } else {
                                    self.send_ts_data(data).await?;
                                }
                            }
                            Some(Err(broadcast::error::RecvError::Lagged(count))) => {
                                warn!("[Session {}] Broadcast receiver lagged, skipped {} messages", self.id, count);
                                self.packets_dropped += count;
                            }
                            Some(Err(broadcast::error::RecvError::Closed)) => {
                                info!("[Session {}] Broadcast channel closed", self.id);
                                self.disconnect_reason = Some("broadcast_closed".to_string());
                                break;
                            }
                            None => {}
                        }
                    }

                    // Check for incoming socket data
                    result = self.socket.read(&mut tmp_buf) => {
                        let n = result?;
                        if n == 0 {
                            self.disconnect_reason = Some("client_disconnect".to_string());
                            break; // Connection closed
                        }
                        self.read_buf.extend_from_slice(&tmp_buf[..n]);
                    }
                }
            } else {
                // Not streaming, just wait for messages or shutdown
                let socket = &mut self.socket;
                let read_buf = &mut self.read_buf;
                let shutdown_rx = &mut self.shutdown_rx;

                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        self.disconnect_reason = Some("remote_shutdown".to_string());
                        break;
                    }
                    result = Self::read_message_with(socket, read_buf, self.id) => {
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

        // Cleanup
        self.cleanup().await;
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

                    match decode_client_message(
                        header.message_type,
                        Bytes::from(payload.to_vec()),
                    ) {
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
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
            }
        }
    }

    /// Read and decode a client message (borrowed socket/buffer).
    async fn read_message_with(
        socket: &mut TcpStream,
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
                                    error!("[Session {}] Failed to decode message: {}", session_id, e);
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
            ClientMessage::Hello { version } => {
                self.handle_hello(version).await?;
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
            ClientMessage::SetChannel { channel, priority, exclusive } => {
                self.handle_set_channel(channel, priority, exclusive).await?;
            }
            ClientMessage::SetChannelSpace { space, channel, priority, exclusive } => {
                self.handle_set_channel_space(space, channel, priority, exclusive).await?;
            }
            ClientMessage::SetChannelSpaceInGroup { group_name, space_idx, channel, priority, exclusive } => {
                // Group mode is handled by `handle_set_channel_space` via current group context.
                // Keep the explicit group open path for compatibility.
                if self.current_group_name.as_deref() != Some(group_name.as_str()) {
                    self.handle_open_tuner(group_name).await?;
                }
                self.handle_set_channel_space(space_idx, channel, priority, exclusive).await?;
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
        }
        Ok(true)
    }

    /// Handle Hello message.
    async fn handle_hello(&mut self, version: u16) -> std::io::Result<()> {
        info!(
            "[Session {}] Client hello, version {}",
            self.id, version
        );

        let success = version == PROTOCOL_VERSION;
        if success {
            self.state = SessionState::Ready;
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
                    return self
                        .send_message(ServerMessage::OpenTunerAck {
                            success: false,
                            error_code: ErrorCode::InvalidParameter.into(),
                            bondriver_version: 0,
                        })
                        .await;
                }
            }
        } else {
            tuner_path
        };

        // ★ Resolve: DLL path -> group name -> display_name -> first driver
        let (resolved_path, is_group) = {
            let db = self.database.lock().await;
            
            // 1. Try as DLL path
            if let Ok(Some(_driver)) = db.get_bon_driver_by_path(&path) {
                debug!("[Session {}] Tuner '{}' matched as DLL path", self.id, path);
                (path.clone(), false)
            } else {
                // 2. Try as group_name
                match db.get_group_drivers(&path) {
                    Ok(drivers) if !drivers.is_empty() => {
                        debug!("[Session {}] Tuner '{}' matched as group_name (drivers: {})", 
                            self.id, path, drivers.len());
                        (path.clone(), true)
                    },
                    _ => {
                        // 3. Try as display_name
                        match db.get_bon_driver_by_display_name(&path) {
                            Ok(Some(driver)) => {
                                debug!("[Session {}] Tuner '{}' resolved to DLL: {}", 
                                    self.id, path, driver.dll_path);
                                (driver.dll_path, false)
                            },
                            Ok(None) => {
                                // 4. Use first available enabled driver (prefer enabled over disabled)
                                warn!("[Session {}] Tuner '{}' not found, trying first available driver", self.id, path);
                                match db.get_all_bon_drivers() {
                                    Ok(drivers) => {
                                        // Try enabled drivers first
                                        let mut selected_driver = None;
                                        
                                        // First pass: find an enabled driver
                                        for driver in &drivers {
                                            // Check if driver appears in enabled channels
                                            // We can infer enabled status from whether it has enabled channels
                                            let has_enabled_channels = drivers.iter().any(|d| d.dll_path == driver.dll_path);
                                            if has_enabled_channels {
                                                selected_driver = Some(driver);
                                                break;
                                            }
                                        }
                                        
                                        // If no enabled driver found, use first available
                                        let first_driver = selected_driver.or_else(|| drivers.first());
                                        
                                        match first_driver {
                                            Some(driver) => {
                                                warn!("[Session {}] Using driver: {} (path: {})", 
                                                    self.id, 
                                                    driver.driver_name.as_ref().unwrap_or(&driver.dll_path), 
                                                    driver.dll_path);
                                                (driver.dll_path.clone(), false)
                                            }
                                            None => {
                                                error!("[Session {}] No drivers found in database at all", self.id);
                                                drop(db);
                                                return self
                                                    .send_message(ServerMessage::OpenTunerAck {
                                                        success: false,
                                                        error_code: ErrorCode::InvalidParameter.into(),
                                                        bondriver_version: 0,
                                                    })
                                                    .await;
                                            }
                                        }
                                    },
                                    Err(e) => {
                                        error!("[Session {}] Failed to query drivers: {}", self.id, e);
                                        drop(db);
                                        return self
                                            .send_message(ServerMessage::OpenTunerAck {
                                                success: false,
                                                error_code: ErrorCode::InvalidParameter.into(),
                                                bondriver_version: 0,
                                            })
                                            .await;
                                    }
                                }
                            },
                            Err(e) => {
                                error!("[Session {}] Database error resolving tuner: {}", self.id, e);
                                drop(db);
                                return self
                                    .send_message(ServerMessage::OpenTunerAck {
                                        success: false,
                                        error_code: ErrorCode::TunerOpenFailed.into(),
                                        bondriver_version: 0,
                                    })
                                    .await;
                            }
                        }
                    }
                }
            }
        }; // db is dropped here

        info!("[Session {}] Opening tuner: {} (group: {})", self.id, path, is_group);

        // If group, load all drivers in the group
        if is_group {
            let db = self.database.lock().await;
            match db.get_group_drivers(&path) {
                Ok(drivers) => {
                    self.group_driver_paths = drivers.iter().map(|d| d.dll_path.clone()).collect();
                    self.current_group_name = Some(path.clone());
                    info!("[Session {}] Loaded group '{}' with {} drivers: {:?}", 
                        self.id, path, self.group_driver_paths.len(), self.group_driver_paths);
                },
                Err(e) => {
                    error!("[Session {}] Failed to load group drivers: {}", self.id, e);
                    drop(db);
                    return self
                        .send_message(ServerMessage::OpenTunerAck {
                            success: false,
                            error_code: ErrorCode::TunerOpenFailed.into(),
                            bondriver_version: 0,
                        })
                        .await;
                }
            }
        } else {
            self.current_tuner_path = Some(resolved_path.clone());
            self.current_group_name = None;
            self.group_driver_paths.clear();
            self.refresh_current_bon_driver_id().await;
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
        self.session_registry.update_tuner(self.id, Some(path)).await;

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
    async fn handle_set_channel(&mut self, channel: u8, priority: i32, exclusive: bool) -> std::io::Result<()> {
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
        let _priority = effective_priority_opt.unwrap_or(priority);
        let _exclusive = effective_exclusive;

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

        // Create channel key
        let key = ChannelKey::simple(&tuner_path, channel);

        // ★ Same-channel reuse: if we already have a running tuner for this
        // exact key, just refresh the subscription without restarting.
        if let Some(ref existing) = self.current_tuner {
            if existing.key == key && existing.is_running() {
                self.tuner_pool.cancel_idle_close(&key).await;
                if self.state == SessionState::Streaming {
                    let new_rx = existing.subscribe();
                    if self.ts_receiver.is_some() {
                        existing.unsubscribe();
                    }
                    self.ts_receiver = Some(new_rx);
                }
                existing.notify_channel_change();
                self.restart_tsreplace_pipeline_if_streaming().await;
                return self.send_message(ServerMessage::SetChannelAck {
                    success: true,
                    error_code: 0,
                }).await;
            }
        }

        // ★ Check if another session already has this channel running in the pool.
        if let Some(pool_tuner) = self.tuner_pool.get(&key).await {
            if pool_tuner.is_running() {
                self.tuner_pool.cancel_idle_close(&key).await;
                self.stop_warm_tuner().await;
                let old_tuner = self.current_tuner.take();
                if let Some(old) = old_tuner {
                    if self.ts_receiver.is_some() {
                        old.unsubscribe();
                        self.ts_receiver = None;
                        if old.subscriber_count() == 0 {
                            self.tuner_pool.schedule_idle_close(old.key.clone(), old).await;
                        }
                    }
                }
                self.current_tuner = Some(pool_tuner.clone());
                if self.state == SessionState::Streaming {
                    self.ts_receiver = Some(pool_tuner.subscribe());
                }
                pool_tuner.notify_channel_change();
                self.restart_tsreplace_pipeline_if_streaming().await;
                return self.send_message(ServerMessage::SetChannelAck {
                    success: true,
                    error_code: 0,
                }).await;
            } else if !pool_tuner.has_subscribers() {
                // Stale entry — remove so get_or_create below creates a fresh one
                warn!("[Session {}] Found stale (not running) v1 tuner for {:?}, removing from pool",
                      self.id, key);
                self.tuner_pool.remove(&key).await;
            }
        }

        // ★ Clean up old tuner BEFORE creating new one (same order as v2).
        // This frees the DLL slot so the new reader can open it.
        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());
        if let Some(old_tuner) = self.current_tuner.take() {
            if self.ts_receiver.is_some() {
                old_tuner.unsubscribe();
                self.ts_receiver = None;
            }
            if old_tuner.subscriber_count() == 0 {
                info!("[Session {}] Stopping old reader for {:?} before v1 channel switch",
                      self.id, old_tuner.key);
                self.tuner_pool.cancel_idle_close(&old_tuner.key).await;
                old_tuner.stop_reader().await;
                self.tuner_pool.remove(&old_tuner.key).await;
            }
        }

        // Get or create shared tuner
        match self
            .tuner_pool
            .get_or_create(key.clone(), 2, || async { Ok(()) })
            .await
        {
            Ok(tuner) => {
                // Start the BonDriver reader
                if let Err(e) = self.start_reader_with_warm(
                    Arc::clone(&tuner),
                    tuner_path.clone(),
                    0,  // v1 style uses space=0
                    channel as u32,
                ).await {
                    if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                        warn!("[Session {}] Channel unavailable on {}: {}", self.id, tuner_path, e);
                    } else {
                        error!("[Session {}] Failed to start BonDriver reader for {}: {} (kind: {:?})", 
                               self.id, tuner_path, e, e.kind());
                    }
                    // ★ Clean up orphaned pool entry
                    if !tuner.is_running() && !tuner.has_subscribers() {
                        self.tuner_pool.remove(&key).await;
                    }
                    // ★ Try to restore previous channel
                    self.try_restore_previous_channel(&old_tuner_key).await;
                    return self.send_message(ServerMessage::SetChannelAck {
                        success: false,
                        error_code: ErrorCode::ChannelSetFailed.into(),
                    }).await;
                }

                self.current_tuner = Some(tuner.clone());
                if self.state == SessionState::Streaming {
                    self.ts_receiver = Some(tuner.subscribe());
                }

                // Notify B25 decoder about channel change
                tuner.notify_channel_change();

                self.restart_tsreplace_pipeline_if_streaming().await;
                
                self.send_message(ServerMessage::SetChannelAck {
                    success: true,
                    error_code: 0,
                })
                .await
            }
            Err(e) => {
                error!("[Session {}] Failed to set channel: {}", self.id, e);
                self.try_restore_previous_channel(&old_tuner_key).await;
                self.send_message(ServerMessage::SetChannelAck {
                    success: false,
                    error_code: ErrorCode::ChannelSetFailed.into(),
                })
                .await
            }
        }
    }

    /// Handle SetChannelSpace message (IBonDriver v2 style).
    async fn handle_set_channel_space(&mut self, space: u32, channel: u32, priority: i32, exclusive: bool) -> std::io::Result<()> {
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
        let _priority = effective_priority.unwrap_or(priority);
        let _exclusive = effective_exclusive;
        
        if self.state != SessionState::TunerOpen && self.state != SessionState::Streaming {
            error!("[Session {}] SetChannelSpace: Tuner not open (state: {:?})", self.id, self.state);
            return self.send_error(ErrorCode::InvalidState, "Tuner not open").await;
        }

        // ★space は「仮想 space_idx」なので、実 space に変換する
        let Some((actual_space, region_name)) = self.map_space_idx_to_actual_with_region(space).await else {
            error!("[Session {}] SetChannelSpace: Failed to map space_idx {} to actual space", self.id, space);
            return self.send_message(ServerMessage::SetChannelSpaceAck {
                success: false,
                error_code: ErrorCode::InvalidParameter.into(),
            }).await;
        };

        // Get region-filtered channel map
        let map = self.ensure_channel_map_with_region(actual_space, &region_name).await;
        debug!("[Session {}] SetChannelSpace: Checking channel map for space {} (region: {}): {} channels total", 
               self.id, actual_space, region_name, map.len());
        
        let Some(entry) = map.get(channel as usize) else {
            error!("[Session {}] SetChannelSpace: Channel index {} not found in space {} region {} (map size: {})", 
                   self.id, channel, actual_space, region_name, map.len());
            return self.send_message(ServerMessage::SetChannelSpaceAck {
                success: false,
                error_code: ErrorCode::InvalidParameter.into(),
            }).await;
        };

        // ★ In group mode, find which driver has this channel (matching by NID+TSID)
        // NID+TSID matching allows different BonDrivers to use different bon_channel values
        // for the same logical channel (same NID+TSID).
        // Collect all (driver_path, ChannelKeySpec) for this NID+TSID across group drivers
        // so that same-channel reuse check can work across different bon_channel values.
        let mut nid_tsid_channel_keys: Vec<(String, ChannelKeySpec)> = Vec::new();

        // ★ Capture the current session's tuner key BEFORE driver selection.
        // If this session is the sole subscriber, its slot will be freed during
        // channel switch, so it should NOT count against driver capacity.
        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());
        let old_tuner_will_free_slot = self.current_tuner.as_ref()
            .map(|t| {
                let sub_count = t.subscriber_count();
                // Streaming: sole broadcast subscriber → slot freed after unsubscribe
                (sub_count == 1 && self.ts_receiver.is_some()) ||
                // TunerOpen: no broadcast subscription yet → slot freed immediately
                (sub_count == 0 && self.ts_receiver.is_none())
            })
            .unwrap_or(false);

        let (tuner_path, actual_space, actual_bon_channel) = if !self.group_driver_paths.is_empty() {
            // Group mode: find the driver that has this NID+TSID AND has available capacity
            debug!("[Session {}] SetChannelSpace: In group mode, searching for NID=0x{:04X} TSID=0x{:04X}", 
                   self.id, entry.nid, entry.tsid);
            
            // Query all channels and find which drivers have this NID+TSID
            let db = self.database.lock().await;
            let mut candidate_drivers: Vec<(String, u32, u32)> = Vec::new();  // (driver_path, actual_space, bon_channel)

            match db.get_all_channels_with_drivers() {
                Ok(all_channels) => {
                    for (ch, bd_opt) in all_channels {
                        let Some(bd) = bd_opt else { continue; };
                        
                        // Check if this driver is in the group
                        if !self.group_driver_paths.contains(&bd.dll_path) {
                            continue;
                        }
                        
                        // Match by NID+TSID (this correctly handles different bon_channel values across drivers)
                        if ch.nid as u16 == entry.nid && ch.tsid as u16 == entry.tsid && ch.is_enabled {
                            candidate_drivers.push((bd.dll_path.clone(), ch.space, ch.channel));
                            debug!("[Session {}] Found NID+TSID match in driver {} (space {}, ch {})", 
                                self.id, bd.dll_path, ch.space, ch.channel);
                        }
                    }
                }
                Err(e) => {
                    error!("[Session {}] Failed to query channels: {}", self.id, e);
                }
            }

            // Sort candidate drivers by quality score (descending)
            if !candidate_drivers.is_empty() {
                let mut score_map: HashMap<String, f64> = HashMap::new();
                for (driver_path, _, _) in candidate_drivers.iter() {
                    if score_map.contains_key(driver_path) {
                        continue;
                    }
                    let score = db.get_driver_quality_score_by_path(driver_path).unwrap_or(1.0);
                    score_map.insert(driver_path.clone(), score);
                }
                candidate_drivers.sort_by(|a, b| {
                    let score_a = score_map.get(&a.0).copied().unwrap_or(1.0);
                    let score_b = score_map.get(&b.0).copied().unwrap_or(1.0);
                    score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
                });
            }

            // Build NID+TSID → ChannelKey mapping for same-channel reuse across drivers
            for (dp, ds, dc) in &candidate_drivers {
                nid_tsid_channel_keys.push((
                    dp.clone(),
                    ChannelKeySpec::SpaceChannel { space: *ds, channel: *dc },
                ));
            }

            // Now select the driver with available capacity
            // Priority: 1) Driver already streaming this channel, 2) Driver with available capacity
            let mut selected_driver: Option<(String, u32, u32)> = None;
            let keys = self.tuner_pool.keys().await;
            
            // First, check if any driver is already streaming this channel (by its own space+bon_channel)
            for (driver_path, driver_space, driver_bon_channel) in candidate_drivers.iter() {
                let new_channel_key = ChannelKeySpec::SpaceChannel { 
                    space: *driver_space, 
                    channel: *driver_bon_channel 
                };
                for k in keys.iter() {
                    if k.tuner_path == *driver_path && k.channel == new_channel_key {
                        if let Some(tuner) = self.tuner_pool.get(&k).await {
                            if tuner.is_running() {
                                selected_driver = Some((driver_path.clone(), *driver_space, *driver_bon_channel));
                                debug!("[Session {}] Selected driver (already streaming this channel): {} (space {}, ch {})", 
                                       self.id, driver_path, driver_space, driver_bon_channel);
                                break;
                            }
                        }
                    }
                }
                if selected_driver.is_some() {
                    break;
                }
            }

            // If not found, select driver with available capacity
            if selected_driver.is_none() {
                for (driver_path, driver_space, driver_bon_channel) in candidate_drivers.iter() {
                    // Count current instances on this driver
                    let mut driver_instances = 0i32;
                    for k in keys.iter() {
                        if k.tuner_path == *driver_path {
                            // Skip the current session's own tuner if it will be freed
                            // during channel switch (sole subscriber → slot released).
                            if old_tuner_will_free_slot && old_tuner_key.as_ref() == Some(k) {
                                continue;
                            }
                            if let Some(tuner) = self.tuner_pool.get(&k).await {
                                if tuner.is_running() {
                                    driver_instances += 1;
                                }
                            }
                        }
                    }
                    
                    // Get max_instances for this driver
                    let max_instances = db.get_max_instances_for_path(driver_path).unwrap_or(1);
                    
                    debug!("[Session {}] Driver {} has {}/{} instances", 
                           self.id, driver_path, driver_instances, max_instances);
                    
                    // Prefer driver with available capacity
                    if driver_instances < max_instances {
                        selected_driver = Some((driver_path.clone(), *driver_space, *driver_bon_channel));
                        debug!("[Session {}] Selected driver (with capacity): {} (space {}, ch {})", 
                            self.id, driver_path, driver_space, driver_bon_channel);
                        break;
                    }
                }
            }

            // If no driver with capacity, use first candidate (will fail at capacity check)
            if selected_driver.is_none() && !candidate_drivers.is_empty() {
                selected_driver = Some(candidate_drivers[0].clone());
                debug!("[Session {}] Selected driver (all full, will check priority): {} (space {}, ch {})", 
                       self.id, selected_driver.as_ref().unwrap().0, 
                       selected_driver.as_ref().unwrap().1,
                       selected_driver.as_ref().unwrap().2);
            }

            drop(db); // Release database lock

            // Use the selected driver's space and bon_channel
            match selected_driver {
                Some((path, driver_space, driver_bon_channel)) => {
                    debug!("[Session {}] Final selected driver for channel: {} (space {}, ch {})", 
                        self.id, path, driver_space, driver_bon_channel);
                    self.current_tuner_path = Some(path.clone());
                    self.refresh_current_bon_driver_id().await;
                    (path, driver_space, driver_bon_channel)
                }
                None => {
                    error!("[Session {}] SetChannelSpace: Channel NID=0x{:04X} TSID=0x{:04X} not found in any group driver", 
                        self.id, entry.nid, entry.tsid);
                    return self.send_message(ServerMessage::SetChannelSpaceAck {
                        success: false,
                        error_code: ErrorCode::InvalidParameter.into(),
                    }).await;
                }
            }
        } else {
            // Single tuner mode
            match &self.current_tuner_path {
                Some(p) => (p.clone(), actual_space, entry.bon_channel),
                None => {
                    error!("[Session {}] SetChannelSpace: current_tuner_path is None", self.id);
                    return self.send_message(ServerMessage::SetChannelSpaceAck {
                        success: false,
                        error_code: ErrorCode::InvalidState.into(),
                    }).await;
                }
            }
        };

        info!(
            "[Session {}] SetChannelSpace: space_idx={}, actual_space={}, idx={} -> bon_channel={} (NID=0x{:04X} TSID=0x{:04X}) on {} (priority={}, exclusive={})",
            self.id, space, actual_space, channel, actual_bon_channel, entry.nid, entry.tsid, tuner_path, priority, exclusive
        );

        // ★ Use client-provided priority, or database default if priority <= 0
        let channel_priority = if priority > 0 {
            priority
        } else {
            // If exclusive is requested, use maximum priority
            if exclusive {
                i32::MAX
            } else {
                // Use database default
                let db = self.database.lock().await;
                db.get_channel_priority(&tuner_path, actual_space, actual_bon_channel)
                    .unwrap_or(Some(0))
                    .unwrap_or(0)
            }
        };

        // ★ If exclusive is requested, kick off all other tuners on this BonDriver
        if exclusive {
            info!("[Session {}] Exclusive access requested - forcing all other tuners off", self.id);
            let keys = self.tuner_pool.keys().await;
            for existing_key in keys.iter() {
                if existing_key.tuner_path == tuner_path {
                    // NOTE: We intentionally do NOT skip our own old tuner here.
                    // On a max_instances=1 DLL where another session shares the same
                    // SharedTuner (subscriber_count > 1), skipping would leave the
                    // tuner running.  The old-tuner-cleanup below can only unsubscribe;
                    // it cannot stop a tuner that still has other subscribers.  The
                    // capacity check would then find the DLL at capacity and fail —
                    // despite exclusive being requested.
                    // stop_reader() is idempotent (second call is a no-op), so the
                    // old-tuner-cleanup section safely handles the already-stopped
                    // tuner via the !is_running() guard.
                    if let Some(existing_tuner) = self.tuner_pool.get(&existing_key).await {
                        if existing_tuner.is_running() {
                            let subs = existing_tuner.subscriber_count();
                            if subs > 0 {
                                warn!("[Session {}] Exclusive: stopping tuner {:?} that still has {} active subscriber(s) — those sessions will lose data",
                                      self.id, existing_key, subs);
                            }
                            info!("[Session {}] Stopping existing tuner {:?} for exclusive access", self.id, existing_key);
                            self.tuner_pool.cancel_idle_close(existing_key).await;
                            existing_tuner.stop_reader().await;
                            // ★ Remove the stopped entry from the pool so it doesn't
                            // linger as a ghost (is_running=false). Other sessions'
                            // event loops will detect the reader stoppage via the
                            // periodic is_running check and disconnect cleanly.
                            self.tuner_pool.remove(existing_key).await;
                        }
                    }
                }
            }
        }

        // ★ Check if requesting a channel that's already running (same NID+TSID, any driver in group)
        let keys = self.tuner_pool.keys().await;
        let new_key = ChannelKey::space_channel(&tuner_path, actual_space, actual_bon_channel);
        
        // First pass: check for same channel running on ANY driver
        // In group mode, we use NID+TSID-aware matching via nid_tsid_channel_keys
        // to handle different bon_channel values across drivers for the same logical channel.
        for existing_key in keys.iter() {
            // Determine if this existing tuner is streaming the same logical channel
            let is_same_channel = if !nid_tsid_channel_keys.is_empty() {
                // Group mode: check if existing key matches ANY candidate for this NID+TSID
                // This correctly handles different bon_channel values across drivers
                nid_tsid_channel_keys.iter().any(|(path, spec)|
                    existing_key.tuner_path == *path && existing_key.channel == *spec
                )
            } else {
                // Single tuner mode: exact ChannelKeySpec match on same driver
                existing_key.channel == new_key.channel && existing_key.tuner_path == tuner_path
            };

            if is_same_channel {
                if let Some(existing_tuner) = self.tuner_pool.get(&existing_key).await {
                    if !existing_tuner.is_running() {
                        // ★ Stale entry: reader stopped (e.g. by idle-close race).
                        // Remove it so get_or_create below will create a fresh SharedTuner
                        // with a new reader instead of returning this dead entry.
                        warn!("[Session {}] Found stale (not running) tuner for {:?}, removing from pool",
                              self.id, existing_key);
                        self.tuner_pool.remove(&existing_key).await;
                    } else {
                        info!("[Session {}] Same channel already running on driver {}, reusing existing tuner", 
                              self.id, existing_key.tuner_path);

                        // ★ Cancel any pending idle close FIRST, before anything else.
                        // This prevents a race where the idle timer fires between
                        // SetChannelSpaceAck and the subsequent StartStream subscribe.
                        self.tuner_pool.cancel_idle_close(&existing_key).await;

                        // ★ Shut down the warm tuner opened during handle_open_tuner.
                        // We are reusing an existing reader, so the warm tuner will
                        // never be activated.  Keeping it open holds an extra DLL
                        // handle that can interfere with the running stream on some
                        // BonDriver implementations.
                        self.stop_warm_tuner().await;

                        // Track the actual physical tuner path currently used.
                        self.current_tuner_path = Some(existing_key.tuner_path.clone());
                        self.refresh_current_bon_driver_id().await;
                        self.session_registry
                            .update_tuner(self.id, Some(existing_key.tuner_path.clone()))
                            .await;

                        // Unsubscribe from old tuner if we had one,
                        // BUT skip the cycle when old tuner IS the same SharedTuner
                        // (solo re-tune: unsubscribe would drop count to 0 → stop reader).
                        let old_tuner = self.current_tuner.take();
                        if let Some(old) = old_tuner {
                            let same_tuner = Arc::ptr_eq(&old, &existing_tuner);
                            if same_tuner {
                                // Same tuner re-tune: keep subscription as-is, just refresh ts_receiver
                                debug!("[Session {}] Re-tune to same channel, keeping existing subscription", self.id);
                                if self.state == SessionState::Streaming {
                                    // Re-subscribe FIRST (count N→N+1), then unsubscribe old (count N+1→N).
                                    // This order avoids a transient subscriber_count==0 which would
                                    // erroneously trigger idle close on this still-active tuner.
                                    let new_rx = existing_tuner.subscribe();
                                    self.ts_receiver = Some(new_rx);
                                    old.unsubscribe();
                                }
                                self.current_tuner = Some(existing_tuner.clone());
                            } else {
                                // Different tuner: unsubscribe from old, subscribe to existing
                                if self.ts_receiver.is_some() {
                                    old.unsubscribe();
                                    self.ts_receiver = None;
                                    debug!("[Session {}] Unsubscribed from old tuner", self.id);
                                    if old.subscriber_count() == 0 {
                                        // Don't await stop_reader inline; schedule idle close instead
                                        // so we don't block the reuse path for 1+ seconds.
                                        self.tuner_pool.schedule_idle_close(old.key.clone(), old).await;
                                    }
                                }
                                if self.state == SessionState::Streaming {
                                    self.ts_receiver = Some(existing_tuner.subscribe());
                                }
                                self.current_tuner = Some(existing_tuner.clone());
                            }
                        } else {
                            // No old tuner (first channel selection)
                            if self.state == SessionState::Streaming {
                                self.ts_receiver = Some(existing_tuner.subscribe());
                            }
                            self.current_tuner = Some(existing_tuner.clone());
                        }

                        // Update session registry with channel info and name
                        let channel_info = format!("Space {}, Ch {}", actual_space, actual_bon_channel);
                        self.session_registry.update_channel(self.id, Some(channel_info.clone())).await;
                        self.current_channel_info = Some(channel_info);

                        // Try to get channel name and NID/SID from database
                        let (channel_name, ch_nid, ch_sid) = {
                            let db = self.database.lock().await;
                            match db.get_channel_by_physical(&existing_key.tuner_path, actual_space, actual_bon_channel) {
                                Ok(Some(rec)) => (
                                    rec.channel_name.or(rec.raw_name),
                                    Some(rec.nid),
                                    Some(rec.sid),
                                ),
                                _ => (None, None, None),
                            }
                        };
                        self.session_registry.update_channel_name(self.id, channel_name.clone()).await;
                        self.session_registry.update_channel_ids(self.id, ch_nid, ch_sid).await;
                        self.current_channel_name = channel_name;

                        return self.send_message(ServerMessage::SetChannelSpaceAck { success: true, error_code: 0 }).await;
                    } // end else (is_running)
                }
            }
        }

        // ★ If this session has an active tuner, properly unsubscribe
        // Don't stop the tuner immediately - let it stop naturally when last subscriber unsubscribes
        let old_tuner = self.current_tuner.take();
        
        if let Some(tuner) = old_tuner {
            if self.ts_receiver.is_some() {
                // Unsubscribe from the old tuner
                tuner.unsubscribe();
                self.ts_receiver = None;
                debug!("[Session {}] Unsubscribed from old tuner, remaining subscribers: {}", 
                       self.id, tuner.subscriber_count());
                
                // If no more subscribers, handle cleanup.
                if tuner.subscriber_count() == 0 {
                    if !tuner.is_running() {
                        // Already stopped (e.g. by exclusive pre-start, another
                        // session's eviction, or hardware failure).  Just make sure
                        // the pool entry is removed (no-op if already gone).
                        // Do NOT call schedule_idle_close — it would pointlessly
                        // spawn a timer task for a dead tuner.
                        debug!("[Session {}] Old tuner {:?} already stopped, ensuring pool cleanup",
                               self.id, tuner.key);
                        self.tuner_pool.remove(&tuner.key).await;
                    } else if tuner.key.tuner_path == tuner_path {
                        // Same DLL channel switch.  Whether we must stop the old
                        // reader depends on whether the DLL supports multiple
                        // concurrent instances.
                        let old_dll_max = {
                            let db = self.database.lock().await;
                            db.get_max_instances_for_path(&tuner.key.tuner_path).unwrap_or(1)
                        };
                        // Count how many tuners are currently running on this DLL
                        let old_dll_running = {
                            let ks = self.tuner_pool.keys().await;
                            let mut n = 0i32;
                            for k in &ks {
                                if k.tuner_path == tuner.key.tuner_path {
                                    if let Some(t) = self.tuner_pool.get(k).await {
                                        if t.is_running() { n += 1; }
                                    }
                                }
                            }
                            n
                        };
                        if old_dll_running >= old_dll_max {
                            // At or over capacity — must stop one to make room.
                            info!("[Session {}] Same DLL switch (max_instances={}), stopping old reader for {:?}",
                                  self.id, old_dll_max, tuner.key);
                            tuner.stop_reader().await;
                            self.tuner_pool.remove(&tuner.key).await;
                        } else {
                            // DLL has spare capacity — old tuner can idle-close later.
                            info!("[Session {}] Same DLL switch (max_instances={}, running={}), scheduling idle close for {:?}",
                                  self.id, old_dll_max, old_dll_running, tuner.key);
                            self.tuner_pool.schedule_idle_close(tuner.key.clone(), tuner).await;
                        }
                    } else {
                        // Different DLL switch.  Check whether the old DLL is at
                        // capacity.  If so, stop synchronously to free the slot —
                        // some hardware (e.g. multi-tuner USB cards) cannot have
                        // multiple group DLLs open simultaneously beyond their
                        // max_instances, and the new DLL's OpenTuner would fail
                        // if the old one is still held.
                        let old_dll_max = {
                            let db = self.database.lock().await;
                            db.get_max_instances_for_path(&tuner.key.tuner_path).unwrap_or(1)
                        };
                        let old_dll_running = {
                            let ks = self.tuner_pool.keys().await;
                            let mut n = 0i32;
                            for k in &ks {
                                if k.tuner_path == tuner.key.tuner_path {
                                    if let Some(t) = self.tuner_pool.get(k).await {
                                        if t.is_running() { n += 1; }
                                    }
                                }
                            }
                            n
                        };
                        if old_dll_running >= old_dll_max {
                            info!("[Session {}] Different DLL switch (old DLL at capacity {}/{}), stopping old reader for {:?}",
                                  self.id, old_dll_running, old_dll_max, tuner.key);
                            tuner.stop_reader().await;
                            self.tuner_pool.remove(&tuner.key).await;
                        } else {
                            info!("[Session {}] Different DLL switch (old DLL has spare capacity {}/{}), scheduling idle close for {:?}",
                                  self.id, old_dll_running, old_dll_max, tuner.key);
                            self.tuner_pool.schedule_idle_close(tuner.key.clone(), tuner).await;
                        }
                    }
                }
            }
        }
        
        // Note: current_tuner is now None, cleared by .take() above

        // ★ Get the group name and max instances for this driver
        let driver_info = {
            let db = self.database.lock().await;
            match db.get_bon_driver_by_path(&tuner_path) {
                Ok(Some(driver)) => (driver.group_name.clone(), driver.max_instances),
                _ => (None, 1),
            }
        };
        let (group_name, max_instances) = driver_info;
        
        // Store candidate drivers for fallback in case the primary driver fails
        // Rebuild the list from the database using NID+TSID matching (not bon_channel)
        let fallback_candidates: Vec<(String, u32, u32)> = if !self.group_driver_paths.is_empty() {
            // In group mode, find all group drivers that have this NID+TSID
            let db = self.database.lock().await;
            let all_channels = db.get_all_channels_with_drivers().unwrap_or_default();
            let mut candidates: Vec<(String, u32, u32)> = Vec::new();  // (driver_path, space, bon_channel)
            
            for (ch, bd_opt) in &all_channels {
                let Some(bd) = bd_opt else { continue; };
                if !self.group_driver_paths.contains(&bd.dll_path) {
                    continue;
                }
                // Match by NID+TSID so each driver gets its own correct bon_channel
                if ch.nid as u16 == entry.nid && ch.tsid as u16 == entry.tsid && ch.is_enabled {
                    candidates.push((bd.dll_path.clone(), ch.space, ch.channel));
                }
            }
            candidates
        } else {
            vec![]
        };

        // ★ Re-take fresh keys snapshot for capacity check
        // (The previous `keys` was obtained before old tuner unsubscribe/stop,
        //  and other sessions may have modified the pool since then)
        let keys = self.tuner_pool.keys().await;

        // ★ Count current running instances
        // In group mode, count only instances of the SELECTED driver (not all group drivers)
        // In standalone mode, count only this driver's instances
        let mut current_instances = 0i32;
        
        if let Some(group) = &group_name {
            // Group mode: count instances of the SELECTED driver only
            // Each driver in the group has its own max_instances limit
            info!("[Session {}] BonDriver group '{}', counting instances for driver: {}", 
                  self.id, group, tuner_path);
            
            // Count instances from only the selected driver
            for k in keys.iter() {
                if k.tuner_path == tuner_path {
                    if let Some(tuner) = self.tuner_pool.get(&k).await {
                        if tuner.is_running() {
                            current_instances += 1;
                            debug!("[Session {}] Found running instance for driver: {}", self.id, k.tuner_path);
                        }
                    }
                }
            }
        } else {
            // Standalone driver: count only this driver's instances
            for k in keys.iter() {
                if k.tuner_path == tuner_path {
                    if let Some(tuner) = self.tuner_pool.get(&k).await {
                        if tuner.is_running() {
                            current_instances += 1;
                        }
                    }
                }
            }
        }

        // ★ Check if we're at capacity
        if current_instances >= max_instances {
            // At capacity - find lowest priority channel and force it off if new priority is higher
            info!("[Session {}] Driver '{}' at capacity ({}/{} instances), checking priority-based forcing",
                  self.id, 
                  tuner_path,
                  current_instances, max_instances);

            let mut lowest_priority_key: Option<ChannelKey> = None;
            let mut lowest_priority_value = i32::MAX;

            // Check only this driver's instances (even in group mode)
            // Each driver has its own max_instances limit
            for existing_key in keys.iter() {
                if existing_key.tuner_path == tuner_path {
                    // ★ Bug B fix: skip channels with active subscribers — stopping them
                    // would cut off clients that are already streaming on that channel.
                    // Only TunerOpen-state (subscriber-less) channels are eligible for eviction.
                    if let Some(candidate) = self.tuner_pool.get(existing_key).await {
                        if candidate.has_subscribers() {
                            debug!("[Session {}] Skipping {:?} for priority eviction: has {} active subscriber(s)",
                                   self.id, existing_key, candidate.subscriber_count());
                            continue;
                        }
                    }

                    let (existing_space, existing_channel) = match &existing_key.channel {
                        ChannelKeySpec::SpaceChannel { space, channel } => (*space, *channel),
                        ChannelKeySpec::Simple(ch) => (0, *ch as u32),
                    };

                    let existing_priority = {
                        let db = self.database.lock().await;
                        db.get_channel_priority(&existing_key.tuner_path, existing_space, existing_channel)
                            .unwrap_or(Some(0))
                            .unwrap_or(0)
                    };

                    // Find the lowest priority channel on this driver
                    if existing_priority < lowest_priority_value {
                        lowest_priority_value = existing_priority;
                        lowest_priority_key = Some(existing_key.clone());
                    }
                }
            }

            // If new priority is higher than the lowest, force the change
            if channel_priority > lowest_priority_value {
                if let Some(lowest_key) = lowest_priority_key {
                    if let Some(lowest_tuner) = self.tuner_pool.get(&lowest_key).await {
                        info!("[Session {}] Forcing lower priority channel (priority {}) to make room for new channel (priority {})",
                              self.id, lowest_priority_value, channel_priority);
                        self.tuner_pool.cancel_idle_close(&lowest_key).await;
                        lowest_tuner.stop_reader().await;
                        
                        // Wait for reader to stop
                        let mut wait_attempts = 0;
                        while lowest_tuner.is_running() && wait_attempts < 50 {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            wait_attempts += 1;
                        }

                        // ★ Remove the stopped entry from the pool so it doesn't
                        // linger as a ghost (is_running=false). Without this, the
                        // stale entry inflates the capacity count and blocks future
                        // channel selections on this DLL. The evicted session's
                        // event loop will detect the reader stoppage via the
                        // periodic reader_alive_check and disconnect cleanly.
                        self.tuner_pool.remove(&lowest_key).await;
                    }
                }
            } else {
                // New priority is not higher on the selected driver.
                // In group mode, try other drivers that may have capacity.
                warn!("[Session {}] Driver {} at capacity and priority {} not higher than lowest {}; trying fallback drivers",
                      self.id, tuner_path, channel_priority, lowest_priority_value);
                if let Some((fb_tuner, fb_path)) = self.try_fallback_drivers(&fallback_candidates, &[&tuner_path]).await {
                    self.current_tuner_path = Some(fb_path.clone());
                    self.refresh_current_bon_driver_id().await;
                    self.session_registry.update_tuner(self.id, Some(fb_path.clone())).await;
                    self.current_tuner = Some(fb_tuner.clone());
                    if self.state == SessionState::Streaming {
                        self.ts_receiver = Some(fb_tuner.subscribe());
                    }
                    self.restart_tsreplace_pipeline_if_streaming().await;

                    let channel_info = format!("Space {}, Ch {}", actual_space, actual_bon_channel);
                    self.session_registry.update_channel(self.id, Some(channel_info.clone())).await;
                    self.current_channel_info = Some(channel_info);
                    let (fb_ch_name, fb_nid, fb_sid) = {
                        let db = self.database.lock().await;
                        match db.get_channel_by_physical(&fb_path, actual_space, actual_bon_channel) {
                            Ok(Some(rec)) => (rec.channel_name.or(rec.raw_name), Some(rec.nid), Some(rec.sid)),
                            _ => (None, None, None),
                        }
                    };
                    self.session_registry.update_channel_name(self.id, fb_ch_name.clone()).await;
                    self.session_registry.update_channel_ids(self.id, fb_nid, fb_sid).await;
                    self.current_channel_name = fb_ch_name;
                    return self.send_message(ServerMessage::SetChannelSpaceAck { success: true, error_code: 0 }).await;
                }
                error!("[Session {}] Cannot switch: all drivers at capacity and priority insufficient",
                       self.id);
                self.try_restore_previous_channel(&old_tuner_key).await;
                return self.send_message(ServerMessage::SetChannelSpaceAck {
                    success: false,
                    error_code: ErrorCode::ChannelSetFailed.into(),
                }).await;
            }
        }

        // ★ No existing tuner found - create new one
        // In group mode, if the primary driver fails, try fallback candidates
        let mut key = ChannelKey::space_channel(&tuner_path, actual_space, actual_bon_channel);

        info!("[Session {}] Creating new tuner for key: {:?}", self.id, key);

        // Try primary driver
        let mut tuner_result = self.tuner_pool.get_or_create(key.clone(), 2, || async { Ok(()) }).await;
        let mut actual_tuner_path = tuner_path.clone();
        let mut actual_actual_space = actual_space;
        
        // If primary fails and we have fallback candidates, try them via the shared helper
        if tuner_result.is_err() && !fallback_candidates.is_empty() {
            warn!("[Session {}] Primary driver {} creation failed, trying fallback candidates", self.id, tuner_path);
            if let Some((fb_tuner, fb_path)) = self.try_fallback_drivers(&fallback_candidates, &[&tuner_path]).await {
                // Find the matching (space, bon_channel) for this fallback path
                let (fb_space, fb_bon_ch) = fallback_candidates.iter()
                    .find(|(p, _, _)| p == &fb_path)
                    .map(|(_, s, c)| (*s, *c))
                    .unwrap_or((actual_space, actual_bon_channel));
                tuner_result = Ok(fb_tuner);
                actual_tuner_path = fb_path.clone();
                actual_actual_space = fb_space;
                key = ChannelKey::space_channel(&fb_path, fb_space, fb_bon_ch);
            }
        }

        match tuner_result {
            Ok(tuner) => {
                info!("[Session {}] Tuner pool returned tuner, is_running={}", self.id, tuner.is_running());

                // Track the actual physical tuner path selected for this session.
                self.current_tuner_path = Some(actual_tuner_path.clone());
                self.refresh_current_bon_driver_id().await;
                self.session_registry
                    .update_tuner(self.id, Some(actual_tuner_path.clone()))
                    .await;
                
                // Start the BonDriver reader if not already running
                if !tuner.is_running() {
                    // ★ Safety guard: verify the same physical BonDriver is not
                    //   already at its max_instances limit before starting a new
                    //   reader.  A DLL with max_instances > 1 CAN have multiple
                    //   channels open simultaneously.
                    let guard_max = {
                        let db = self.database.lock().await;
                        db.get_max_instances_for_path(&actual_tuner_path).unwrap_or(1)
                    };
                    let guard_keys = self.tuner_pool.keys().await;
                    let mut same_dll_running = 0i32;
                    for gk in &guard_keys {
                        if gk.tuner_path == actual_tuner_path && *gk != key {
                            if let Some(other) = self.tuner_pool.get(gk).await {
                                if other.is_running() {
                                    same_dll_running += 1;
                                }
                            }
                        }
                    }
                    // +1 because we are about to start a new instance
                    let conflict_found = (same_dll_running + 1) > guard_max;
                    if conflict_found {
                        warn!(
                            "[Session {}] CONFLICT: driver {} already has {}/{} instances running, cannot start another",
                            self.id, actual_tuner_path, same_dll_running, guard_max
                        );
                    }
                    if conflict_found {
                        // The tuner entry was just created by get_or_create but will not be
                        // started (conflict). Remove it from the pool to prevent accumulation
                        // of orphaned (not-running, no-subscriber) entries.
                        if !tuner.is_running() && !tuner.has_subscribers() {
                            self.tuner_pool.remove(&key).await;
                        }
                        // Primary driver has a conflict — try fallback candidates
                        warn!("[Session {}] Primary driver {} has conflict, trying fallback candidates", self.id, actual_tuner_path);
                        if let Some((fb_tuner, fb_path)) = self.try_fallback_drivers(&fallback_candidates, &[&actual_tuner_path]).await {
                            self.current_tuner_path = Some(fb_path.clone());
                            self.refresh_current_bon_driver_id().await;
                            self.session_registry.update_tuner(self.id, Some(fb_path.clone())).await;
                            self.current_tuner = Some(fb_tuner.clone());
                            if self.state == SessionState::Streaming {
                                self.ts_receiver = Some(fb_tuner.subscribe());
                            }
                            self.restart_tsreplace_pipeline_if_streaming().await;

                            let channel_info = format!("Space {}, Ch {}", actual_space, actual_bon_channel);
                            self.session_registry.update_channel(self.id, Some(channel_info.clone())).await;
                            self.current_channel_info = Some(channel_info);
                            let (fb_ch_name, fb_nid, fb_sid) = {
                                let db = self.database.lock().await;
                                match db.get_channel_by_physical(&fb_path, actual_space, actual_bon_channel) {
                                    Ok(Some(rec)) => (rec.channel_name.or(rec.raw_name), Some(rec.nid), Some(rec.sid)),
                                    _ => (None, None, None),
                                }
                            };
                            self.session_registry.update_channel_name(self.id, fb_ch_name.clone()).await;
                            self.session_registry.update_channel_ids(self.id, fb_nid, fb_sid).await;
                            self.current_channel_name = fb_ch_name;
                            return self.send_message(ServerMessage::SetChannelSpaceAck { success: true, error_code: 0 }).await;
                        }
                        self.try_restore_previous_channel(&old_tuner_key).await;
                        return self.send_message(ServerMessage::SetChannelSpaceAck {
                            success: false,
                            error_code: ErrorCode::ChannelSetFailed.into(),
                        }).await;
                    }

                    info!("[Session {}] Starting BonDriver reader for new tuner", self.id);
                    if let Err(e) = self.start_reader_with_warm(
                        Arc::clone(&tuner),
                        actual_tuner_path.clone(),
                        actual_actual_space,
                        actual_bon_channel,
                    ).await {
                        if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                            warn!("[Session {}] Channel unavailable: {}", self.id, e);
                        } else {
                            error!("[Session {}] Failed to start BonDriver reader: {}", self.id, e);
                        }
                        // Try fallback drivers
                        if let Some((fb_tuner, fb_path)) = self.try_fallback_drivers(&fallback_candidates, &[&actual_tuner_path]).await {
                            self.current_tuner_path = Some(fb_path.clone());
                            self.refresh_current_bon_driver_id().await;
                            self.session_registry.update_tuner(self.id, Some(fb_path.clone())).await;
                            self.current_tuner = Some(fb_tuner.clone());
                            if self.state == SessionState::Streaming {
                                self.ts_receiver = Some(fb_tuner.subscribe());
                            }
                            self.restart_tsreplace_pipeline_if_streaming().await;

                            let channel_info = format!("Space {}, Ch {}", actual_space, actual_bon_channel);
                            self.session_registry.update_channel(self.id, Some(channel_info.clone())).await;
                            self.current_channel_info = Some(channel_info);
                            let (fb_ch_name, fb_nid, fb_sid) = {
                                let db = self.database.lock().await;
                                match db.get_channel_by_physical(&fb_path, actual_space, actual_bon_channel) {
                                    Ok(Some(rec)) => (rec.channel_name.or(rec.raw_name), Some(rec.nid), Some(rec.sid)),
                                    _ => (None, None, None),
                                }
                            };
                            self.session_registry.update_channel_name(self.id, fb_ch_name.clone()).await;
                            self.session_registry.update_channel_ids(self.id, fb_nid, fb_sid).await;
                            self.current_channel_name = fb_ch_name;
                            return self.send_message(ServerMessage::SetChannelSpaceAck { success: true, error_code: 0 }).await;
                        }
                        // ★ Bug D fix: get_or_create inserted this tuner into the pool but
                        // start_reader failed and all fallbacks are exhausted.  Remove the
                        // orphaned (not-running, no-subscriber) entry so it doesn't persist
                        // indefinitely and confuse future capacity/reuse checks.
                        if !tuner.is_running() && !tuner.has_subscribers() {
                            self.tuner_pool.remove(&key).await;
                        }
                        self.try_restore_previous_channel(&old_tuner_key).await;
                        return self.send_message(ServerMessage::SetChannelSpaceAck {
                            success: false,
                            error_code: ErrorCode::ChannelSetFailed.into(),
                        }).await;
                    }
                } else {
                    info!("[Session {}] BonDriver reader already running, reusing", self.id);
                }

                // ★ Exclusive post-start re-check: stop any sessions that started on this
                // DLL during the reader initialization window (up to ~10 s).  The pre-start
                // stop-loop ran before our reader was up, so there is a race where another
                // session slipped in.  Now that our reader is confirmed running we can safely
                // evict any interlopers.
                if exclusive {
                    let recheck_keys = self.tuner_pool.keys().await;
                    for rk in recheck_keys.iter() {
                        if rk.tuner_path == tuner_path && *rk != key {
                            if let Some(interloper) = self.tuner_pool.get(rk).await {
                                if interloper.is_running() {
                                    let subs = interloper.subscriber_count();
                                    if subs > 0 {
                                        warn!("[Session {}] Exclusive post-start: evicting interloper {:?} with {} active subscriber(s)",
                                              self.id, rk, subs);
                                    }
                                    info!("[Session {}] Exclusive post-start: evicting interloper {:?}", self.id, rk);
                                    self.tuner_pool.cancel_idle_close(rk).await;
                                    interloper.stop_reader().await;
                                    self.tuner_pool.remove(rk).await;
                                }
                            }
                        }
                    }
                }

                self.current_tuner = Some(tuner.clone());

                // Notify B25 decoder about channel change
                tuner.notify_channel_change();

                // If we were streaming before, re-subscribe to the new tuner
                if self.state == SessionState::Streaming {
                    info!("[Session {}] Re-subscribing to new tuner after channel switch", self.id);
                    self.ts_receiver = Some(tuner.subscribe());
                }

                self.restart_tsreplace_pipeline_if_streaming().await;

                // Update session registry with channel info and name
                let channel_info = format!("Space {}, Ch {}", actual_space, actual_bon_channel);
                self.session_registry.update_channel(self.id, Some(channel_info.clone())).await;
                self.current_channel_info = Some(channel_info);

                // Try to get channel name and NID/SID from database
                let (channel_name, ch_nid, ch_sid) = {
                    let db = self.database.lock().await;
                    match db.get_channel_by_physical(&tuner_path, actual_space, actual_bon_channel) {
                        Ok(Some(rec)) => (
                            rec.channel_name.or(rec.raw_name),
                            Some(rec.nid),
                            Some(rec.sid),
                        ),
                        _ => (None, None, None),
                    }
                };
                self.session_registry.update_channel_name(self.id, channel_name.clone()).await;
                self.session_registry.update_channel_ids(self.id, ch_nid, ch_sid).await;
                self.current_channel_name = channel_name;

                // BonDriver reader is confirmed ready by start_reader_with_warm (via ready_rx, up to 10s timeout).
                // The run() loop's select! will forward TS data as soon as this function returns.
                // Do NOT call wait_first_data here — it stalls the select! loop and causes TVTest disconnection.

                info!("[Session {}] Successfully set channel, sending SetChannelSpaceAck success=true", self.id);
                self.send_message(ServerMessage::SetChannelSpaceAck { success: true, error_code: 0 }).await
            }
            Err(e) => {
                error!("[Session {}] Failed to set channel: {}", self.id, e);
                self.try_restore_previous_channel(&old_tuner_key).await;
                self.send_message(ServerMessage::SetChannelSpaceAck {
                    success: false,
                    error_code: ErrorCode::ChannelSetFailed.into(),
                }).await
            }
        }
    }

    async fn handle_get_signal_level(&mut self) -> std::io::Result<()> {
        let signal_level = self
            .current_tuner
            .as_ref()
            .map(|t| t.signal_level())
            .unwrap_or(0.0);

        self.send_message(ServerMessage::GetSignalLevelAck { signal_level }).await
    }


    /// Handle EnumTuningSpace message.
    async fn handle_enum_tuning_space(&mut self, space: u32) -> std::io::Result<()> {
        debug!("[Session {}] EnumTuningSpace: space_idx={}", self.id, space);

        // Get space list with names
        let space_list = self.get_space_list_with_names().await;
        
        if space >= space_list.len() as u32 {
            // No more spaces, end enumeration
            return self.send_message(ServerMessage::EnumTuningSpaceAck { name: None }).await;
        }

        let (actual_space, name, _region_key) = &space_list[space as usize];

        debug!("[Session {}] EnumTuningSpace: space_idx={} actual_space={} name={:?}",
            self.id, space, actual_space, name);

        self.send_message(ServerMessage::EnumTuningSpaceAck { name: Some(name.clone()) })
            .await
    }

    /// Handle EnumChannelName message.
    async fn handle_enum_channel_name(&mut self, space: u32, channel: u32) -> std::io::Result<()> {
        debug!("[Session {}] EnumChannelName: space={}, channel={}", self.id, space, channel);

        let Some((actual_space, region_name)) = self.map_space_idx_to_actual_with_region(space).await else {
            return self.send_message(ServerMessage::EnumChannelNameAck { name: None }).await;
        };

        let map = self.ensure_channel_map_with_region(actual_space, &region_name).await;
        let name = map.get(channel as usize).map(|e| e.name.clone());

        debug!("[Session {}] EnumChannelName: space_idx={} actual_space={} region={} channel={} name={:?}",
            self.id, space, actual_space, region_name, channel, name);

        self.send_message(ServerMessage::EnumChannelNameAck { name }).await
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
        let rx = tuner.subscribe();
        self.ts_receiver = Some(rx);
        self.state = SessionState::Streaming;

        if let Err(e) = self.start_tsreplace_pipeline().await {
            if self.tsreplace_passthrough_on_error {
                warn!("[Session {}] tsreplace unavailable, fallback to raw TS: {}", self.id, e);
                self.stop_tsreplace_pipeline().await;
            } else {
                tuner.unsubscribe();
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

        // Unsubscribe from the broadcast — only if we actually have an active subscription.
        // Without this guard, a redundant StopStream (or StopStream in TunerOpen state) would
        // call unsubscribe() with no matching subscribe(), causing AtomicU32 to wrap to u32::MAX
        // and permanently disabling idle-close detection.
        if self.ts_receiver.is_some() {
            if let Some(tuner) = &self.current_tuner {
                tuner.unsubscribe();

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
        self.ts_receiver = None;
        self.stop_tsreplace_pipeline().await;
        self.state = SessionState::TunerOpen;

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

    /// Handle SelectLogicalChannel message.
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

        // Look up channel in database
        let channels = {
            let db = self.database.lock().await;
            match db.get_channels_by_nid_tsid_ordered(nid, tsid, sid) {
                Ok(chs) => chs,
                Err(e) => {
                    drop(db);
                    error!("[Session {}] Failed to query channels: {}", self.id, e);
                    return self
                        .send_message(ServerMessage::SelectLogicalChannelAck {
                            success: false,
                            error_code: ErrorCode::ChannelSetFailed.into(),
                            tuner_id: None,
                            space: None,
                            channel: None,
                        })
                        .await;
                }
            }
        };

        if channels.is_empty() {
            info!(
                "[Session {}] No channel found for nid={}, tsid={}, sid={:?}",
                self.id, nid, tsid, sid
            );
            return self
                .send_message(ServerMessage::SelectLogicalChannelAck {
                    success: false,
                    error_code: ErrorCode::ChannelSetFailed.into(),
                    tuner_id: None,
                    space: None,
                    channel: None,
                })
                .await;
        }

        // ★ Iterate through all candidate channels (sorted by priority) and try
        // each one until we find a tuner that can be opened successfully.
        // This provides automatic fallback when the highest-priority driver is
        // busy, at capacity, or experiencing a hardware error.
        let pool_keys = self.tuner_pool.keys().await;

        // ★ Capture the current session's tuner info BEFORE the loop.
        // If this session is the sole subscriber, its slot will be freed during
        // channel switch, so it should NOT count against driver capacity.
        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());
        let old_tuner_will_free_slot = self.current_tuner.as_ref()
            .map(|t| {
                let sub_count = t.subscriber_count();
                // Streaming: sole broadcast subscriber → slot freed after unsubscribe
                (sub_count == 1 && self.ts_receiver.is_some()) ||
                // TunerOpen: no broadcast subscription yet → slot freed immediately
                (sub_count == 0 && self.ts_receiver.is_none())
            })
            .unwrap_or(false);

        for (candidate_idx, channel_with_driver) in channels.iter().enumerate() {
            let channel_record = &channel_with_driver.channel;
            let tuner_id = channel_with_driver.bon_driver_path.clone();
            let space = channel_record.bon_space.unwrap_or(0);
            let channel = channel_record.bon_channel.unwrap_or(0);

            // ★ Capacity check: skip drivers that are already at max_instances.
            let max_instances = {
                let db = self.database.lock().await;
                db.get_max_instances_for_path(&tuner_id).unwrap_or(1)
            };

            let key = ChannelKey::space_channel(&tuner_id, space, channel);

            // Count how many instances of this driver are already running
            // (excluding an entry for the exact same channel key we're about
            // to create, since get_or_create would reuse it).
            let mut running_instances = 0i32;
            for gk in &pool_keys {
                if gk.tuner_path == tuner_id && *gk != key {
                    // Skip the current session's own tuner if it will be freed
                    // during channel switch (sole subscriber → slot released).
                    if old_tuner_will_free_slot && old_tuner_key.as_ref() == Some(gk) {
                        continue;
                    }
                    if let Some(existing) = self.tuner_pool.get(gk).await {
                        if existing.is_running() {
                            running_instances += 1;
                        }
                    }
                }
            }

            // Check if an exact-key tuner is already in the pool and running;
            // if so it doesn't count as a "new" instance.
            let existing_for_key = self.tuner_pool.get(&key).await;
            let reuse_existing = existing_for_key
                .as_ref()
                .map_or(false, |t| t.is_running());

            if !reuse_existing && (running_instances + 1) > max_instances {
                info!(
                    "[Session {}] SelectLogicalChannel: skipping candidate {} '{}' — at capacity ({}/{} instances)",
                    self.id, candidate_idx, tuner_id, running_instances, max_instances
                );
                continue;
            }

            // Set current tuner path (will be overwritten if this attempt fails and
            // we move on to the next candidate).
            self.current_tuner_path = Some(tuner_id.clone());
            self.refresh_current_bon_driver_id().await;

            // Try to obtain or create the tuner entry in the pool
            let tuner = match self
                .tuner_pool
                .get_or_create(key.clone(), 2, || async { Ok(()) })
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        "[Session {}] SelectLogicalChannel: candidate {} '{}' pool creation failed: {}",
                        self.id, candidate_idx, tuner_id, e
                    );
                    continue;
                }
            };

            // ★ Bug H fix: cancel any pending idle-close before using this tuner.
            self.tuner_pool.cancel_idle_close(&key).await;

            // Start the BonDriver reader if not already running
            if !tuner.is_running() {
                if let Err(e) = self.start_reader_with_warm(
                    Arc::clone(&tuner),
                    tuner_id.clone(),
                    space,
                    channel,
                ).await {
                    if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                        warn!(
                            "[Session {}] SelectLogicalChannel: candidate {} '{}' channel unavailable: {}",
                            self.id, candidate_idx, tuner_id, e
                        );
                    } else {
                        error!(
                            "[Session {}] SelectLogicalChannel: candidate {} '{}' failed to start reader: {}",
                            self.id, candidate_idx, tuner_id, e
                        );
                    }
                    // Clean up the orphaned pool entry
                    if !tuner.is_running() && !tuner.has_subscribers() {
                        self.tuner_pool.remove(&key).await;
                    }
                    // Try the next candidate
                    continue;
                }
            }

            // ★ Success — this candidate works.
            // Properly unsubscribe from the old tuner before switching.
            let old_tuner = self.current_tuner.take();
            if let Some(old) = old_tuner {
                let same_tuner_reuse = Arc::ptr_eq(&old, &tuner);
                if same_tuner_reuse {
                    // Same SharedTuner (same channel key) — keep subscription.
                    debug!("[Session {}] SelectLogicalChannel: reusing same tuner", self.id);
                    if self.state == SessionState::Streaming {
                        let new_rx = tuner.subscribe();
                        self.ts_receiver = Some(new_rx);
                        old.unsubscribe();
                    }
                } else {
                    // Different tuner — unsubscribe from old and subscribe to new.
                    if self.ts_receiver.is_some() {
                        old.unsubscribe();
                        self.ts_receiver = None;
                        debug!("[Session {}] SelectLogicalChannel: unsubscribed from old tuner, remaining subscribers: {}",
                               self.id, old.subscriber_count());
                        if old.subscriber_count() == 0 {
                            // Stop the old tuner synchronously.  This is critical when
                            // the hardware (e.g. multi-tuner USB card) cannot have
                            // multiple DLLs open simultaneously within a group.
                            let old_max = {
                                let db = self.database.lock().await;
                                db.get_max_instances_for_path(&old.key.tuner_path).unwrap_or(1)
                            };
                            let old_running = {
                                let ks = self.tuner_pool.keys().await;
                                let mut n = 0i32;
                                for k in &ks {
                                    if k.tuner_path == old.key.tuner_path {
                                        if let Some(t) = self.tuner_pool.get(k).await {
                                            if t.is_running() { n += 1; }
                                        }
                                    }
                                }
                                n
                            };
                            if old.key.tuner_path == tuner_id || old_running >= old_max {
                                // Same DLL switch or at capacity — stop synchronously.
                                info!("[Session {}] SelectLogicalChannel: stopping old reader for {:?}",
                                      self.id, old.key);
                                self.tuner_pool.cancel_idle_close(&old.key).await;
                                old.stop_reader().await;
                                self.tuner_pool.remove(&old.key).await;
                            } else {
                                // Different DLL with spare capacity — schedule idle close.
                                info!("[Session {}] SelectLogicalChannel: scheduling idle close for {:?}",
                                      self.id, old.key);
                                self.tuner_pool.schedule_idle_close(old.key.clone(), old).await;
                            }
                        }
                    }
                    if self.state == SessionState::Streaming {
                        self.ts_receiver = Some(tuner.subscribe());
                    }
                }
            } else if self.state == SessionState::Streaming {
                self.ts_receiver = Some(tuner.subscribe());
            }

            self.current_tuner = Some(tuner);

            // Notify B25 decoder about channel change
            if let Some(tuner) = &self.current_tuner {
                tuner.notify_channel_change();
            }

            self.restart_tsreplace_pipeline_if_streaming().await;

            if self.state == SessionState::Ready {
                self.state = SessionState::TunerOpen;
            }

            info!(
                "[Session {}] Logical channel selected (candidate {}): tuner={}, space={}, channel={}",
                self.id, candidate_idx, tuner_id, space, channel
            );

            // Update session registry
            self.session_registry
                .update_tuner(self.id, Some(tuner_id.clone()))
                .await;

            // Update channel info, name, and NID/SID for dashboard logo
            let channel_info = format!("Space {}, Ch {}", space, channel);
            self.session_registry.update_channel(self.id, Some(channel_info.clone())).await;
            self.current_channel_info = Some(channel_info);

            let (channel_name, ch_nid, ch_sid) = {
                let db = self.database.lock().await;
                match db.get_channel_by_physical(&tuner_id, space, channel) {
                    Ok(Some(rec)) => (
                        rec.channel_name.or(rec.raw_name),
                        Some(rec.nid),
                        Some(rec.sid),
                    ),
                    _ => (None, None, None),
                }
            };
            self.session_registry.update_channel_name(self.id, channel_name.clone()).await;
            self.session_registry.update_channel_ids(self.id, ch_nid, ch_sid).await;
            self.current_channel_name = channel_name;

            return self.send_message(ServerMessage::SelectLogicalChannelAck {
                success: true,
                error_code: 0,
                tuner_id: Some(tuner_id),
                space: Some(space),
                channel: Some(channel),
            })
            .await;
        }

        // All candidates exhausted
        error!(
            "[Session {}] SelectLogicalChannel: all {} candidate drivers failed for nid={}, tsid={}, sid={:?}",
            self.id, channels.len(), nid, tsid, sid
        );
        self.send_message(ServerMessage::SelectLogicalChannelAck {
            success: false,
            error_code: ErrorCode::ChannelSetFailed.into(),
            tuner_id: None,
            space: None,
            channel: None,
        })
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
                remote_control_key: ch.remote_control_key.map(|k| k as u8),
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
    async fn send_ts_data(&mut self, data: Bytes) -> std::io::Result<()> {
        // ---- 1) Align outgoing TS to 188-byte packets ----
        self.ts_send_carry.extend_from_slice(&data);

        // Best-effort resync if head is not sync byte (0x47)
        if !self.ts_send_carry.is_empty() && self.ts_send_carry[0] != 0x47 {
            let mut sync_pos: Option<usize> = None;
            for i in 0..self.ts_send_carry.len() {
                if self.ts_send_carry[i] != 0x47 {
                    continue;
                }

                let ok_188 = i + 188 < self.ts_send_carry.len() && self.ts_send_carry[i + 188] == 0x47;
                let ok_376 = i + 376 < self.ts_send_carry.len() && self.ts_send_carry[i + 376] == 0x47;
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
            return Ok(());
        }

        let send_data = Bytes::copy_from_slice(&self.ts_send_carry[..send_len]);
        self.ts_send_carry.drain(0..send_len);

        self.ts_msgs_sent += 1;
        self.ts_bytes_sent += send_len as u64;
        self.bytes_since_last += send_len as u64;

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

                let ok_188 = i + 188 < self.ts_quality_carry.len() && self.ts_quality_carry[i + 188] == 0x47;
                let ok_376 = i + 376 < self.ts_quality_carry.len() && self.ts_quality_carry[i + 376] == 0x47;
                if ok_188 || ok_376 {
                    sync_pos = Some(i);
                    break;
                }
            }

            if let Some(pos) = sync_pos {
                if pos > 0 {
                    self.ts_quality_carry.drain(0..pos);
                }
            } else if self.ts_quality_carry.len() > 188 * 4 {
                // Keep a small tail and wait for next chunk to find sync sequence.
                let keep = 188 * 4;
                let drop_len = self.ts_quality_carry.len() - keep;
                self.ts_quality_carry.drain(0..drop_len);
            }
        }

        let mut delta = crate::tuner::ts_analyzer::TsStreamQualityDelta::default();
        let full_len = self.ts_quality_carry.len() - (self.ts_quality_carry.len() % 188);
        if full_len >= 188 {
            delta = self.ts_quality_analyzer.analyze(&self.ts_quality_carry[..full_len]);
            self.ts_quality_carry.drain(0..full_len);
        }

        self.packets_dropped += delta.packets_dropped;
        self.packets_scrambled += delta.packets_scrambled;
        self.packets_error += delta.packets_error;
        self.interval_packets_total += delta.packets_total;
        self.interval_packets_dropped += delta.packets_dropped;

        if self.last_ts_log.elapsed().as_secs_f32() >= 1.0 {
            info!(
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
                    (self.interval_packets_dropped as f64 / self.interval_packets_total as f64) * 100.0
                } else {
                    0.0
                };

                self.session_registry.update_stats(
                    self.id,
                    signal_level,
                    packets_sent,
                    self.packets_dropped,
                    self.packets_scrambled,
                    self.packets_error,
                    bitrate_mbps,
                ).await;

                let timestamp_ms = chrono::Utc::now().timestamp_millis();
                self.session_registry.push_metrics_sample(
                    self.id,
                    timestamp_ms,
                    bitrate_mbps,
                    packet_loss_rate,
                    signal_level,
                ).await;

                self.signal_samples += 1;
                self.signal_level_sum += signal_level as f64;

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

        self.send_message(ServerMessage::TsData { data: send_data.to_vec() }).await
    }


    /// Send a server message to the client.
    async fn send_message(&mut self, msg: ServerMessage) -> std::io::Result<()> {
        trace!("[Session {}] Sending: {:?}", self.id, msg);

        let encoded = encode_server_message(&msg).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;

        self.socket.write_all(&encoded).await
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
                warn!("[Session {}] Failed to flush session progress to DB: {}", self.id, e);
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
                warn!("[Session {}] Failed to flush driver quality stats to DB: {}", self.id, e);
            }

            // Update flushed counters
            self.flushed_packets = current_packets;
            self.flushed_dropped = self.packets_dropped;
            self.flushed_scrambled = self.packets_scrambled;
            self.flushed_error = self.packets_error;
        }

        debug!("[Session {}] Flushed metrics to DB (duration={}s, dropped={}, scrambled={}, error={})",
            self.id, duration_secs, self.packets_dropped, self.packets_scrambled, self.packets_error);
    }

    /// Clean up session resources.
    async fn cleanup(&mut self) {
        self.stop_warm_tuner().await;
        // Unsubscribe from tuner and check if we should stop reader
        if let Some(tuner) = self.current_tuner.take() {
            // Unsubscribe only if we have an active subscription
            if self.ts_receiver.is_some() {
                tuner.unsubscribe();
            }

            // ★ Always check if we should stop the reader
            // This handles the case where StopStream was called before disconnect
            // (ts_receiver is None but tuner may still have no subscribers)
            if tuner.subscriber_count() == 0 {
                info!("[Session {}] No more subscribers, scheduling keep-alive close for {:?}", self.id, tuner.key);
                self.tuner_pool
                    .schedule_idle_close(tuner.key.clone(), Arc::clone(&tuner))
                    .await;
            }
        }
        self.ts_receiver = None;
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
            ) {
                warn!("[Session {}] Failed to update session history: {}", self.id, e);
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
                warn!("[Session {}] Failed to update driver quality stats: {}", self.id, e);
            }
        }

        // Update session registry
        self.session_registry.update_tuner(self.id, None).await;
        self.session_registry.update_streaming(self.id, false).await;
        self.session_registry.update_channel(self.id, None).await;
    }

    /// Handle OpenTunerWithGroup message.
    async fn handle_open_tuner_with_group(&mut self, group_name: String) -> std::io::Result<()> {
        if self.state != SessionState::Ready {
            return self
                .send_error(ErrorCode::InvalidState, "Not in ready state")
                .await;
        }

        info!("[Session {}] Opening tuner group: {}", self.id, group_name);
        self.stop_warm_tuner().await;

        // TODO: Implement group space info building
        // For now, send error
        self.send_message(ServerMessage::OpenTunerAck {
            success: false,
            error_code: 0xFF00, // Not implemented
            bondriver_version: 0,
        })
        .await
    }

    /// Handle SetChannelSpaceInGroup message.
    async fn handle_set_channel_space_in_group(
        &mut self,
        _group_name: String,
        _space_idx: u32,
        _channel: u32,
        priority: i32,
        exclusive: bool,
    ) -> std::io::Result<()> {
        self.session_registry
            .update_client_controls(self.id, Some(priority), Some(exclusive))
            .await;
        let (effective_priority, effective_exclusive) = self
            .session_registry
            .get_effective_controls(self.id)
            .await
            .unwrap_or((Some(priority), exclusive));
        let priority = effective_priority.unwrap_or(priority);
        let exclusive = effective_exclusive;
        // TODO: Implement group-based channel selection
        self.send_message(ServerMessage::SetChannelSpaceAck {
            success: false,
            error_code: 0xFF00, // Not implemented
        })
        .await
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        debug!("[Session {}] Session dropped", self.id);
    }
}
