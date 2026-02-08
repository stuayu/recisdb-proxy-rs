//! Periodic channel scanning scheduler.
//!
//! This module provides automatic periodic scanning of BonDriver tuners
//! to keep the channel database up to date.
//!
//! # How It Works
//!
//! 1. The scheduler runs as a background task
//! 2. It periodically checks which BonDrivers are due for scanning
//! 3. For each due BonDriver, it initiates a channel scan using BonDriverTuner
//! 4. Scan results are merged into the database
//!
//! # Configuration
//!
//! Each BonDriver can be configured with:
//! - `auto_scan_enabled`: Whether automatic scanning is enabled
//! - `scan_interval_hours`: How often to scan (0 = disabled)
//! - `scan_priority`: Priority order for scanning

use std::sync::Arc;
use std::time::Duration;
use std::collections::BTreeMap;

use log::{debug, error, info, warn};
use tokio::sync::Mutex;
use tokio::time::interval;

use crate::bondriver::BonDriverTuner;
use crate::database::BonDriverRecord;
use crate::server::listener::DatabaseHandle;
use crate::tuner::TunerPool;
use recisdb_protocol::BandType;

/// Scan scheduler configuration.
#[derive(Debug, Clone)]
pub struct ScanSchedulerConfig {
    /// Interval between scheduler checks (seconds).
    pub check_interval_secs: u64,
    /// Maximum concurrent scans.
    pub max_concurrent_scans: usize,
    /// Scan timeout per BonDriver (seconds).
    pub scan_timeout_secs: u64,
}

impl Default for ScanSchedulerConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 60,        // Check every minute
            max_concurrent_scans: 1,         // One scan at a time
            scan_timeout_secs: 900,          // 15 minute timeout
        }
    }
}

/// Scan scheduler state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerState {
    /// Scheduler is running.
    Running,
    /// Scheduler is paused.
    Paused,
    /// Scheduler is stopped.
    Stopped,
}

/// Periodic channel scanning scheduler.
pub struct ScanScheduler {
    /// Database handle.
    database: DatabaseHandle,
    /// Tuner pool reference.
    tuner_pool: Arc<TunerPool>,
    /// Configuration.
    config: ScanSchedulerConfig,
    /// Current state.
    state: Arc<Mutex<SchedulerState>>,
    /// Number of active scans.
    active_scans: Arc<std::sync::atomic::AtomicUsize>,
}

impl ScanScheduler {
    /// Create a new scan scheduler.
    pub fn new(
        database: DatabaseHandle,
        tuner_pool: Arc<TunerPool>,
        config: ScanSchedulerConfig,
    ) -> Self {
        Self {
            database,
            tuner_pool,
            config,
            state: Arc::new(Mutex::new(SchedulerState::Running)),
            active_scans: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Start the scheduler background task.
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Run the scheduler loop.
    async fn run(&self) {
        info!("ScanScheduler: Starting with check interval {} seconds",
              self.config.check_interval_secs);

        let mut check_interval = interval(Duration::from_secs(self.config.check_interval_secs));

        loop {
            check_interval.tick().await;

            // Check scheduler state
            let state = *self.state.lock().await;
            match state {
                SchedulerState::Stopped => {
                    info!("ScanScheduler: Stopped");
                    break;
                }
                SchedulerState::Paused => {
                    debug!("ScanScheduler: Paused, skipping check");
                    continue;
                }
                SchedulerState::Running => {
                    // Continue with check
                }
            }

            // Check for due scans
            if let Err(e) = self.check_and_scan().await {
                error!("ScanScheduler: Error during scan check: {}", e);
            }
        }
    }

    /// Check for due BonDrivers and initiate scans.
    async fn check_and_scan(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if we can start more scans
        let active = self.active_scans.load(std::sync::atomic::Ordering::SeqCst);
        if active >= self.config.max_concurrent_scans {
            debug!("ScanScheduler: Max concurrent scans reached ({})", active);
            return Ok(());
        }

        // Get due BonDrivers from database
        let due_drivers = {
            let db = self.database.lock().await;
            db.get_due_bon_drivers()?
        };

        if due_drivers.is_empty() {
            debug!("ScanScheduler: No BonDrivers due for scanning");
            return Ok(());
        }

        info!("ScanScheduler: {} BonDriver(s) due for scanning", due_drivers.len());

        // Process each due driver
        for driver in due_drivers {
            // Check again if we can start more scans
            let active = self.active_scans.load(std::sync::atomic::Ordering::SeqCst);
            if active >= self.config.max_concurrent_scans {
                break;
            }

            // Start scan in background
            self.spawn_scan(driver).await;
        }

        Ok(())
    }

    /// Spawn a scan task for a BonDriver.
    async fn spawn_scan(&self, driver: BonDriverRecord) {
        let database = self.database.clone();
        let tuner_pool = self.tuner_pool.clone();
        let active_scans = self.active_scans.clone();
        let timeout_secs = self.config.scan_timeout_secs;

        // Increment active scan count
        active_scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        tokio::spawn(async move {
            info!("ScanScheduler: Starting scan for {}", driver.dll_path);

            // Perform the scan with timeout
            let scan_result = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                perform_scan(&driver, database.clone(), tuner_pool),
            )
            .await;

            match scan_result {
                Ok(Ok(channel_count)) => {
                    info!(
                        "ScanScheduler: Scan completed for {}: {} channels found",
                        driver.dll_path, channel_count
                    );

                    // Update next scan time
                    let next_scan = chrono::Utc::now().timestamp()
                        + (driver.scan_interval_hours as i64 * 3600);

                    let db = database.lock().await;
                    if let Err(e) = db.update_next_scan(driver.id, next_scan) {
                        warn!("ScanScheduler: Failed to update next scan time: {}", e);
                    }
                }
                Ok(Err(e)) => {
                    error!("ScanScheduler: Scan failed for {}: {}", driver.dll_path, e);

                    // Record failure in scan history
                    let db = database.lock().await;
                    let _ = db.insert_scan_history(
                        driver.id,
                        0,
                        false,
                        Some(&e.to_string()),
                    );
                }
                Err(_) => {
                    error!(
                        "ScanScheduler: Scan timed out for {} after {} seconds",
                        driver.dll_path, timeout_secs
                    );

                    // Record timeout in scan history
                    let db = database.lock().await;
                    let _ = db.insert_scan_history(
                        driver.id,
                        0,
                        false,
                        Some("Scan timed out"),
                    );
                }
            }

            // Decrement active scan count
            active_scans.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// Pause the scheduler.
    #[allow(dead_code)]
    pub async fn pause(&self) {
        let mut state = self.state.lock().await;
        if *state == SchedulerState::Running {
            *state = SchedulerState::Paused;
            info!("ScanScheduler: Paused");
        }
    }

    /// Resume the scheduler.
    #[allow(dead_code)]
    pub async fn resume(&self) {
        let mut state = self.state.lock().await;
        if *state == SchedulerState::Paused {
            *state = SchedulerState::Running;
            info!("ScanScheduler: Resumed");
        }
    }

    /// Stop the scheduler.
    #[allow(dead_code)]
    pub async fn stop(&self) {
        let mut state = self.state.lock().await;
        *state = SchedulerState::Stopped;
        info!("ScanScheduler: Stop requested");
    }

    /// Get the current scheduler state.
    #[allow(dead_code)]
    pub async fn state(&self) -> SchedulerState {
        *self.state.lock().await
    }

    /// Get the number of active scans.
    #[allow(dead_code)]
    pub fn active_scan_count(&self) -> usize {
        self.active_scans.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Trigger an immediate scan check.
    /// This can be called to force a scan outside the regular schedule.
    pub async fn trigger_scan(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("ScanScheduler: Manual scan triggered");
        self.check_and_scan().await
    }
}

/// Minimum signal level to consider a channel as having signal.
const MIN_SIGNAL_LEVEL: f32 = 3.0;

/// Time to wait for signal lock after setting channel (milliseconds).
const SIGNAL_LOCK_WAIT_MS: u64 = 500;

/// Maximum time to wait for TS analysis (milliseconds).
const TS_ANALYSIS_TIMEOUT_MS: u64 = 300000;

/// TS パケット長
const TS_PACKET_SIZE: usize = 188;

/// Buffer size for TS stream reading.
const TS_BUFFER_SIZE: usize = TS_PACKET_SIZE * 1024; // ~192KB

/// WaitTsStream 1回あたりの待機(ms)
const TS_WAIT_MS: u32 = 200;

/// 解析に投入する最小パケット数（PAT/NIT/SDT拾うだけならこの程度から開始）
const MIN_PACKETS_PER_FEED: usize = 10;

/// WaitTsStream が false のときでも、一定回数に1回は Get を試す（実装差対策）
const FORCE_GET_EVERY: usize = 10;


/// Result from scanning a single channel.
#[derive(Debug)]
struct ScanChannelResult {
    space: u32,
    channel: u32,
    channel_name: String,
    signal_level: f32,
    /// Network ID (from NIT/SDT)
    network_id: Option<u16>,
    /// Transport Stream ID (from PAT)
    transport_stream_id: Option<u16>,
    /// Services found on this channel
    services: Vec<ServiceInfo>,
}

/// Service information extracted from TS stream.
#[derive(Debug, Clone)]
struct ServiceInfo {
    /// Service ID
    service_id: u16,
    /// Service name (from SDT)
    service_name: Option<String>,
    /// Service type (0x01=TV, etc.)
    service_type: Option<u8>,
}

use crate::ts_analyzer::{TsAnalyzer, AnalyzerConfig};

/// Scan channels in a space by enumerating BonDriver's channel list.
/// This runs in a blocking thread to avoid Send/Sync issues with raw pointers.
fn scan_space_blocking(
    dll_path: &str,
    space: u32,
) -> Result<Vec<ScanChannelResult>, Box<dyn std::error::Error + Send + Sync>> {
    info!("scan_space_blocking: Loading BonDriver {}", dll_path);
    let tuner = BonDriverTuner::new(dll_path)?;
    info!("scan_space_blocking: BonDriver loaded, version {}", tuner.version());

    // Enumerate channels defined by BonDriver for this space
    let mut channels: Vec<(u32, String)> = Vec::new();
    let mut ch_idx: u32 = 0;
    // BonDriver implementations may not return None reliably,
    // so cap at a reasonable maximum as a safety net.
    const MAX_CHANNELS: u32 = 1024;
    let mut consecutive_none: u32 = 0;
    while ch_idx < MAX_CHANNELS {
        match tuner.enum_channel_name(space, ch_idx) {
            Some(name) => {
                channels.push((ch_idx, name));
                consecutive_none = 0;
            }
            None => {
                // Some BonDrivers have gaps in channel indices.
                // Allow up to 3 consecutive Nones before stopping.
                consecutive_none += 1;
                if consecutive_none > 3 {
                    break;
                }
            }
        }
        ch_idx += 1;
    }

    info!("scan_space_blocking: Space {} has {} channels defined by BonDriver", space, channels.len());

    let mut results = Vec::new();

    for (channel, channel_name) in &channels {
        let channel = *channel;

        debug!("scan_space_blocking: Trying space={}, channel={} ({})", space, channel, channel_name);

        // Set channel
        if let Err(e) = tuner.set_channel(space, channel) {
            debug!("scan_space_blocking: SetChannel failed: {}", e);
            continue;
        }

        // Purge any old data
        tuner.purge_ts_stream();

        // Wait for signal lock (blocking sleep)
        std::thread::sleep(std::time::Duration::from_millis(SIGNAL_LOCK_WAIT_MS));

        // Check signal level
        let signal_level = tuner.get_signal_level();
        debug!("scan_space_blocking: Signal level = {:.2} dB", signal_level);

        if signal_level < MIN_SIGNAL_LEVEL {
            debug!("scan_space_blocking: Signal too weak ({:.2} < {:.2})", signal_level, MIN_SIGNAL_LEVEL);
            continue;
        }

        info!("scan_space_blocking: ✓ Found channel - Space={} CH={} Name=\"{}\" Signal={:.2}dB",
              space, channel, channel_name, signal_level);

        // Analyze TS stream to get TSID/SID
        // Retry up to 3 times if NID is missing or invalid (0x0000)
        let mut analysis_result = None;
        for attempt in 0..3 {
            // catch_unwind to prevent panics (e.g. from FFI) from crashing the process
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                analyze_ts_stream(&tuner)
            })) {
                Ok(r) => r,
                Err(panic_err) => {
                    error!("scan_space_blocking: PANIC in analyze_ts_stream (attempt {}/3): {:?}", attempt + 1, panic_err);
                    Err("panic in analyze_ts_stream".into())
                }
            };
            
            match result {
                Ok((Some(nid), tsid, svcs)) if nid == 0x0000 => {
                    warn!("scan_space_blocking: NID is 0x0000 (attempt {}/3), retrying...", attempt + 1);
                    // Purge and wait before retry
                    tuner.purge_ts_stream();
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                Ok((None, tsid, svcs)) => {
                    // NID not detected, retry
                    warn!("scan_space_blocking: NID not detected (attempt {}/3), retrying...", attempt + 1);
                    tuner.purge_ts_stream();
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    if attempt < 2 {
                        continue;
                    } else {
                        // After 3 attempts, log warning but keep the result
                        warn!("scan_space_blocking:   → NID not detected after {} attempts, using available data", attempt + 1);
                        analysis_result = Some((None, tsid, svcs));
                        break;
                    }
                }
                Ok((nid, tsid, svcs)) => {
                    analysis_result = Some((nid, tsid, svcs));
                    break;
                }
                Err(e) => {
                    if attempt < 2 {
                        warn!("scan_space_blocking: TS analysis failed (attempt {}/3): {}, retrying...", attempt + 1, e);
                        tuner.purge_ts_stream();
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        continue;
                    } else {
                        warn!("scan_space_blocking:   → TS analysis failed after {} attempts: {}", attempt + 1, e);
                        analysis_result = Some((None, None, Vec::new()));
                        break;
                    }
                }
            }
        }

        let (network_id, transport_stream_id, services) = match analysis_result {
            Some((nid, tsid, svcs)) => {
                let nid_str = nid.map(|n| format!("0x{:04X}", n)).unwrap_or_else(|| "N/A".to_string());
                let tsid_str = tsid.map(|n| format!("0x{:04X}", n)).unwrap_or_else(|| "N/A".to_string());
                info!("scan_space_blocking:   → NID={} TSID={} ({} services detected)",
                      nid_str, tsid_str, svcs.len());
                for (idx, svc) in svcs.iter().enumerate() {
                    let svc_type = match svc.service_type {
                        Some(0x01) => "TV",
                        Some(0x02) => "Radio",
                        Some(0xC0) => "Data",
                        Some(t) => &format!("{:02X}", t),
                        None => "Unknown",
                    };
                    let svc_name = svc.service_name.as_deref().unwrap_or("(unnamed)");
                    info!("scan_space_blocking:     [{}/{}] SID=0x{:04X} Type={} Name=\"{}\"",
                          idx + 1, svcs.len(), svc.service_id, svc_type, svc_name);
                }
                (nid, tsid, svcs)
            }
            None => {
                warn!("scan_space_blocking:   → TS analysis failed");
                (None, None, Vec::new())
            }
        };

        results.push(ScanChannelResult {
            space,
            channel,
            channel_name: channel_name.clone(),
            signal_level,
            network_id,
            transport_stream_id,
            services,
        });
    }

    Ok(results)
}

/// Analyze TS stream to extract TSID, NID, and service information.
fn analyze_ts_stream(
    tuner: &BonDriverTuner,
) -> Result<(Option<u16>, Option<u16>, Vec<ServiceInfo>), Box<dyn std::error::Error + Send + Sync>> {
    debug!("analyze_ts_stream: Starting TS analysis");

    let config = AnalyzerConfig {
        parse_nit: true,
        parse_sdt: true,
        parse_all_pmts: false,
        max_packets: 200_000,
    };

    let mut analyzer = TsAnalyzer::new(config);
    let mut buffer = vec![0u8; TS_BUFFER_SIZE];

    // TS は 188 バイト固定長なので carry で 188 境界に揃える [1](https://zenn.dev/sakuraimikoto33/articles/36b1b633c7607d)
    let mut carry: Vec<u8> = Vec::with_capacity(TS_PACKET_SIZE * 4);

    let start_time = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(TS_ANALYSIS_TIMEOUT_MS);

    let mut total_bytes_read = 0usize;
    let mut reads = 0usize;
    let mut attempts = 0usize;

    // size==0 / WouldBlock が続くときのバックオフ（ms）
    let mut backoff_ms: u64 = 1;

    while !analyzer.is_complete() && start_time.elapsed() < timeout {
        // 1) WaitTsStream は “ヒント”。ゲートにしない（実装差吸収）[2](https://support.rockwellautomation.com/app/answers/answer_view/a_id/1153049/~/studio-5000-logix-designer-error-0xc0000005-on-windows-11-24h2-)
        let waited = tuner.wait_ts_stream(TS_WAIT_MS);

        // 2) GetTsStream は毎回試す
        attempts += 1;

        let (size, remaining) = match tuner.get_ts_stream(&mut buffer) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // データ無し
                backoff_ms = (backoff_ms * 2).min(50);
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        if size == 0 {
            // TRUE でも size=0 を返す実装があるので同様にバックオフ
            backoff_ms = (backoff_ms * 2).min(50);
            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            continue;
        }

        // データが来たらバックオフをリセット
        backoff_ms = if waited { 0 } else { 1 };

        reads += 1;
        total_bytes_read += size;

        // ログは間引く（毎回出すと遅い）
        if reads % 20 == 1 {
            debug!(
                "analyze_ts_stream: Read {} bytes (total: {} bytes, remaining: {}, attempts: {})",
                size, total_bytes_read, remaining, attempts
            );
        }

        // 3) carry へ入れて 188 単位で feed（TS固定長）[1](https://zenn.dev/sakuraimikoto33/articles/36b1b633c7607d)
        carry.extend_from_slice(&buffer[..size]);

        // resync（sync byte 0x47 を探す）[1](https://zenn.dev/sakuraimikoto33/articles/36b1b633c7607d)
        if !carry.is_empty() && carry[0] != 0x47 {
            if let Some(pos) = carry.iter().position(|&b| b == 0x47) {
                debug!("analyze_ts_stream: resync drop {} bytes (0x47 at {})", pos, pos);
                carry.drain(0..pos);
            } else {
                debug!("analyze_ts_stream: resync failed, dropping {} bytes", carry.len());
                carry.clear();
                continue;
            }
        }

        let full_len = carry.len() - (carry.len() % TS_PACKET_SIZE);
        if full_len >= TS_PACKET_SIZE {
            analyzer.feed(&carry[..full_len]);
            carry.drain(0..full_len);
        }

        if reads % 50 == 0 {
            let r = analyzer.result();
            debug!(
                "analyze_ts_stream: Progress - PAT:{} NIT:{} SDT:{} packets:{}",
                r.pat.is_some(),
                r.nit.is_some(),
                r.sdt.is_some(),
                r.packets_processed
            );
        }
    }

    let elapsed = start_time.elapsed();
    let result = analyzer.into_result();

    info!(
        "analyze_ts_stream: Completed in {:?}, read {} bytes in {} reads ({} attempts), {} packets processed",
        elapsed, total_bytes_read, reads, attempts, result.packets_processed
    );
    info!(
        "analyze_ts_stream: PAT:{} NIT:{} SDT:{} complete:{}",
        result.pat.is_some(), result.nit.is_some(), result.sdt.is_some(), result.complete
    );

    // services 抽出（元コード踏襲）
    let services: Vec<ServiceInfo> = if let Some(ref pat) = result.pat {
        pat.get_all_program_numbers()
            .into_iter()
            .filter(|&sid| sid != 0)
            .map(|sid| {
                let (service_name, service_type) = result
                    .sdt
                    .as_ref()
                    .and_then(|sdt| sdt.find_service(sid))
                    .map(|svc| {
                        let name = svc.get_service_name().map(|s| s.to_string());
                        let stype = svc.get_service_type();
                        (name, stype)
                    })
                    .unwrap_or((None, None));

                ServiceInfo { service_id: sid, service_name, service_type }
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok((result.network_id, result.transport_stream_id, services))
}

/// Convert scan results to ChannelInfo for database storage.
/// Each ScanChannelResult may contain multiple services (SIDs).
fn scan_results_to_channel_infos(
    results: &[ScanChannelResult],
) -> Vec<recisdb_protocol::ChannelInfo> {
    let mut channel_infos = Vec::new();

    for r in results {
        let nid = r.network_id.unwrap_or(0);
        let tsid = r.transport_stream_id.unwrap_or(0);

        if r.services.is_empty() {
            // No services found, create entry with minimal info
            // This shouldn't happen if TS analysis succeeded
            warn!("scan_results_to_channel_infos: No services found for space={}, channel={}",
                  r.space, r.channel);
            let mut info = recisdb_protocol::ChannelInfo::new(nid, 0, tsid);
            info.channel_name = Some(r.channel_name.clone());
            info.bon_space = Some(r.space);
            info.bon_channel = Some(r.channel);
            channel_infos.push(info);
        } else {
            // Create a ChannelInfo entry for each service
            for svc in &r.services {
                let mut info = recisdb_protocol::ChannelInfo::new(nid, svc.service_id, tsid);
                info.channel_name = svc.service_name.clone().or_else(|| Some(r.channel_name.clone()));
                info.service_type = svc.service_type;
                info.bon_space = Some(r.space);
                info.bon_channel = Some(r.channel);
                channel_infos.push(info);
            }
        }
    }

    channel_infos
}

/// Perform a channel scan for a BonDriver.
async fn perform_scan(
    driver: &BonDriverRecord,
    database: DatabaseHandle,
    _tuner_pool: Arc<TunerPool>,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    debug!("perform_scan: Starting scan for {}", driver.dll_path);

    let dll_path = driver.dll_path.clone();
    let driver_id = driver.id;

    // Get existing channel spaces from database to know what to scan
    let scan_ranges = {
        let db = database.lock().await;
        db.get_tuning_spaces(driver_id).unwrap_or_default()
    };

    // Collect all scan results
    let mut all_results = Vec::new();

    if scan_ranges.is_empty() {
        warn!("perform_scan: No known spaces, enumerating spaces from BonDriver");

        let dll = dll_path.clone();
        let results = tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();

            // 1) BonDriver を一度ロードして、実在する tuning space を列挙する
            let tuner = match BonDriverTuner::new(&dll) {
                Ok(t) => t,
                Err(e) => {
                    warn!("perform_scan: Failed to load BonDriver for space enumeration: {}", e);
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(results);
                }
            };

            // BonDriver 実装差対策：enum_tuning_space が None を返さない場合の保険
            const MAX_SPACES: u32 = 64;

            let mut spaces: Vec<(u32, String)> = Vec::new();
            let mut space_idx: u32 = 0;

            while space_idx < MAX_SPACES {
                match tuner.enum_tuning_space(space_idx) {
                    Some(space_name) => {
                        info!("perform_scan: Found space {}: {}", space_idx, space_name);
                        spaces.push((space_idx, space_name));
                        space_idx += 1;
                    }
                    None => break,
                }
            }

            if spaces.is_empty() {
                warn!("perform_scan: BonDriver reported no tuning spaces");
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(results);
            }

            // 2) 列挙された space だけスキャンする
            for (space, space_name) in spaces {
                info!("perform_scan: Scanning space {} ({})", space, space_name);

                match scan_space_blocking(&dll, space) {
                    Ok(r) => results.extend(r),
                    Err(e) => warn!("perform_scan: Space {} scan failed: {}", space, e),
                }
            }

            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(results)
        })
        .await??;

        all_results = results;
    } else {
        // Scan known spaces（元のままでOK）
        for (space, space_name) in scan_ranges {
            info!("perform_scan: Scanning space {} ({})", space, space_name);
            let dll = dll_path.clone();

            let results = tokio::task::spawn_blocking(move || {
                scan_space_blocking(&dll, space)
            })
            .await??;

            all_results.extend(results);
        }
    }

    // Convert results to ChannelInfo
    let channel_infos = scan_results_to_channel_infos(&all_results);
    let total = channel_infos.len();

    // Log detailed scan results
    log_scan_results(&channel_infos, total);

    // Merge results into database
    if !channel_infos.is_empty() {
        let mut db = database.lock().await;
        match db.merge_scan_results(driver_id, &channel_infos) {
            Ok(result) => {
                info!("perform_scan: Merged {} inserted, {} updated", result.inserted, result.updated);
            }
            Err(e) => {
                error!("perform_scan: Failed to merge results: {}", e);
            }
        }

        // Record successful scan in history
        let _ = db.insert_scan_history(
            driver_id,
            total as i32,
            true,
            None,
        );
    }

    info!(
        "perform_scan: Completed scan for {}: {} channels found",
        driver.dll_path, total
    );

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_scheduler_config_default() {
        let config = ScanSchedulerConfig::default();
        assert_eq!(config.check_interval_secs, 60);
        assert_eq!(config.max_concurrent_scans, 1);
        assert_eq!(config.scan_timeout_secs, 900);
    }
}

/// Log detailed scan results with regional and band-type information.
fn log_scan_results(channel_infos: &[recisdb_protocol::ChannelInfo], total: usize) {
    // Aggregate by band type and region
    let mut stats: BTreeMap<(u8, Option<String>), usize> = BTreeMap::new();
    let mut band_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut region_counts: BTreeMap<String, usize> = BTreeMap::new();

    for ch in channel_infos {
        let band_type = BandType::from_nid(ch.nid);
        let band_name = band_type.display_name();

        // Count by band
        *band_counts.entry(band_name).or_insert(0) += 1;

        // Count by region (for terrestrial)
        if band_type == BandType::Terrestrial {
            if let Some(ref region) = ch.terrestrial_region {
                *region_counts.entry(region.clone()).or_insert(0) += 1;
            }
        }

        // Count by band + region
        let band_type_u8 = band_type as u8;
        let region = ch.terrestrial_region.clone();
        *stats.entry((band_type_u8, region)).or_insert(0) += 1;
    }

    // Log summary by band type
    info!("perform_scan: ==== Scan Results Summary ====");
    info!("perform_scan: Total channels found: {}", total);
    info!("perform_scan: ");
    info!("perform_scan: [Breakdown by Broadcast Type]");
    for (band_name, count) in &band_counts {
        info!("perform_scan:   {}: {} channels", band_name, count);
    }

    // Log terrestrial regions if present
    if !region_counts.is_empty() {
        info!("perform_scan: ");
        info!("perform_scan: [Terrestrial Regions]");
        for (region, count) in &region_counts {
            info!("perform_scan:   {}: {} channels", region, count);
        }
    }

    // Log detailed stats
    if channel_infos.len() <= 100 {
        info!("perform_scan: ");
        info!("perform_scan: [Detailed Channel List]");
        for ch in channel_infos {
            let band_type = BandType::from_nid(ch.nid);
            let band_name = band_type.display_name();
            let region = ch.terrestrial_region.as_deref().unwrap_or("N/A");
            let service_type = ch.service_type.map(|st| {
                match st {
                    0x01 => "TV",
                    0x02 => "Radio",
                    0xC0 => "Data",
                    _ => "Other",
                }
            }).unwrap_or("Unknown");

            let service_name = ch.channel_name.as_deref().unwrap_or("(unnamed)");

            info!(
                "perform_scan:   NID=0x{:04X} SID={:4} TSID={:5} Type={:5} Band={:5} Region={:6} Name: {}",
                ch.nid, ch.sid, ch.tsid, service_type, band_name, region, service_name
            );
        }
    } else {
        info!("perform_scan: (Omitting detailed list - {} channels)", channel_infos.len());
    }

    info!("perform_scan: ==== End of Results ====");
}
