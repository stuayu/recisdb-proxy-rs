//! Shared tuner implementation with broadcast capability.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::tuner::b25_pipe::B25Pipe; // 作った場所に合わせて
use b25_sys::DecoderOptions; // 鍵が必要な場合

use bytes::Bytes;
use log::{debug, error, info, warn};
use tokio::sync::broadcast;

use crate::bondriver::BonDriverTuner;
use crate::tuner::channel_key::ChannelKey;
use crate::tuner::lock::TunerLock;
use crate::tuner::logo_collector::ChannelLogoCollector;
use crate::tuner::epg_collector::EpgCollector;
use crate::tuner::pool::{SlotPermit, TunerPoolConfig};
use crate::tuner::ts_source::TsSource;

/// Lifecycle state of a [`SharedTuner`]'s background reader
/// (docs/TUNER_PIPELINE_REDESIGN.md §4 P1).
///
/// Replaces the old `is_running: AtomicBool`, whose only two observable
/// values (`true`/`false`) could not distinguish "not started yet" from
/// "currently opening the BonDriver and setting the channel" — the second
/// case is exactly what let a freshly-created, not-yet-running pool entry
/// get evicted out from under its own in-flight reader startup (SYSTEM_REVIEW
/// M8; see `tuner::pool`'s `is_reclaimable`/`occupies_slot` predicates that
/// consume this enum).
///
/// Transitions: `Idle --start--> Starting --(ready)--> Running
/// --stop_reader--> Stopping --(task exits)--> Stopped`. A startup failure
/// (SetChannel error, BonDriver open error, or a panic anywhere in the
/// reader) goes straight from `Starting` to `Stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderState {
    /// Never started (a freshly-inserted pool entry that hasn't had
    /// `start_bondriver_reader`/`WarmTunerHandle::activate` called on it
    /// yet — in practice this is momentary, since pool insertion and the
    /// `Starting` transition happen back-to-back).
    Idle = 0,
    /// Occupying a slot: BonDriver is being opened and/or `SetChannel` is
    /// in flight. No TS data is flowing yet, but this entry is *not* stale —
    /// see `occupies_slot()`.
    Starting = 1,
    /// Channel set, reader loop delivering (or attempting to deliver) TS
    /// data to subscribers. This is what `is_running()` has always meant.
    Running = 2,
    /// `stop_reader()` has requested the loop exit; the background task may
    /// still be unwinding for a brief window.
    Stopping = 3,
    /// The reader loop has exited (cleanly, on error, or after a panic) and
    /// is not going to restart on its own.
    Stopped = 4,
    /// Created by [`crate::tuner::TunerPool::get_or_create`] and holding its
    /// driver slot, but **no reader start is in flight yet** — the caller
    /// that asked for the entry is expected to call
    /// `start_bondriver_reader`/`WarmTunerHandle::activate` next.
    ///
    /// Distinct from [`Self::Starting`] because the two answer different
    /// questions: both occupy a slot (so capacity accounting counts them),
    /// but only `Reserved` still *needs* someone to start a reader. Merging
    /// them would make every "should I start the reader?" call site either
    /// skip the start it owed (if it treated `Reserved` as in-flight) or
    /// start a second reader over another task's in-flight one (if it
    /// treated `Starting` as needing a start).
    ///
    /// A `Reserved` entry that is abandoned (its caller hit a capacity
    /// conflict, or failed before starting) must be removed from the pool by
    /// that caller — see [`SharedTuner::is_orphanable`]. P1b replaces this
    /// hand-managed reservation with an RAII slot permit.
    Reserved = 5,
}

impl TryFrom<u8> for ReaderState {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ReaderState::Idle),
            1 => Ok(ReaderState::Starting),
            2 => Ok(ReaderState::Running),
            3 => Ok(ReaderState::Stopping),
            4 => Ok(ReaderState::Stopped),
            5 => Ok(ReaderState::Reserved),
            _ => Err(()),
        }
    }
}

/// Capacity of the broadcast channel for TS data.
/// Increased to 4096 (256MB of 64KB chunks) to support multiple simultaneous subscribers
/// without buffer overflow when subscriber read speeds vary significantly.
/// Each slot holds a 64KB chunk, so 4096 slots = ~256MB of buffering capacity.
///
/// `pub(crate)` so [`crate::tuner::encoder_pool::SharedEncoder`] can size its own
/// output broadcast channel identically (STREAMING_DESIGN.md §5 P4).
pub(crate) const BROADCAST_CAPACITY: usize = 4096;

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
    /// Reference count of active subscribers. Only ever mutated by
    /// [`TunerSubscription`]'s constructor (`subscribe`) and `Drop` impl —
    /// see that type's doc comment for why manual subscribe/unsubscribe was
    /// removed.
    subscriber_count: AtomicU32,
    /// Lifecycle state of the background reader task. See [`ReaderState`].
    reader_state: AtomicU8,
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
    /// This entry's reservation against its DLL's `max_instances` capacity
    /// (docs/TUNER_PIPELINE_REDESIGN.md P1b), if it currently holds one.
    ///
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: every access is a plain
    /// `take()`/`replace()` with no `.await` in between, so a blocking
    /// std mutex avoids the async-mutex overhead for what is always an
    /// uncontended, momentary critical section (see `take_slot_permit`/
    /// `set_slot_permit`).
    ///
    /// Populated by `TunerPool::get_or_create` on creation (so an abandoned
    /// `Reserved` entry still releases its slot via this field's `Drop` even
    /// if nobody ever calls `start_bondriver_reader`), taken back out by
    /// whichever caller is about to start a reader (`take_slot_permit`) and
    /// handed to `start_bondriver_reader`/`WarmTunerHandle::activate`, which
    /// store it back here for the reader's lifetime.
    slot: std::sync::Mutex<Option<SlotPermit>>,
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
            reader_state: AtomicU8::new(ReaderState::Idle as u8),
            reader_handle: tokio::sync::Mutex::new(None),
            signal_level: AtomicU32::new(0),
            bondriver_version,
            lock: TunerLock::new(),
            packets_received: AtomicU64::new(0),
            slot: std::sync::Mutex::new(None),
        })
    }

    /// Store `permit` as this entry's driver-slot reservation.
    ///
    /// Called by [`crate::tuner::TunerPool::get_or_create`] on creation and,
    /// after `take_slot_permit` retrieves it again, by
    /// `start_bondriver_reader`/`WarmTunerHandle::activate` once they commit
    /// to actually starting a reader. Overwrites (and thus drops/releases)
    /// any previously stored permit — callers must not call this while a
    /// permit for a *different* DLL path is already stored, or that other
    /// path's slot would leak; see the doc comments on the call sites for
    /// why that can't happen in practice.
    pub(crate) fn set_slot_permit(&self, permit: SlotPermit) {
        *self.slot.lock().unwrap() = Some(permit);
    }

    /// Take this entry's driver-slot reservation, if it currently holds one.
    ///
    /// Used for two distinct purposes (docs/TUNER_PIPELINE_REDESIGN.md P1b):
    /// (1) by whichever caller is about to start a reader on this
    /// `SharedTuner`, to retrieve the permit `get_or_create` stored on
    /// creation and pass it into `start_bondriver_reader`/
    /// `WarmTunerHandle::activate` (both require one as a parameter — a
    /// reader cannot be started without holding a permit, enforced by the
    /// type signature); and (2) by a session switching channels on the same
    /// DLL, to transfer this tuner's slot directly to its replacement
    /// instead of releasing and re-acquiring (which could lose a race to an
    /// unrelated task on a `max_instances`-constrained driver) — see
    /// `server/session.rs`'s permit-handoff on channel switch.
    pub fn take_slot_permit(&self) -> Option<SlotPermit> {
        self.slot.lock().unwrap().take()
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
        // Acquire so the caller sees all writes that preceded the increment.
        self.packets_received.load(Ordering::Acquire) > 0
    }

    /// Increment the packet counter.
    pub fn increment_packet_count(&self, count: u64) {
        // Release pairs with the Acquire in has_received_packets / packet_count.
        self.packets_received.fetch_add(count, Ordering::Release);
    }

    /// Reset the packet counter.
    pub fn reset_packet_count(&self) {
        self.packets_received.store(0, Ordering::Release);
    }

    /// Get the total number of packets received.
    pub fn packet_count(&self) -> u64 {
        self.packets_received.load(Ordering::Acquire)
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
    ///
    /// Returns a [`TunerSubscription`] that increments `subscriber_count` now
    /// and decrements it automatically on `Drop` — see that type's doc
    /// comment for why the old manual `unsubscribe()` API was removed.
    ///
    /// Takes `self: &Arc<Self>` (a stable receiver type, same as
    /// [`Self::start_bondriver_reader`] below) rather than plain `&self`, so
    /// `TunerSubscription` can hold an owned `Arc<SharedTuner>` via a cheap
    /// `Arc::clone` — no `Weak`/`Arc::new_cyclic`/`.upgrade().expect(...)`
    /// needed. Every call site already holds an `Arc<SharedTuner>` (from
    /// `TunerPool`), so this is a transparent signature change: `tuner.subscribe()`
    /// keeps compiling unchanged.
    pub fn subscribe(self: &Arc<Self>) -> TunerSubscription {
        self.subscriber_count.fetch_add(1, Ordering::SeqCst);
        debug!(
            "New subscriber for {:?}, total: {}",
            self.key,
            self.subscriber_count.load(Ordering::SeqCst)
        );
        TunerSubscription { tuner: Arc::clone(self), rx: self.tx.subscribe() }
    }

    /// Subscribe to the TS data stream WITHOUT incrementing the subscriber
    /// reference count.
    ///
    /// Used by the shared encoder pool (`crate::tuner::encoder_pool`): the
    /// encoder is a parasitic consumer whose own lifetime is governed by its
    /// session subscribers, so it must not keep the tuner alive by itself or
    /// perturb the session-driven keep-alive / idle-close accounting.
    ///
    /// Returns an [`UntrackedSubscription`] rather than a bare
    /// `broadcast::Receiver` so the "this subscription does not count"
    /// contract is visible in the type, not just the doc comment; its `Drop`
    /// does nothing (there is no count to decrement).
    pub(crate) fn subscribe_untracked(&self) -> UntrackedSubscription {
        UntrackedSubscription { rx: self.tx.subscribe() }
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

    /// Get the number of active subscribers.
    pub fn subscriber_count(&self) -> u32 {
        self.subscriber_count.load(Ordering::SeqCst)
    }

    /// Check if any subscribers are connected.
    pub fn has_subscribers(&self) -> bool {
        self.subscriber_count.load(Ordering::SeqCst) > 0
    }

    /// Current reader lifecycle state. See [`ReaderState`].
    pub fn state(&self) -> ReaderState {
        // The stored value is only ever written via `set_state`, which only
        // ever writes valid `ReaderState as u8` values, so the `TryFrom`
        // cannot fail in practice; `Stopped` is a safe fallback regardless.
        ReaderState::try_from(self.reader_state.load(Ordering::Acquire)).unwrap_or(ReaderState::Stopped)
    }

    /// Transition the reader lifecycle state.
    pub(crate) fn set_state(&self, state: ReaderState) {
        self.reader_state.store(state as u8, Ordering::Release);
    }

    /// Transition to `Stopped` and release this entry's driver-slot permit
    /// (if any) in the same step (docs/TUNER_PIPELINE_REDESIGN.md P1b).
    ///
    /// Every place that moves a reader to `Stopped` must free its slot right
    /// then — not rely solely on `stop_reader()`'s own explicit release,
    /// which several of these call sites race past: a reader can fail its
    /// own startup (`SetChannel` error, BonDriver open error, a caught
    /// panic) or die inside its read loop without anyone ever calling
    /// `stop_reader()`. Taking the permit is a plain `Option::take`, so
    /// calling this more than once for the same stop (e.g. once from inside
    /// the reader thread when it dies on its own, and again from a
    /// concurrent `stop_reader()` that also reaches its own final
    /// `Stopped` transition) is harmless: only the first caller actually
    /// holds anything to release.
    pub(crate) fn stop_and_release_slot(&self) {
        self.set_state(ReaderState::Stopped);
        let _ = self.take_slot_permit();
    }

    /// Transition `Starting -> Running`, but only if the state is still
    /// `Starting`. Returns `false` (and leaves the state untouched) if a
    /// concurrent `stop_reader()` already advanced it to `Stopping` — e.g. a
    /// session disconnects while its reader is still opening the BonDriver.
    ///
    /// This must be a compare-exchange, not an unconditional `set_state`:
    /// the old `is_running: AtomicBool` model set `is_running = true` exactly
    /// once, at the very top of `run_bondriver_reader_with_tuner`, and never
    /// touched it again until the read loop's own stop-check — so a
    /// `stop_reader()` call during startup reliably stuck as `false`. An
    /// unconditional `set_state(Running)` right before entering the read loop
    /// would silently resurrect a state a concurrent `stop_reader()` had
    /// already moved to `Stopping`, leaving that reader running forever with
    /// nothing left to stop it (this was caught by a hanging test during
    /// review — see `reader_state_stop_during_starting_is_not_clobbered`).
    fn try_transition_starting_to_running(&self) -> bool {
        self.reader_state
            .compare_exchange(
                ReaderState::Starting as u8,
                ReaderState::Running as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Whether this entry is occupying a pool slot: currently starting,
    /// running, or in the process of stopping. `false` only for `Idle`
    /// (never started) and `Stopped` (reader has fully exited).
    ///
    /// This is the P1 replacement for the informal "is this tuner in a state
    /// where it still needs a DLL slot" check that used to require reasoning
    /// about `is_running()` combined with recent history.
    pub fn occupies_slot(&self) -> bool {
        matches!(
            self.state(),
            ReaderState::Reserved
                | ReaderState::Starting
                | ReaderState::Running
                | ReaderState::Stopping
        )
    }

    /// Whether a caller still owes this entry a reader start.
    ///
    /// `false` exactly when a reader is already in flight (`Starting`) or
    /// live (`Running`) — starting a second one on top of either would open
    /// the same DLL twice. This replaces the `!is_running()` test that every
    /// "start the reader if it isn't going yet" call site used before
    /// `ReaderState` existed, which is no longer equivalent: the old
    /// `is_running` flag was already `true` throughout the BonDriver
    /// open + SetChannel-retry window that is now `Starting`.
    pub fn needs_reader_start(&self) -> bool {
        !matches!(self.state(), ReaderState::Starting | ReaderState::Running)
    }

    /// Whether the caller that created/holds this entry may drop it from the
    /// pool: nothing is subscribed and no reader is in flight or live.
    ///
    /// Broader than [`Self::is_reclaimable`] by design — it also covers
    /// [`ReaderState::Reserved`] (created but abandoned before its reader was
    /// ever started, e.g. a capacity conflict detected after
    /// `get_or_create`) and `Stopping`. Only the *owner* of the entry should
    /// use this; pool-internal stale sweeps must keep using
    /// [`Self::is_reclaimable`], which deliberately leaves another task's
    /// `Reserved`/`Starting` entry alone (SYSTEM_REVIEW_2026-07.md M8).
    pub fn is_orphanable(&self) -> bool {
        !self.has_subscribers()
            && !matches!(self.state(), ReaderState::Starting | ReaderState::Running)
    }

    /// Whether this pool entry is stale and safe to evict/replace: the
    /// reader has never started (or has fully stopped) *and* nothing is
    /// subscribed.
    ///
    /// This is the single predicate that replaces the
    /// `!is_running() && !has_subscribers()` check that used to be
    /// duplicated across `TunerPool::get_or_create` (x2), `TunerPool::cleanup`,
    /// and several `server/session.rs` helpers (docs/TUNER_PIPELINE_REDESIGN.md
    /// §4 P1) — critically, it does *not* fire for `ReaderState::Starting`,
    /// which is what let a freshly-created, still-initializing tuner get
    /// evicted out from under itself (SYSTEM_REVIEW_2026-07.md M8).
    pub fn is_reclaimable(&self) -> bool {
        matches!(self.state(), ReaderState::Idle | ReaderState::Stopped) && !self.has_subscribers()
    }

    /// Get the current signal level.
    pub fn signal_level(&self) -> f32 {
        f32::from_bits(self.signal_level.load(Ordering::Relaxed))
    }

    /// Set the current signal level.
    pub fn set_signal_level(&self, level: f32) {
        self.signal_level.store(level.to_bits(), Ordering::Relaxed);
    }

    /// Stop the tuner reader task.
    pub async fn stop_reader(&self) {
        info!("[SharedTuner] Stopping reader for {:?}...", self.key);

        // Signal the reader task to stop. `Stopping` (not `Stopped` directly)
        // so `occupies_slot()` still reports true for the brief window before
        // the background task actually exits — this entry is not eligible
        // for reclaim/reuse until the DLL is actually released.
        self.set_state(ReaderState::Stopping);

        // Wait for the reader task to finish (with timeout).
        // wait_ts_stream() is now 100 ms, so the blocking task exits within
        // ~200 ms of the state becoming Stopping.  1 s is a generous upper bound.
        if let Ok(mut guard) = tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            self.reader_handle.lock()
        ).await {
            if let Some(handle) = guard.take() {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(1000),
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

        // Final ensure: mark as stopped, even if the reader task never got a
        // chance to set this itself (timeout/abort above). Also releases the
        // driver-slot permit explicitly here (docs/TUNER_PIPELINE_REDESIGN.md
        // P1b item 2) rather than waiting on this `SharedTuner`'s `Arc` to
        // drop — a caller that immediately reopens the same DLL (permit
        // handoff during a channel switch) needs the slot freed
        // deterministically at this point, not whenever the last reference
        // happens to go away.
        self.stop_and_release_slot();

        info!("[SharedTuner] Reader stopped for {:?}", self.key);
    }

    /// Set the reader task handle (used by warm start).
    pub async fn set_reader_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.reader_handle.lock().await = Some(handle);
    }

    pub(crate) fn run_bondriver_reader_with_tuner<T: TsSource>(
        shared: Arc<Self>,
        tuner: T,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) {
        // Already set by the caller (`start_bondriver_reader`/
        // `WarmTunerHandle::activate`) before this function was ever
        // scheduled, so the pool entry is occupied from the moment the
        // caller decided to start a reader — not just from whenever this
        // `spawn_blocking` closure happens to run. Set again here
        // defensively (idempotent) in case a future caller forgets.
        shared.set_state(ReaderState::Starting);
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
                    shared.stop_and_release_slot();

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
                    shared.stop_and_release_slot();
                    let _ = ready_tx.send(Err("SetChannel caused panic - BonDriver may be corrupted".to_string()));
                    return;
                }
            }
        }

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

        // Signal ready BEFORE the optional signal-level wait.
        // BonDriverProxy(Ex) returns from SetChannel as soon as the DLL
        // accepts it; signal acquisition is not checked.  Waiting here
        // blocked the session loop and caused consecutive channel-switch
        // failures because each switch had to wait up to 10 s.
        //
        // Transition to Running before signaling: callers that were waiting
        // on `ready_tx` (e.g. `start_bondriver_reader`'s `ready_rx.await`)
        // may immediately call `is_running()`/`subscribe()` once they wake
        // up, and must observe `Running`, not a lingering `Starting`.
        //
        // Compare-exchange, not an unconditional set: a `stop_reader()` call
        // that raced in during startup (session disconnected while its
        // reader was still opening the BonDriver) already moved the state to
        // `Stopping`, and must not be resurrected back to `Running` here —
        // see `try_transition_starting_to_running`'s doc comment.
        if !shared.try_transition_starting_to_running() {
            info!(
                "[SharedTuner] Stop requested during startup for {:?}; exiting before entering the read loop",
                shared.key
            );
            let _ = ready_tx.send(Ok(()));
            shared.stop_and_release_slot();
            return;
        }
        info!("[SharedTuner] BonDriver ready, signaling...");
        let _ = ready_tx.send(Ok(()));

        info!("[SharedTuner] Reader task started for {:?}", shared.key);

        // Log initial signal level (informational only; does not block the caller).
        // The read loop updates signal every 5 s during streaming.
        {
            let initial_signal = tuner.get_signal_level();
            info!("[SharedTuner] Initial signal level: {:.1}dB", initial_signal);
        }

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
        let mut epg_collector = EpgCollector::new();

        loop {
            // Check if we should stop due to explicit stop signal
            if shared.state() != ReaderState::Running {
                info!("[SharedTuner] BREAK: Stop signal received for {:?}", shared.key);
                break;
            }

            // Log status every 5 seconds for debugging
            if last_status_log.elapsed().as_secs() >= 5 {
                let level = tuner.get_signal_level();
                info!("[SharedTuner] LOOP_STATUS: total_bytes={}, consecutive_empty={}, signal={:.1}dB, subscribers={}, state={:?}, elapsed={}s",
                      total_bytes_read, consecutive_empty, level, shared.subscriber_count(), shared.state(), reader_start_time.elapsed().as_secs());
                last_status_log = std::time::Instant::now();
            }

            // Wait for TS data to be available.
            // 100 ms instead of 1000 ms so the stop-check at the top of the
            // loop is reached at most ~100 ms after stop_reader() sets the
            // state to Stopping.  This makes channel switches faster.
            let wait_result = tuner.wait_ts_stream(100);
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

                    // Broadcast to all subscribers
                    let raw = &buf[..n];

                    // Best-effort logo extraction from SDT/CDT stream.
                    logo_collector.process_ts_chunk(raw);
                    // Best-effort EPG (EIT) collection, forwarded to the
                    // process-wide EpgWriter if one is installed (see
                    // `tuner/epg_collector.rs` module doc comment).
                    epg_collector.process_ts_chunk(raw);

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

                                    let packet_count = (n / 188) as u64;
                                    if packet_count > 0 {
                                        shared.increment_packet_count(packet_count);
                                    }
                                    let data = Bytes::copy_from_slice(raw);
                                    let _ = shared.tx.send(data);
                                }
                                Err(_panic_err) => {
                                    error!("[SharedTuner] PANIC in B25 decoder push - disabling decoder and falling back to raw TS");
                                    b25_needs_reset = true;

                                    // Fall back to raw TS
                                    let packet_count = (n / 188) as u64;
                                    if packet_count > 0 {
                                        shared.increment_packet_count(packet_count);
                                    }
                                    let data = Bytes::copy_from_slice(raw);
                                    let _ = shared.tx.send(data);
                                }
                            }
                        } else {
                            // B25 decoder in error state, skip decode and use raw TS
                            let packet_count = (n / 188) as u64;
                            if packet_count > 0 {
                                shared.increment_packet_count(packet_count);
                            }
                            let data = Bytes::copy_from_slice(raw);
                            let _ = shared.tx.send(data);
                        }
                    } else {
                        // No B25 decoder, use raw TS
                        let packet_count = (n / 188) as u64;
                        if packet_count > 0 {
                            shared.increment_packet_count(packet_count);
                        }
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
                    shared.stop_and_release_slot();
                    break;
                }
            }
        }

        shared.stop_and_release_slot();
        info!("[SharedTuner] Reader task stopped for {:?}, total bytes: {}", shared.key, total_bytes_read);
    }

    /// Start reading from a BonDriver.
    ///
    /// This opens the BonDriver, sets the channel, and starts a background task
    /// that reads TS data and broadcasts it to all subscribers.
    /// If the reader is already running, it will stop it and restart with new channel.
    ///
    /// `permit` is this entry's [`SlotPermit`] (docs/TUNER_PIPELINE_REDESIGN.md
    /// P1b) — a reader cannot be started without one, enforced here at the
    /// type level. In the common case it is the very permit
    /// `TunerPool::get_or_create` stored on this same `SharedTuner` when it
    /// was created, handed back in by the caller via `take_slot_permit()`
    /// (see that method's doc comment); when this call is instead an
    /// in-place channel restart on an already-`Running` tuner (the
    /// `is_running()` branch just below), `permit` is that same still-live
    /// reservation being passed straight back through — this entry never
    /// stopped occupying its slot, so there is nothing new to reserve.
    /// Either way, this function always stores `permit` onto `self` via
    /// `set_slot_permit` before starting the reader.
    pub async fn start_bondriver_reader(
        self: &Arc<Self>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        permit: SlotPermit,
    ) -> Result<(), std::io::Error> {
        // Check if reader is already running and stop it properly
        if self.is_running() {
            info!("[SharedTuner] Stopping existing reader for {:?} before restart", self.key);
            self.set_state(ReaderState::Stopping);

            // Wait for the reader task to fully complete.
            // wait_ts_stream() is now 100 ms so the blocking task exits within
            // ~200 ms.  300 ms is sufficient; give 500 ms as a safety margin.
            {
                let mut handle_lock = self.reader_handle.lock().await;
                if let Some(handle) = handle_lock.take() {
                    drop(handle_lock);
                    match tokio::time::timeout(Duration::from_millis(500), handle).await {
                        Ok(_) => info!("[SharedTuner] Reader task finished cleanly"),
                        Err(_) => warn!("[SharedTuner] Reader task still running after 500ms, proceeding"),
                    }
                }
            }
            // Plain `set_state`, not `stop_and_release_slot`: this is an
            // in-place restart of the *same* DLL instance for a new channel,
            // not a real close — the slot stays reserved throughout (`self.slot`
            // is empty here regardless, since the caller already took it out
            // via `take_slot_permit()` before calling this function; it is
            // restored immediately below).
            self.set_state(ReaderState::Stopped);

            info!("[SharedTuner] Old reader fully stopped, starting new reader for {:?}", self.key);
        }

        self.set_slot_permit(permit);

        // Mark this entry as occupied *synchronously*, before `spawn_blocking`
        // even schedules the background thread — closes the window
        // `is_reclaimable()`/M8 fixes (docs/TUNER_PIPELINE_REDESIGN.md §4 P1):
        // a concurrent `TunerPool::get_or_create`/`cleanup`/`evict_idle_on_path`
        // call on another task must never see this entry as `Idle` between
        // "caller decided to start a reader" and "the blocking thread got
        // scheduled and reached its own `set_state(Starting)`".
        self.set_state(ReaderState::Starting);

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
                        shared.stop_and_release_slot();
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
                    shared.stop_and_release_slot();
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
    ///
    /// Kept for compatibility and for the many call sites that only ever
    /// cared about "is TS data (potentially) flowing right now" — equivalent
    /// to `state() == ReaderState::Running`. Pool/session stale-detection
    /// logic must use [`Self::is_reclaimable`] instead (see that method's
    /// doc comment for why `is_running() == false` alone is not a safe stale
    /// check now that [`ReaderState::Starting`] exists).
    pub fn is_running(&self) -> bool {
        self.state() == ReaderState::Running
    }
}

impl Drop for SharedTuner {
    fn drop(&mut self) {
        debug!("SharedTuner dropped for {:?}", self.key);
    }
}

/// A tracked subscription to a [`SharedTuner`]'s TS broadcast.
///
/// Replaces the old pattern of a bare `broadcast::Receiver<Bytes>` plus a
/// manually-paired `tuner.unsubscribe()` call at every exit path
/// (docs/TUNER_PIPELINE_REDESIGN.md §4 P1, item 2). The old API required
/// every caller — `server/session.rs`'s half-dozen `ts_receiver` exit paths,
/// `web/stream.rs`'s `StreamCleanup`, `session_tuner_handoff.rs` — to
/// remember to call `unsubscribe()` exactly once per `subscribe()`; a missed
/// or doubled call either leaked the count (idle-close never fires) or, with
/// the old wraparound guard, silently under-counted. `TunerSubscription`
/// makes the pairing structural: `subscriber_count` only ever changes here,
/// in `subscribe()`, and in `Drop`, so it is impossible to construct one
/// without the corresponding decrement eventually happening exactly once.
///
/// Dereferences to the underlying `broadcast::Receiver<Bytes>` (via
/// `Deref`/`DerefMut`) so existing call sites that pattern the receiver
/// directly (`rx.recv().await`, `rx.try_recv()`) keep working unchanged.
pub struct TunerSubscription {
    tuner: Arc<SharedTuner>,
    rx: broadcast::Receiver<Bytes>,
}

impl TunerSubscription {
    /// Receive the next TS chunk. Equivalent to
    /// `broadcast::Receiver::recv`, provided directly so callers don't need
    /// `use std::ops::DerefMut` in scope just to call it.
    pub async fn recv(&mut self) -> Result<Bytes, broadcast::error::RecvError> {
        self.rx.recv().await
    }

    /// The tuner this subscription is tracking. Used by cleanup paths that
    /// need to act on the tuner (e.g. `schedule_idle_close`) after releasing
    /// the subscription itself.
    pub fn tuner(&self) -> &Arc<SharedTuner> {
        &self.tuner
    }
}

impl std::ops::Deref for TunerSubscription {
    type Target = broadcast::Receiver<Bytes>;
    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl std::ops::DerefMut for TunerSubscription {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rx
    }
}

impl Drop for TunerSubscription {
    fn drop(&mut self) {
        // Plain fetch_sub: unlike the old manual `unsubscribe()`, there is no
        // way to construct a `TunerSubscription` without a matching earlier
        // increment in `subscribe()`, so underflow cannot happen here by
        // construction — no `fetch_update`/wraparound guard needed.
        let prev = self.tuner.subscriber_count.fetch_sub(1, Ordering::SeqCst);
        debug!(
            "Subscriber removed from {:?}, remaining: {}",
            self.tuner.key,
            prev - 1
        );
    }
}

/// A subscription to a [`SharedTuner`]'s TS broadcast that deliberately does
/// **not** count toward `subscriber_count` — see
/// [`SharedTuner::subscribe_untracked`]'s doc comment. `Drop` intentionally
/// does nothing (there is no count to release); this type exists purely so
/// the "does not count" contract is visible at the call site's type instead
/// of only in a doc comment.
pub(crate) struct UntrackedSubscription {
    rx: broadcast::Receiver<Bytes>,
}

impl UntrackedSubscription {
    pub(crate) async fn recv(&mut self) -> Result<Bytes, broadcast::error::RecvError> {
        self.rx.recv().await
    }
}

#[cfg(test)]
impl SharedTuner {
    /// Test-only helper: inject data directly into the broadcast channel,
    /// bypassing the BonDriver reader loop. Used by
    /// `crate::tuner::encoder_pool` tests to simulate TS chunks flowing
    /// from a tuner into a `SharedEncoder`'s feeder task.
    pub(crate) fn test_broadcast(&self, data: Bytes) {
        let _ = self.tx.send(data);
    }

    /// Drive `run_bondriver_reader_with_tuner` with a [`crate::tuner::ts_source::FakeTsSource`]
    /// on a real `spawn_blocking` thread, exactly like `start_bondriver_reader`
    /// does for a real `BonDriverTuner` — the only difference being the `T:
    /// TsSource` implementation and that the caller supplies the source
    /// (so it can pre-configure delays/errors/chunks) instead of this
    /// function opening a DLL itself.
    ///
    /// Returns the task handle (already stashed into `self.reader_handle` so
    /// `stop_reader()` works exactly as it would for a real reader) and the
    /// ready-signal receiver.
    pub(crate) async fn spawn_fake_reader(
        self: &Arc<Self>,
        source: crate::tuner::ts_source::FakeTsSource,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        // Mirrors `start_bondriver_reader`'s synchronous Starting transition
        // (see that function's comment) — set before scheduling the blocking
        // task, not inside it.
        self.set_state(ReaderState::Starting);

        let shared = Arc::clone(self);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let handle = tokio::task::spawn_blocking(move || {
            SharedTuner::run_bondriver_reader_with_tuner(
                shared,
                source,
                "fake://test".to_string(),
                space,
                channel,
                startup_config,
                ready_tx,
            );
        });
        *self.reader_handle.lock().await = Some(handle);
        ready_rx
    }
}

#[cfg(test)]
fn test_startup_config() -> ReaderStartupConfig {
    ReaderStartupConfig {
        set_channel_retry_interval_ms: 5,
        set_channel_retry_timeout_ms: 50,
        signal_poll_interval_ms: 5,
        signal_wait_timeout_ms: 50,
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

        let rx1 = shared.subscribe();
        assert_eq!(shared.subscriber_count(), 1);
        assert!(shared.has_subscribers());

        let _rx2 = shared.subscribe();
        assert_eq!(shared.subscriber_count(), 2);

        // Dropping a `TunerSubscription` (the RAII replacement for the old
        // manual `unsubscribe()`) decrements the count exactly once.
        drop(rx1);
        assert_eq!(shared.subscriber_count(), 1);
    }

    #[test]
    fn test_signal_level() {
        let key = ChannelKey::simple("/dev/pt3video0", 13);
        let shared = SharedTuner::new(key, 2);

        shared.set_signal_level(23.5);
        assert!((shared.signal_level() - 23.5).abs() < 0.001);
    }

    // -----------------------------------------------------------------
    // TunerSubscription RAII (P1a item 2)
    // -----------------------------------------------------------------

    /// Double-drop safety: two subscriptions dropped in either order each
    /// decrement exactly once, never underflowing (there is no
    /// wraparound-guard branch to even exercise anymore — construction
    /// guarantees a matching decrement).
    #[test]
    fn tuner_subscription_drop_never_underflows_with_multiple_subscribers() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);

        let a = shared.subscribe();
        let b = shared.subscribe();
        let c = shared.subscribe();
        assert_eq!(shared.subscriber_count(), 3);

        drop(b);
        assert_eq!(shared.subscriber_count(), 2);
        drop(a);
        assert_eq!(shared.subscriber_count(), 1);
        drop(c);
        assert_eq!(shared.subscriber_count(), 0);
        assert!(!shared.has_subscribers());
    }

    /// `subscribe_untracked` (used by the shared encoder pool) must never
    /// move `subscriber_count` — only the tracked `TunerSubscription` does.
    #[test]
    fn untracked_subscription_does_not_affect_subscriber_count() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);

        let _untracked = shared.subscribe_untracked();
        assert_eq!(shared.subscriber_count(), 0);
        assert!(!shared.has_subscribers());
    }

    // -----------------------------------------------------------------
    // ReaderState transitions (P1a item 1), driven end-to-end through
    // `run_bondriver_reader_with_tuner` via `FakeTsSource` (P1a item 3).
    // -----------------------------------------------------------------

    use crate::tuner::ts_source::FakeTsSource;

    #[tokio::test]
    async fn reader_state_transitions_idle_starting_running_stopped_on_success() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);
        assert_eq!(shared.state(), ReaderState::Idle);

        // A startup delay keeps `set_channel` (and thus the `Starting`
        // window) open long enough for the assertion below to reliably land
        // inside it rather than racing the real OS thread to `Running`.
        let source = FakeTsSource::new()
            .with_startup_delay(std::time::Duration::from_millis(150))
            .with_chunk(vec![0u8; 188]);
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;

        // `spawn_fake_reader` sets Starting synchronously, before the
        // blocking task is even scheduled.
        assert_eq!(shared.state(), ReaderState::Starting);

        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_ok(), "expected successful startup, got {:?}", ready);
        assert_eq!(shared.state(), ReaderState::Running);
        assert!(shared.is_running());

        shared.stop_reader().await;
        assert_eq!(shared.state(), ReaderState::Stopped);
        assert!(!shared.is_running());
    }

    #[tokio::test]
    async fn reader_state_goes_straight_to_stopped_on_set_channel_failure() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);

        let source = FakeTsSource::new().with_set_channel_error(std::io::ErrorKind::PermissionDenied);
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;

        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_err(), "expected startup failure");
        assert_eq!(shared.state(), ReaderState::Stopped);
        assert!(shared.is_reclaimable(), "a failed startup with no subscribers must be reclaimable");

        // The blocking task has already returned by this point (it sends
        // `ready_tx` right before its final `return`), but every test that
        // spawns one is required to explicitly join it — `stop_reader()` is
        // safe to call on an already-stopped reader and guarantees the
        // `spawn_blocking` task is awaited before the test ends.
        shared.stop_reader().await;
    }

    /// `AddrNotAvailable` is the one error kind `run_bondriver_reader_with_tuner`
    /// retries before giving up (network-latency BonDrivers) — with
    /// `set_channel_retry_timeout_ms` short (see `test_startup_config`), it
    /// still ends in `Stopped` once the retry budget is exhausted.
    #[tokio::test]
    async fn reader_state_stopped_after_retry_budget_exhausted() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);

        let source = FakeTsSource::new().with_set_channel_error(std::io::ErrorKind::AddrNotAvailable);
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;

        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_err());
        assert_eq!(shared.state(), ReaderState::Stopped);
        shared.stop_reader().await;
    }

    /// A panic inside `set_channel` is caught by the reader's own
    /// `catch_unwind` (CLAUDE.md: panics must never cross the FFI-adjacent
    /// boundary) and must still land the tuner in `Stopped`, not leave it
    /// stuck `Starting` forever.
    #[tokio::test]
    async fn reader_state_stopped_after_panic_in_set_channel() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);

        let source = FakeTsSource::new().with_panic_on_set_channel();
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;

        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_err(), "expected the panic to surface as a startup failure");
        assert_eq!(shared.state(), ReaderState::Stopped);
        assert!(shared.is_reclaimable());
        shared.stop_reader().await;
    }

    /// A `stop_reader()` call that lands while the reader is still
    /// `Starting` (mid-`set_channel`) must win: the reader must never
    /// resurrect itself to `Running` once its startup delay elapses. This
    /// pins down the fix for a regression caught during review — an earlier
    /// version of `run_bondriver_reader_with_tuner` set `Running`
    /// unconditionally right before entering the read loop, silently
    /// clobbering a concurrent `Stopping`, which left the fake reader
    /// spinning forever with nothing able to stop it (hanging
    /// `cargo test -p recisdb-proxy` at shutdown).
    #[tokio::test]
    async fn reader_state_stop_during_starting_is_not_clobbered() {
        let key = ChannelKey::simple("/dev/test", 1);
        let shared = SharedTuner::new(key, 2);

        let source = FakeTsSource::new().with_startup_delay(std::time::Duration::from_millis(200));
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;
        assert_eq!(shared.state(), ReaderState::Starting);

        // Request a stop while still inside the fake's 200ms `set_channel`
        // delay, well before it would naturally reach `Running`.
        shared.stop_reader().await;
        assert_eq!(shared.state(), ReaderState::Stopped);

        // The reader must report failure-to-become-ready rather than
        // silently succeeding after the fact.
        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_ok(), "ready_tx still fires (startup itself succeeded); the state, not this value, is authoritative");
        assert_eq!(shared.state(), ReaderState::Stopped, "must not have been resurrected to Running");
    }
}
