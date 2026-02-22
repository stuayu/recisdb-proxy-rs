//! Shared tuner implementation with broadcast capability.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::tuner::b25_pipe::B25Pipe; // 作った場所に合わせて
use b25_sys::DecoderOptions; // 鍵が必要な場合

use bytes::Bytes;
use futures_util::AsyncBufRead;
use log::{debug, error, info, trace, warn};
use tokio::sync::broadcast;

use crate::bondriver::BonDriverTuner;
use crate::tuner::channel_key::ChannelKey;
use crate::tuner::lock::TunerLock;
use crate::tuner::logo_collector::ChannelLogoCollector;
use crate::tuner::ts_analyzer::{TsPacketAnalyzer, TsStreamQuality};
use crate::tuner::pool::TunerPoolConfig;

/// Capacity of the broadcast channel for TS data.
/// Increased to 4096 (256MB of 64KB chunks) to support multiple simultaneous subscribers
/// without buffer overflow when subscriber read speeds vary significantly.
/// Each slot holds a 64KB chunk, so 4096 slots = ~256MB of buffering capacity.
const BROADCAST_CAPACITY: usize = 4096;

/// Size of each TS data chunk to read from the tuner.
/// Increased to 256KB to handle BonDrivers (like FukuDLL) that may return
/// data in larger chunks than standard 64KB.
const TS_CHUNK_SIZE: usize = 262144; // 256KB buffer

/// Runtime startup tuning parameters for delayed network-backed drivers.
#[derive(Debug, Clone, Copy)]
pub struct ReaderStartupConfig {
    pub set_channel_retry_interval_ms: u64,
    pub set_channel_retry_timeout_ms: u64,
    pub signal_poll_interval_ms: u64,
    pub signal_wait_timeout_ms: u64,
}

impl From<&TunerPoolConfig> for ReaderStartupConfig {
    fn from(cfg: &TunerPoolConfig) -> Self {
        Self {
            set_channel_retry_interval_ms: cfg.set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms: cfg.set_channel_retry_timeout_ms,
            signal_poll_interval_ms: cfg.signal_poll_interval_ms,
            signal_wait_timeout_ms: cfg.signal_wait_timeout_ms,
        }
    }
}

/// A shared tuner instance that can broadcast TS data to multiple clients.
pub struct SharedTuner {
    /// The channel key identifying this tuner/channel combination.
    pub key: ChannelKey,
    /// Broadcast sender for TS data.
    tx: broadcast::Sender<Bytes>,
    /// Channel change notification sender.
    channel_change_tx: broadcast::Sender<()>,
    /// Reference count of active subscribers.
    subscriber_count: AtomicU32,
    /// Flag indicating if the tuner reader task is running.
    is_running: AtomicBool,
    /// Handle to the reader task (if running).
    reader_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Signal level (updated periodically).
    signal_level: AtomicU32,
    /// BonDriver version (1, 2, or 3).
    bondriver_version: u8,
    /// Lock for exclusive/shared access control.
    lock: TunerLock,
    /// Counter for received TS packets.
    packets_received: AtomicU64,
    /// TS quality analyzer (drop/scramble/error stats).
    quality_analyzer: tokio::sync::Mutex<TsPacketAnalyzer>,
}

impl SharedTuner {
    /// Create a new shared tuner with the given key.
    pub fn new(key: ChannelKey, bondriver_version: u8) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (channel_change_tx, _) = broadcast::channel(1); // Only need to notify once
        Arc::new(Self {
            key,
            tx,
            channel_change_tx,
            subscriber_count: AtomicU32::new(0),
            is_running: AtomicBool::new(false),
            reader_handle: tokio::sync::Mutex::new(None),
            signal_level: AtomicU32::new(0),
            bondriver_version,
            lock: TunerLock::new(),
            packets_received: AtomicU64::new(0),
            quality_analyzer: tokio::sync::Mutex::new(TsPacketAnalyzer::new()),
        })
    }

    /// Get a reference to the tuner lock.
    pub fn lock(&self) -> &TunerLock {
        &self.lock
    }

    /// Get the current signal level (alias for signal_level()).
    pub fn get_signal_level(&self) -> f32 {
        self.signal_level()
    }

    /// Check if TS packets have been received.
    pub fn has_received_packets(&self) -> bool {
        self.packets_received.load(Ordering::SeqCst) > 0
    }

    /// Increment the packet counter.
    pub fn increment_packet_count(&self, count: u64) {
        self.packets_received.fetch_add(count, Ordering::SeqCst);
    }

    /// Reset the packet counter.
    pub fn reset_packet_count(&self) {
        self.packets_received.store(0, Ordering::SeqCst);
    }

    /// Get the total number of packets received.
    pub fn packet_count(&self) -> u64 {
        self.packets_received.load(Ordering::SeqCst)
    }

    /// Get a snapshot of TS stream quality stats.
    pub async fn quality_snapshot(&self) -> TsStreamQuality {
        let analyzer = self.quality_analyzer.lock().await;
        analyzer.snapshot()
    }

    /// Wait for the first TS packet to arrive (indicating driver is ready).
    /// Returns true if packet received within timeout, false if timeout.
    pub async fn wait_first_data(&self, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        
        loop {
            // Check if we've received any data
            if self.has_received_packets() {
                info!("[SharedTuner] First data received after {}ms", start.elapsed().as_millis());
                return true;
            }
            
            // Check timeout
            if start.elapsed() > timeout {
                warn!("[SharedTuner] wait_first_data timeout after {}ms", timeout_ms);
                return false;
            }
            
            // Small sleep to avoid busy waiting
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Get the BonDriver version.
    pub fn bondriver_version(&self) -> u8 {
        self.bondriver_version
    }

    /// Subscribe to the TS data stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.subscriber_count.fetch_add(1, Ordering::SeqCst);
        debug!(
            "New subscriber for {:?}, total: {}",
            self.key,
            self.subscriber_count.load(Ordering::SeqCst)
        );
        self.tx.subscribe()
    }

    /// Subscribe to channel change notifications.
    pub fn subscribe_channel_change(&self) -> broadcast::Receiver<()> {
        self.channel_change_tx.subscribe()
    }

    /// Notify all subscribers that the channel has changed (to trigger B25 reset).
    pub fn notify_channel_change(&self) {
        let _ = self.channel_change_tx.send(());
        debug!("Channel change notified for {:?}", self.key);
    }

    /// Unsubscribe from the TS data stream.
    pub fn unsubscribe(&self) {
        let prev = self.subscriber_count.fetch_sub(1, Ordering::SeqCst);
        debug!(
            "Subscriber removed from {:?}, remaining: {}",
            self.key,
            prev - 1
        );
    }

    /// Get the number of active subscribers.
    pub fn subscriber_count(&self) -> u32 {
        self.subscriber_count.load(Ordering::SeqCst)
    }

    /// Check if any subscribers are connected.
    pub fn has_subscribers(&self) -> bool {
        self.subscriber_count.load(Ordering::SeqCst) > 0
    }

    /// Get the current signal level.
    pub fn signal_level(&self) -> f32 {
        f32::from_bits(self.signal_level.load(Ordering::Relaxed))
    }

    /// Set the current signal level.
    pub fn set_signal_level(&self, level: f32) {
        self.signal_level.store(level.to_bits(), Ordering::Relaxed);
    }

    /// Start the tuner reader task.
    ///
    /// This spawns a background task that reads TS data from the tuner
    /// and broadcasts it to all subscribers.
    pub async fn start_reader<R>(self: &Arc<Self>, mut reader: R)
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        if self.is_running.swap(true, Ordering::SeqCst) {
            // Already running
            return;
        }

        let shared = Arc::clone(self);

        let handle = tokio::spawn(async move {
            info!("Starting tuner reader for {:?}", shared.key);

            let mut buf = vec![0u8; TS_CHUNK_SIZE];

            loop {
                // Check if we still have subscribers
                if !shared.has_subscribers() {
                    debug!("No more subscribers, stopping reader for {:?}", shared.key);
                    break;
                }

                // Read TS data from the tuner
                let result = {
                    let mut pinned = Pin::new(&mut reader);
                    poll_read_async(&mut pinned, &mut buf).await
                };

                match result {
                    Ok(0) => {
                        debug!("EOF from tuner {:?}", shared.key);
                        break;
                    }
                    Ok(n) => {
                        trace!("Read {} bytes from tuner {:?}", n, shared.key);

                        // Increment packet count (n / 188 packets)
                        let packet_count = (n / 188) as u64;
                        if packet_count > 0 {
                            shared.increment_packet_count(packet_count);
                        }

                        // Update TS quality analyzer
                        {
                            let mut analyzer = shared.quality_analyzer.lock().await;
                            analyzer.analyze(&buf[..n]);
                        }

                        let data = Bytes::copy_from_slice(&buf[..n]);

                        // Broadcast to all subscribers
                        match shared.tx.send(data) {
                            Ok(count) => {
                                trace!("Broadcast {} bytes to {} receivers", n, count);
                            }
                            Err(_) => {
                                // No receivers, this is fine
                                trace!("No receivers for broadcast");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading from tuner {:?}: {}", shared.key, e);
                        break;
                    }
                }
            }

            shared.is_running.store(false, Ordering::SeqCst);
            info!("Tuner reader stopped for {:?}", shared.key);
        });

        *self.reader_handle.lock().await = Some(handle);
    }

    /// Stop the tuner reader task.
    pub async fn stop_reader(&self) {
        info!("[SharedTuner] Stopping reader for {:?}...", self.key);
        
        // Signal the reader task to stop
        self.is_running.store(false, Ordering::SeqCst);

        // Wait for the reader task to finish (with timeout)
        if let Ok(mut guard) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.reader_handle.lock()
        ).await {
            if let Some(handle) = guard.take() {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    handle
                ).await {
                    Ok(_) => {
                        info!("[SharedTuner] Reader task completed gracefully for {:?}", self.key);
                    }
                    Err(_) => {
                        error!("[SharedTuner] Reader task timeout for {:?}, aborting", self.key);
                    }
                }
            }
        } else {
            error!("[SharedTuner] Failed to acquire reader handle lock for {:?}", self.key);
        }
        
        // Final ensure: mark as not running
        self.is_running.store(false, Ordering::SeqCst);
        
        info!("[SharedTuner] Reader stopped for {:?}", self.key);
    }

    /// Set the reader task handle (used by warm start).
    pub async fn set_reader_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.reader_handle.lock().await = Some(handle);
    }

    pub(crate) fn run_bondriver_reader_with_tuner(
        shared: Arc<Self>,
        tuner: BonDriverTuner,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) {
        shared.is_running.store(true, Ordering::SeqCst);
        info!("[SharedTuner] Using BonDriver: {}", tuner_path);

        // Set channel with retry for network-latency environments
        info!("[SharedTuner] Setting channel: space={}, channel={}", space, channel);
        let set_start = std::time::Instant::now();
        let mut set_attempts: u32 = 0;

        loop {
            set_attempts += 1;

            let set_channel_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tuner.set_channel(space, channel)
            }));

            match set_channel_result {
                Ok(Ok(())) => {
                    info!(
                        "[SharedTuner] Channel set successfully (attempt {}, elapsed {}ms)",
                        set_attempts,
                        set_start.elapsed().as_millis()
                    );
                    break;
                }
                Ok(Err(e)) => {
                    let elapsed = set_start.elapsed().as_millis() as u64;
                    let can_retry = elapsed < startup_config.set_channel_retry_timeout_ms;

                    if can_retry && e.kind() == std::io::ErrorKind::AddrNotAvailable {
                        warn!(
                            "[SharedTuner] SetChannel delayed/unavailable (attempt {}, elapsed {}ms): {}. Retrying...",
                            set_attempts,
                            elapsed,
                            e
                        );
                        std::thread::sleep(std::time::Duration::from_millis(startup_config.set_channel_retry_interval_ms));
                        continue;
                    }

                    if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                        warn!("[SharedTuner] Channel unavailable space={} channel={}: {}",
                              space, channel, e);
                    } else {
                        error!("[SharedTuner] Failed to set channel space={} channel={}: {} (kind: {:?})",
                               space, channel, e, e.kind());
                    }
                    shared.is_running.store(false, Ordering::SeqCst);

                    let err_msg = match e.kind() {
                        std::io::ErrorKind::AddrNotAvailable =>
                            "Channel not available - check space/channel number or signal is too weak".to_string(),
                        std::io::ErrorKind::Unsupported =>
                            "IBonDriver version does not support SetChannel2".to_string(),
                        _ => format!("SetChannel error: {}", e)
                    };

                    let _ = ready_tx.send(Err(err_msg));
                    return;
                }
                Err(panic_err) => {
                    error!("[SharedTuner] PANIC during SetChannel: {:?}", panic_err);
                    shared.is_running.store(false, Ordering::SeqCst);
                    let _ = ready_tx.send(Err("SetChannel caused panic - BonDriver may be corrupted".to_string()));
                    return;
                }
            }
        }

        // Wait for signal to become observable (network/driver latency can be several seconds)
        info!("[SharedTuner] Waiting for tuner signal lock...");
        let signal_start = std::time::Instant::now();
        let mut initial_signal = tuner.get_signal_level();

        while initial_signal <= 0.0 && signal_start.elapsed().as_millis() < startup_config.signal_wait_timeout_ms as u128 {
            std::thread::sleep(std::time::Duration::from_millis(startup_config.signal_poll_interval_ms));
            initial_signal = tuner.get_signal_level();
        }

        info!(
            "[SharedTuner] Initial signal level: {:.1}dB (elapsed {}ms)",
            initial_signal,
            signal_start.elapsed().as_millis()
        );

        // Purge any stale data from the buffer
        tuner.purge_ts_stream();

        // Short stabilization wait for new driver to have something in buffer
        std::thread::sleep(std::time::Duration::from_millis(500));

        // ===== B25 decoder init =====
        let b25_opt = DecoderOptions {
            strip: true,
            emm: true,
            simd: true,
            round: 4,
            enable_working_key: false,
        };

        let mut b25 = match B25Pipe::new(b25_opt) {
            Ok(d) => {
                info!("[SharedTuner] B25 decoder enabled");
                Some(d)
            }
            Err(e) => {
                error!("[SharedTuner] Failed to init B25 decoder: {}", e);
                error!("[SharedTuner] Falling back to raw TS streaming");
                None
            }
        };

        // Track decoder state
        let mut b25_needs_reset = false;
        let mut consecutive_b25_errors = 0;

        // Reset packet counter for the new channel
        shared.reset_packet_count();

        // Signal that we're ready
        info!("[SharedTuner] BonDriver ready, signaling...");
        let _ = ready_tx.send(Ok(()));

        info!("[SharedTuner] Reader task started for {:?}", shared.key);

        // Use a larger initial buffer, and expand dynamically if needed
        let mut buf = vec![0u8; TS_CHUNK_SIZE];
        let mut buf_size = TS_CHUNK_SIZE;
        let mut consecutive_empty = 0u64;
        let mut total_bytes_read = 0u64;
        let mut last_log_time = std::time::Instant::now();
        let mut last_status_log = std::time::Instant::now();
        let mut reader_first_read = true;
        let reader_start_time = std::time::Instant::now();
        let mut broadcast_send_errors: u64 = 0;
        let mut logo_collector = ChannelLogoCollector::new();

        loop {
            // Check if we should stop due to explicit stop signal
            if !shared.is_running.load(Ordering::SeqCst) {
                info!("[SharedTuner] BREAK: Stop signal received for {:?}", shared.key);
                break;
            }

            // Log status every 5 seconds for debugging
            if last_status_log.elapsed().as_secs() >= 5 {
                let level = tuner.get_signal_level();
                info!("[SharedTuner] LOOP_STATUS: total_bytes={}, consecutive_empty={}, signal={:.1}dB, subscribers={}, is_running={}, elapsed={}s",
                      total_bytes_read, consecutive_empty, level, shared.subscriber_count(), shared.is_running.load(Ordering::SeqCst), reader_start_time.elapsed().as_secs());
                last_status_log = std::time::Instant::now();
            }

            // Wait for TS data to be available
            let wait_result = tuner.wait_ts_stream(1000);
            if !wait_result {
                consecutive_empty = consecutive_empty.saturating_add(1);
                if consecutive_empty % 50 == 1 {
                    info!("[SharedTuner] wait_ts_stream returned false ({} times), total_bytes={}, elapsed={}ms",
                          consecutive_empty, total_bytes_read, reader_start_time.elapsed().as_millis());
                }
            }

            // Read TS data with panic safety
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tuner.get_ts_stream(&mut buf)
            })) {
                Ok(Ok((n, remaining))) => {
                    // Check if BonDriver is requesting more buffer space
                    if n > buf.len() {
                        // BonDriver returned a size larger than our current buffer
                        // Expand the buffer to accommodate this size, plus some headroom
                        let new_size = (n * 2).max(buf_size * 2).min(16 * 1024 * 1024); // Cap at 16MB
                        info!("[SharedTuner] Expanding buffer from {} to {} bytes due to BonDriver request: n={}",
                              buf_size, new_size, n);
                        buf.resize(new_size, 0);
                        buf_size = new_size;

                        // Retry with larger buffer
                        if remaining > 0 {
                            warn!("[SharedTuner] GetTsStream returned size {} exceeds buffer {}, remaining={}. Retrying with expanded buffer...",
                                  n, buf.len(), remaining);
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            continue;
                        }
                    }

                    // Clip the returned size to buffer size (safety measure)
                    let n = std::cmp::min(n, buf.len());

                    // Log at INFO level only if we got significant data
                    if n > 0 && n % 327680 == 0 {  // Log every 5MB
                        info!("[SharedTuner] GetTsStream: n={} bytes, remaining={}", n, remaining);
                    }

                    if n == 0 {
                        consecutive_empty = consecutive_empty.saturating_add(1);
                        if consecutive_empty == 1 {
                            warn!("[SharedTuner] First get_ts_stream returned 0 bytes after reading {} total bytes, remaining={}, elapsed={}ms, continuing to wait...",
                                  total_bytes_read, remaining, reader_start_time.elapsed().as_millis());
                        }
                        if reader_first_read && reader_start_time.elapsed().as_secs() < 30 {
                            if consecutive_empty % 100 == 1 && consecutive_empty > 1 {
                                let signal = tuner.get_signal_level();
                                debug!("[SharedTuner] Early startup: waiting for TS data ({} empty reads, {}s elapsed, signal={:.1}dB)",
                                       consecutive_empty, reader_start_time.elapsed().as_secs(), signal);
                            }
                        } else if consecutive_empty % 500 == 1 {
                            let signal = tuner.get_signal_level();
                            debug!("[SharedTuner] Still waiting for TS data after {} empty reads, total_bytes={}, signal={:.1}dB, elapsed={}ms",
                                   consecutive_empty, total_bytes_read, signal, reader_start_time.elapsed().as_millis());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }

                    // Got data!
                    if reader_first_read {
                        info!("[SharedTuner] FIRST_DATA_RECEIVED: {} bytes after {} empty reads, elapsed={}ms, STARTUP_SUCCESSFUL",
                              n, consecutive_empty, reader_start_time.elapsed().as_millis());
                        reader_first_read = false;
                    } else if consecutive_empty > 0 {
                        debug!("[SharedTuner] Got data after {} empty reads: {} bytes", consecutive_empty, n);
                    }
                    consecutive_empty = 0;
                    total_bytes_read += n as u64;

                    // Increment packet count
                    let packet_count = (n / 188) as u64;
                    if packet_count > 0 {
                        shared.increment_packet_count(packet_count);
                    }

                    // Broadcast to all subscribers
                    let raw = &buf[..n];

                    // Best-effort logo extraction from SDT/CDT stream.
                    logo_collector.process_ts_chunk(raw);

                    // Data validation before B25 decode (log only on first packet)
                    if reader_first_read && n > 0 {
                        // Safely log first few bytes
                        info!("[SharedTuner] First TS packet received: size={} bytes, has_b25_decoder={}", n, b25.is_some());
                    }

                    // B25 decode with panic safety
                    if let Some(b25_decoder) = &mut b25 {
                        if !b25_needs_reset {
                            // Wrap B25 push in panic safety
                            let push_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                b25_decoder.push(raw)
                            }));

                            match push_result {
                                Ok(Ok(decoded)) => {
                                    if decoded.is_empty() {
                                        consecutive_b25_errors = 0;
                                        continue;
                                    }

                                    consecutive_b25_errors = 0;

                                    let packet_count = (decoded.len() / 188) as u64;
                                    if packet_count > 0 {
                                        shared.increment_packet_count(packet_count);
                                    }

                                    let data = Bytes::from(decoded);

                                    match shared.tx.send(data) {
                                        Ok(_count) => {}
                                        Err(_e) => {
                                            broadcast_send_errors += 1;
                                            if broadcast_send_errors == 1 || broadcast_send_errors % 100 == 0 {
                                                warn!("[SharedTuner] Broadcast send failed ({} times total) for {:?} - no active receivers",
                                                      broadcast_send_errors, shared.key);
                                            }
                                        }
                                    }
                                }
                                Ok(Err(_)) => {
                                    consecutive_b25_errors += 1;
                                    // Log error count without error details (to avoid binary data in logs)
                                    if consecutive_b25_errors == 1 {
                                        warn!("[SharedTuner] B25 decode error detected");
                                    }

                                    if consecutive_b25_errors >= 10 {
                                        error!("[SharedTuner] Too many B25 errors, resetting decoder");
                                        b25_needs_reset = true;
                                    }

                                    let data = Bytes::copy_from_slice(raw);
                                    let _ = shared.tx.send(data);
                                }
                                Err(_panic_err) => {
                                    error!("[SharedTuner] PANIC in B25 decoder push - disabling decoder and falling back to raw TS");
                                    b25_needs_reset = true;

                                    // Fall back to raw TS
                                    let data = Bytes::copy_from_slice(raw);
                                    let _ = shared.tx.send(data);
                                }
                            }
                        } else {
                            // B25 decoder in error state, skip decode and use raw TS
                            let data = Bytes::copy_from_slice(raw);
                            let _ = shared.tx.send(data);
                        }
                    } else {
                        // No B25 decoder, use raw TS
                        let data = Bytes::copy_from_slice(raw);
                        let _ = shared.tx.send(data);
                    }

                    // Update signal level and log periodically
                    if last_log_time.elapsed().as_secs() >= 5 {
                        let level = tuner.get_signal_level();
                        shared.set_signal_level(level);
                        info!("[SharedTuner] {:?}: {} bytes sent, signal={:.1}dB",
                              shared.key, total_bytes_read, level);
                        last_log_time = std::time::Instant::now();
                    }
                }
                Ok(Err(e)) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        consecutive_empty = consecutive_empty.saturating_add(1);
                        if consecutive_empty % 50 == 1 && !reader_first_read {
                            info!("[SharedTuner] get_ts_stream WouldBlock ({} times), total_bytes={}", consecutive_empty, total_bytes_read);
                        }
                        let max_attempts = if reader_first_read { 40000 } else { 1000 };
                        if consecutive_empty > max_attempts {
                            error!("[SharedTuner] Too many WouldBlock errors ({} times), stopping reader for {:?}", consecutive_empty, shared.key);
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }

                    if reader_first_read && reader_start_time.elapsed().as_secs() < 30 {
                        warn!("[SharedTuner] Early startup error (ignored): {} (kind={:?}), elapsed={}s, continuing to wait",
                              e, e.kind(), reader_start_time.elapsed().as_secs());
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }

                    warn!("[SharedTuner] Error reading TS data: {} (kind={:?}), total_bytes={}", e, e.kind(), total_bytes_read);
                    consecutive_empty = consecutive_empty.saturating_add(1);
                    if consecutive_empty > 1000 {
                        error!("[SharedTuner] Too many consecutive errors ({} times), stopping reader for {:?}", consecutive_empty, shared.key);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(panic_err) => {
                    error!("[SharedTuner] PANIC during get_ts_stream: {:?}", panic_err);
                    shared.is_running.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }

        shared.is_running.store(false, Ordering::SeqCst);
        info!("[SharedTuner] Reader task stopped for {:?}, total bytes: {}", shared.key, total_bytes_read);
    }

    /// Start reading from a BonDriver.
    ///
    /// This opens the BonDriver, sets the channel, and starts a background task
    /// that reads TS data and broadcasts it to all subscribers.
    /// If the reader is already running, it will stop it and restart with new channel.
    pub async fn start_bondriver_reader(
        self: &Arc<Self>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
    ) -> Result<(), std::io::Error> {
        // Check if reader is already running and stop it properly
        if self.is_running() {
            info!("[SharedTuner] Stopping existing reader for {:?} before restart", self.key);
            self.is_running.store(false, Ordering::SeqCst);
            
            // Wait for the reader task to fully complete
            // This is critical to ensure BonDriver is fully closed before opening a new one
            let mut wait_attempts = 0;
            const MAX_WAIT_ATTEMPTS: u32 = 150;  // 15 seconds (150 * 100ms)
            
            loop {
                // Try to take the handle and wait for it to finish
                {
                    let mut handle_lock = self.reader_handle.lock().await;
                    if let Some(handle) = handle_lock.take() {
                        // We have the handle - wait for it to complete
                        drop(handle_lock);  // Release lock before awaiting
                        
                        debug!("[SharedTuner] Waiting for reader task to finish (attempt {}/{})", 
                               wait_attempts + 1, MAX_WAIT_ATTEMPTS);
                        
                        // Wait with a short timeout so we can log progress
                        match tokio::time::timeout(Duration::from_millis(500), handle).await {
                            Ok(_) => {
                                info!("[SharedTuner] Reader task finished cleanly");
                                break;
                            }
                            Err(_) => {
                                // Timeout - task might still be running
                                warn!("[SharedTuner] Reader task still running after 500ms");
                                wait_attempts += 5;  // Count as multiple attempts
                                if wait_attempts >= MAX_WAIT_ATTEMPTS {
                                    error!("[SharedTuner] Giving up on waiting for reader task");
                                    break;
                                }
                                continue;
                            }
                        }
                    } else {
                        // No handle in the lock - task already finished
                        break;
                    }
                }
            }
            
            // Extra safety: wait a bit for any cleanup
            tokio::time::sleep(Duration::from_millis(200)).await;
            
            info!("[SharedTuner] Old reader fully stopped, starting new reader for {:?}", self.key);
        }

        let shared = Arc::clone(self);
        info!("[SharedTuner] Starting BonDriver reader for {:?}", self.key);

        // Use a oneshot channel to signal when the reader is ready
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        // Spawn a single blocking task that handles everything:
        // - Opens the BonDriver
        // - Sets the channel
        // - Reads TS data in a loop
        // - Broadcasts data to subscribers
        // BonDriverTuner is not Send, so all operations must be in the same thread.
        let handle = tokio::task::spawn_blocking(move || {
            // Wrap everything in catch_unwind to prevent panic from crashing the process
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Open BonDriver
                info!("[SharedTuner] Opening BonDriver: {}", tuner_path);
                let tuner = match BonDriverTuner::new(&tuner_path) {
                    Ok(t) => {
                        info!("[SharedTuner] BonDriver created successfully for {}", tuner_path);
                        t
                    },
                    Err(e) => {
                        error!("[SharedTuner] Failed to create/open BonDriver {}: {} (kind: {:?})", 
                               tuner_path, e, e.kind());
                        shared.is_running.store(false, Ordering::SeqCst);
                        let err_msg = match e.kind() {
                            std::io::ErrorKind::NotFound => 
                                format!("BonDriver not found or cannot load: {}", e),
                            std::io::ErrorKind::ConnectionRefused =>
                                format!("Failed to open tuner (may be in use or hardware issue): {}", e),
                            _ => format!("BonDriver error: {}", e)
                        };
                        let _ = ready_tx.send(Err(err_msg));
                        return;
                    }
                };
                SharedTuner::run_bondriver_reader_with_tuner(
                    Arc::clone(&shared),
                    tuner,
                    tuner_path.clone(),
                    space,
                    channel,
                    startup_config,
                    ready_tx,
                );
            }));
            
            // Handle panic at top level
            match result {
                Ok(_) => {
                    info!("[SharedTuner] Reader task completed normally");
                }
                Err(panic_err) => {
                    error!("[SharedTuner] CRITICAL PANIC in reader task: {:?}", panic_err);
                    shared.is_running.store(false, Ordering::SeqCst);
                }
            }
        });

        // Store the handle and spawn a cleanup task
        *self.reader_handle.lock().await = Some(handle);
        
        // Wait for the reader to signal it's ready (BonDriver opened, channel set)
        match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
            Ok(Ok(Ok(()))) => {
                info!("[SharedTuner] Reader ready for {:?}", self.key);
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                let kind = if e.contains("Channel not available") {
                    std::io::ErrorKind::AddrNotAvailable
                } else {
                    std::io::ErrorKind::Other
                };

                if kind == std::io::ErrorKind::AddrNotAvailable {
                    warn!("[SharedTuner] Reader failed to start: {}", e);
                } else {
                    error!("[SharedTuner] Reader failed to start: {}", e);
                }

                Err(std::io::Error::new(kind, e))
            }
            Ok(Err(_)) => {
                error!("[SharedTuner] Reader channel closed unexpectedly");
                Err(std::io::Error::new(std::io::ErrorKind::Other, "Reader channel closed"))
            }
            Err(_) => {
                error!("[SharedTuner] Timeout waiting for reader to start");
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Timeout waiting for reader"))
            }
        }
    }

    /// Check if the reader is running.
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }
}

/// Helper function to poll an AsyncBufRead as a future.
async fn poll_read_async<R>(reader: &mut Pin<&mut R>, buf: &mut [u8]) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    use futures_util::AsyncReadExt;
    reader.read(buf).await
}

impl Drop for SharedTuner {
    fn drop(&mut self) {
        debug!("SharedTuner dropped for {:?}", self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscriber_count() {
        let key = ChannelKey::simple("/dev/pt3video0", 13);
        let shared = SharedTuner::new(key, 2);

        assert_eq!(shared.subscriber_count(), 0);
        assert!(!shared.has_subscribers());

        let _rx1 = shared.subscribe();
        assert_eq!(shared.subscriber_count(), 1);
        assert!(shared.has_subscribers());

        let _rx2 = shared.subscribe();
        assert_eq!(shared.subscriber_count(), 2);

        shared.unsubscribe();
        assert_eq!(shared.subscriber_count(), 1);
    }

    #[test]
    fn test_signal_level() {
        let key = ChannelKey::simple("/dev/pt3video0", 13);
        let shared = SharedTuner::new(key, 2);

        shared.set_signal_level(23.5);
        assert!((shared.signal_level() - 23.5).abs() < 0.001);
    }
}
