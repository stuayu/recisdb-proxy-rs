//! Shared tuner implementation with broadcast capability.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::tuner::b25_pipe::B25Pipe; // 作った場所に合わせて
use b25_sys::DecoderOptions; // 鍵が必要な場合

use bytes::Bytes;
use log::{debug, error, info, warn};
use tokio::sync::{broadcast, watch};

use crate::bondriver::BonDriverTuner;
use crate::tuner::channel_key::ChannelKey;
use crate::tuner::lock::TunerLock;
use crate::tuner::logo_collector::ChannelLogoCollector;
use crate::tuner::epg_collector::EpgCollector;
use crate::tuner::nit_collector::NitCollector;
use crate::tuner::pool::{SlotPermit, TunerPool, TunerPoolConfig};
use crate::tuner::timing;
use crate::tuner::ts_source::TsSource;
use crate::tuner::warm::WarmTunerHandle;

/// 空振り(`wait_ts_stream` が false / `get_ts_stream` が WouldBlock)が
/// 何回続いたらログを出すか。100ms ポーリングなので 50 回 ≒ 5 秒間無データ。
const EMPTY_STREAK_LOG_THRESHOLD: u64 = 50;

/// 空振り連続回数 `streak` でログを出すべきか。
///
/// 正常に流れている間も「1チャンク届く → 1回空振り」は普通に起きるので、
/// 連続回数がしきい値に達するまでは何も出さない。達したあとは
/// `EMPTY_STREAK_LOG_THRESHOLD` 回ごと(= 5秒ごと)に1行だけ出す。
///
/// 旧実装は `streak % 50 == 1` で、`streak` はデータが1バイトでも届くと 1 に
/// 戻るため、通常のストリーミング中ずっと条件が成立して**全ての空振りが
/// ログに出ていた**(本番ログ2026-08-10: このログだけで68万行/日、
/// うち99.4%が "(1 times)")。間引きの条件としては逆になっていた。
fn should_log_empty_streak(streak: u64) -> bool {
    streak >= EMPTY_STREAK_LOG_THRESHOLD && streak % EMPTY_STREAK_LOG_THRESHOLD == 0
}

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

/// Why a reader stopped, recorded so a session that loses its tuner can say
/// *what happened* instead of just dropping the client
/// (docs/TUNER_PIPELINE_REDESIGN.md §2.1-7 / P4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Not set (or the reader is still running).
    Unspecified = 0,
    /// Displaced to free a driver slot for a higher-priority (or exclusive)
    /// request — see `policy::may_evict`.
    Evicted = 1,
    /// The reader itself failed: BonDriver open/SetChannel error, a caught
    /// panic, or too many consecutive read errors.
    ReaderFailed = 2,
    /// Stopped on purpose because nothing was subscribed any more
    /// (keep-alive expiry, or an explicit close).
    Released = 3,
}

impl TryFrom<u8> for StopReason {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, ()> {
        match value {
            0 => Ok(StopReason::Unspecified),
            1 => Ok(StopReason::Evicted),
            2 => Ok(StopReason::ReaderFailed),
            3 => Ok(StopReason::Released),
            _ => Err(()),
        }
    }
}

impl StopReason {
    /// Short, stable token for logs and the session-history
    /// `disconnect_reason` column.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Unspecified => "reader_stopped",
            StopReason::Evicted => "evicted",
            StopReason::ReaderFailed => "reader_failed",
            StopReason::Released => "reader_released",
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

/// B25 デコーダの初期化を待つ上限。
///
/// libaribb25 の初期化は、見つかったカードリーダーへ順に接続を試みる。
/// 応答しないカード / 相性の悪いリーダーが挿さっていると、この中の
/// `SCardTransmit` が1台あたり5秒ほどかけて失敗する (macOS 実機で計測)。
/// リーダー起動全体のタイムアウトは15秒しかないため、そのまま待つと
/// **スクランブル解除ができないどころか、生TSの配信すら開始できずに
/// 503 になる**。カードが無い/使えないことは「復号できない」で済むべきで、
/// 「視聴できない」に格上げしてはいけない。
const B25_INIT_BUDGET: Duration = Duration::from_secs(3);

/// B25 デコーダが使えるかどうかの判定結果。
///
/// `None` = まだ調べていない。判定は [`probe_b25_availability`] が別スレッドで
/// 行い、ここに書き戻す。
static B25_AVAILABLE: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// B25 デコーダが使えるかを別スレッドで調べ、結果を [`B25_AVAILABLE`] に残す。
///
/// `B25Pipe` は `Send` ではない (中に生ポインタを持つ) ので、スレッド境界を
/// 越えられるのは **可否の bool だけ**。デコーダはこのスレッドの中で作って
/// そのまま捨てる。
///
/// 起動時に一度呼んでおけば、最初の視聴要求が来るころには答えが出ている。
/// 判定にかかる時間はカードリーダー次第で、応答しないカードだと十数秒かかる。
pub fn probe_b25_availability() {
    // 既に判定済み、または判定中なら何もしない。
    {
        let guard = B25_AVAILABLE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
    }

    std::thread::spawn(|| {
        let opt = DecoderOptions {
            strip: true,
            emm: true,
            simd: true,
            round: 4,
            enable_working_key: false,
        };
        let started = std::time::Instant::now();
        let available = match B25Pipe::new(opt) {
            Ok(_decoder) => true, // ここで drop され、カードとの接続も閉じる
            Err(e) => {
                error!("[B25] デコーダを初期化できませんでした: {}", e);
                false
            }
        };
        let elapsed = started.elapsed();

        if available && elapsed > B25_INIT_BUDGET {
            warn!(
                "[B25] デコーダの初期化に {:?} かかりました。カードリーダーの応答が遅く、\
                 選局のたびに同じだけ待たされます。ダッシュボードの設定タブで\
                 B-CASカードを入れたリーダーを選んでください",
                elapsed
            );
        } else if !available {
            warn!("[B25] スクランブル解除なしで配信します (生TS)");
        }

        *B25_AVAILABLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(available);
    });
}

/// B25 デコーダを作る。ただし**まだ使えると分かっていない間は作らない**。
///
/// libaribb25 の初期化は、見つかったカードリーダーへ順に接続を試みる。応答しない
/// カードや相性の悪いリーダーが挿さっていると1台あたり5秒ほどかかり、リーダー
/// 起動全体のタイムアウト(15秒)を食い潰して**生TSの配信すら始められずに503**に
/// なる。カードが使えないことは「復号できない」で済むべきで、「視聴できない」に
/// 格上げしてはいけない。
///
/// そのため、判定が済んでいなければこの回は生TSで配信し、判定を裏で走らせる。
/// 次のリーダー起動からは答えが出ている。
/// 判定が済んでいて、かつ「使える」だったか。読み取りループが作り直しの
/// 要否を判断するのに使う。
fn b25_known_available() -> bool {
    matches!(*B25_AVAILABLE.lock().unwrap_or_else(|e| e.into_inner()), Some(true))
}

fn init_b25_with_deadline(opt: DecoderOptions) -> Option<B25Pipe> {
    let known = *B25_AVAILABLE.lock().unwrap_or_else(|e| e.into_inner());
    match known {
        Some(true) => match B25Pipe::new(opt) {
            Ok(decoder) => {
                info!("[SharedTuner] B25 decoder enabled");
                Some(decoder)
            }
            Err(e) => {
                // 判定後にカードが抜かれた等。次回の判定をやり直させる。
                error!("[SharedTuner] Failed to init B25 decoder: {}", e);
                *B25_AVAILABLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
                None
            }
        },
        Some(false) => None,
        None => {
            warn!(
                "[SharedTuner] B25デコーダの可否を判定中です。この配信は生TS \
                 (スクランブル解除なし) になります"
            );
            probe_b25_availability();
            None
        }
    }
}

/// Runtime startup tuning parameters for delayed network-backed drivers.
#[derive(Debug, Clone)]
pub struct ReaderStartupConfig {
    pub set_channel_retry_interval_ms: u64,
    pub set_channel_retry_timeout_ms: u64,
    pub signal_poll_interval_ms: u64,
    pub signal_wait_timeout_ms: u64,
    /// Whether to run the stream through libaribb25.
    ///
    /// `false` for sources that arrive already descrambled. 4K is the case
    /// that forced this: a MMT/TLV→TS converter descrambles ACAS itself but
    /// leaves the CA descriptor in the PMT advertising `CA_system_id` 0x0005 —
    /// the very id our B-CAS shim reports — so libaribb25 latches the declared
    /// ECM PID and waits for keys that never come. With `strip: true` that is
    /// one card outage away from deleting the video instead of merely wasting
    /// work.
    pub b25_enabled: bool,
    /// External MMT/TLV→TS converter to run in front of everything else.
    ///
    /// `Some` only for drivers registered as `stream_format = 'mmttlv'` (4K
    /// tuners). The conversion has to happen here, before the broadcast:
    /// every subscriber, the TS analyzer and the SI collectors all speak TS
    /// and nothing else.
    pub mmt_converter: Option<crate::tuner::mmt_pipe::MmtConverterConfig>,
}

impl From<&TunerPoolConfig> for ReaderStartupConfig {
    fn from(cfg: &TunerPoolConfig) -> Self {
        Self {
            set_channel_retry_interval_ms: cfg.set_channel_retry_interval_ms,
            set_channel_retry_timeout_ms: cfg.set_channel_retry_timeout_ms,
            signal_poll_interval_ms: cfg.signal_poll_interval_ms,
            signal_wait_timeout_ms: cfg.signal_wait_timeout_ms,
            // Callers that know the source is pre-descrambled turn this off;
            // the pool config alone cannot tell.
            b25_enabled: true,
            // Set by callers that know the driver delivers MMT/TLV.
            mmt_converter: None,
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
    /// Broadcasts every [`ReaderState`] transition so subscribers learn that
    /// their reader died *when it dies*, rather than on the next poll tick
    /// (docs/TUNER_PIPELINE_REDESIGN.md §2.1-7).
    state_tx: watch::Sender<ReaderState>,
    /// Why the reader stopped. See [`StopReason`].
    stop_reason: AtomicU8,
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
            state_tx: watch::channel(ReaderState::Idle).0,
            stop_reason: AtomicU8::new(StopReason::Unspecified as u8),
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
            tokio::time::sleep(Duration::from_millis(timing::WAIT_FIRST_DATA_POLL_MS)).await;
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

    /// Transition the reader lifecycle state and publish it to watchers.
    pub(crate) fn set_state(&self, state: ReaderState) {
        self.reader_state.store(state as u8, Ordering::Release);
        // `send_replace`, not `send`: with no receivers attached, `send`
        // returns an error *and leaves the stored value untouched*, so a
        // watcher that subscribes later would see whatever the state was
        // when the last receiver went away rather than the current one.
        // Readers spend most of their life with nobody watching (a session
        // only subscribes once it has selected this tuner), so that is the
        // normal case, not an edge case.
        self.state_tx.send_replace(state);
    }

    /// Watch this reader's lifecycle state.
    ///
    /// A session holds one of these for its current tuner so an eviction or
    /// a driver failure wakes it immediately; before P4 the only signal was
    /// a 2-second poll, so a displaced viewer sat on a dead stream for up to
    /// two seconds before being disconnected without explanation.
    pub fn subscribe_state(&self) -> watch::Receiver<ReaderState> {
        self.state_tx.subscribe()
    }

    /// Record why this reader is stopping. Set by whoever initiates the stop
    /// (the evictor, the reader's own failure paths, or the idle-close
    /// timer) *before* the state reaches `Stopped`, so a watcher that wakes
    /// on the transition already sees the reason.
    pub fn set_stop_reason(&self, reason: StopReason) {
        self.stop_reason.store(reason as u8, Ordering::Release);
    }

    /// Why this reader stopped, if it has.
    pub fn stop_reason(&self) -> StopReason {
        StopReason::try_from(self.stop_reason.load(Ordering::Acquire))
            .unwrap_or(StopReason::Unspecified)
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
    ///
    /// Returns `true` if the reader task was confirmed to have actually
    /// exited (or there was nothing to wait for), `false` if either the
    /// `reader_handle` lock or the task join timed out
    /// (`timing::STOP_READER_TIMEOUT_MS`). Either way, `self` is left in
    /// `ReaderState::Stopped` with its slot permit released — the return
    /// value exists purely so callers that are about to *reuse this exact
    /// DLL slot* for a new reader (see
    /// [`Self::stop_existing_reader_before_restart`]) can tell a confirmed
    /// stop apart from "gave up waiting", since only the former makes it
    /// actually safe to open a second instance on the same DLL
    /// (docs/TUNER_PIPELINE_REDESIGN.md §2.1-3). Most callers (idle-close,
    /// session teardown, eviction) don't care and simply discard it.
    pub async fn stop_reader(&self) -> bool {
        info!("[SharedTuner] Stopping reader for {:?}...", self.key);

        // Signal the reader task to stop. `Stopping` (not `Stopped` directly)
        // so `occupies_slot()` still reports true for the brief window before
        // the background task actually exits — this entry is not eligible
        // for reclaim/reuse until the DLL is actually released.
        self.set_state(ReaderState::Stopping);

        // Wait for the reader task to finish (with timeout).
        // wait_ts_stream() is now timing::WAIT_TS_STREAM_POLL_MS (100 ms), so
        // a healthy blocking task exits within ~200 ms of the state becoming
        // Stopping. timing::STOP_READER_TIMEOUT_MS (1 s) is a generous upper
        // bound for a well-behaved DLL.
        let joined = if let Ok(mut guard) = tokio::time::timeout(
            std::time::Duration::from_millis(timing::STOP_READER_TIMEOUT_MS),
            self.reader_handle.lock()
        ).await {
            if let Some(handle) = guard.take() {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timing::STOP_READER_TIMEOUT_MS),
                    handle
                ).await {
                    Ok(_) => {
                        info!("[SharedTuner] Reader task completed gracefully for {:?}", self.key);
                        true
                    }
                    Err(_) => {
                        error!("[SharedTuner] Reader task timeout for {:?}, aborting", self.key);
                        false
                    }
                }
            } else {
                // Nothing to join (never started, or another concurrent
                // `stop_reader()` already took the handle) — there is no
                // outstanding task, so this counts as cleanly stopped.
                true
            }
        } else {
            error!("[SharedTuner] Failed to acquire reader handle lock for {:?}", self.key);
            false
        };

        // Final ensure: mark as stopped, even if the reader task never got a
        // chance to set this itself (timeout/abort above). Also releases the
        // driver-slot permit explicitly here (docs/TUNER_PIPELINE_REDESIGN.md
        // P1b item 2) rather than waiting on this `SharedTuner`'s `Arc` to
        // drop — a caller that immediately reopens the same DLL (permit
        // handoff during a channel switch) needs the slot freed
        // deterministically at this point, not whenever the last reference
        // happens to go away.
        //
        // This unconditional release even on `joined == false` is a
        // deliberate trade-off carried over from P1b: a DLL that truly never
        // returns from its blocking call would otherwise leak the slot
        // forever. The residual risk (a still-running old thread and a new
        // reader both touching the same DLL) is what
        // `stop_existing_reader_before_restart` guards against by refusing
        // to *proceed to a new open* when `joined` is `false`, rather than
        // by holding the permit here.
        self.stop_and_release_slot();

        info!("[SharedTuner] Reader stopped for {:?}", self.key);
        joined
    }

    /// If this tuner already has a reader in flight or live (an in-place
    /// restart on the same DLL instance, as opposed to a fresh `Reserved`/
    /// `Stopped` entry), stop it and confirm that stop actually completed
    /// before returning `Ok`. Returns `Err` instead of proceeding if the old
    /// reader refuses to stop within `stop_reader`'s own timeout
    /// (docs/TUNER_PIPELINE_REDESIGN.md §2.1-3) — the previous behavior of
    /// waiting a fixed 500 ms and continuing regardless could leave the old
    /// and new readers both open on the same DLL at once, which is exactly
    /// the kind of concurrent-access corruption `dll_init_lock` exists to
    /// prevent for the *open* phase but cannot prevent once an old reader is
    /// already past it.
    async fn stop_existing_reader_before_restart(&self) -> Result<(), std::io::Error> {
        if !matches!(self.state(), ReaderState::Starting | ReaderState::Running | ReaderState::Stopping) {
            return Ok(());
        }
        info!("[SharedTuner] Stopping existing reader for {:?} before restart", self.key);
        if self.stop_reader().await {
            Ok(())
        } else {
            error!(
                "[SharedTuner] Existing reader for {:?} failed to stop before restart; refusing to start a new one",
                self.key
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "existing reader did not stop before restart",
            ))
        }
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
                    shared.set_stop_reason(StopReason::ReaderFailed);
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
                    shared.set_stop_reason(StopReason::ReaderFailed);
                    shared.stop_and_release_slot();
                    let _ = ready_tx.send(Err("SetChannel caused panic - BonDriver may be corrupted".to_string()));
                    return;
                }
            }
        }

        // Purge any stale data from the buffer
        tuner.purge_ts_stream();

        // Short stabilization wait for new driver to have something in buffer
        std::thread::sleep(std::time::Duration::from_millis(timing::SET_CHANNEL_STABILIZATION_SLEEP_MS));

        // ===== B25 decoder init =====
        let b25_opt = DecoderOptions {
            strip: true,
            emm: true,
            simd: true,
            round: 4,
            enable_working_key: false,
        };

        // ===== MMT/TLV → TS converter (4K) =====
        // Runs before everything else: the rest of this loop, the analyzer and
        // every subscriber assume MPEG-2 TS.
        let mut mmt = match startup_config.mmt_converter.as_ref() {
            Some(cfg) => match crate::tuner::mmt_pipe::MmtPipe::new(cfg) {
                Ok(pipe) => {
                    info!("[SharedTuner] MMT/TLV converter started for {:?}", shared.key);
                    Some(pipe)
                }
                Err(e) => {
                    // Without the converter this driver emits MMT/TLV that
                    // nothing downstream can read, so publishing the raw bytes
                    // would just look like a dead channel. Fail the start.
                    error!(
                        "[SharedTuner] Failed to start MMT/TLV converter for {:?}: {}",
                        shared.key, e
                    );
                    let _ = ready_tx.send(Err(format!(
                        "MMT/TLV converter failed to start: {}",
                        e
                    )));
                    shared.stop_and_release_slot();
                    return;
                }
            },
            None => None,
        };

        let mut b25 = if startup_config.b25_enabled {
            init_b25_with_deadline(b25_opt)
        } else {
            info!(
                "[SharedTuner] B25 disabled for {:?} (source is already descrambled)",
                shared.key
            );
            None
        };
        // 判定中に起動した場合の作り直しは1回だけ。毎チャンク試すと、
        // カードが遅い環境で読み取りループが止まり続ける。
        let mut b25_init_retried = false;

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

        // docs/TUNER_PIPELINE_REDESIGN.md §2.1-1: the caller waiting on
        // `ready_rx` may already have timed out and walked away (its pool
        // entry removed, its own timeout computed as
        // `set_channel_retry_timeout_ms + READY_TIMEOUT_MARGIN_MS` via
        // `timing::reader_ready_timeout`) by the time SetChannel finally
        // succeeds — dropping `ready_rx` and making this `send` fail is
        // exactly how that shows up here. If nobody is listening anymore,
        // this reader must not enter the read loop and occupy the DLL slot
        // on nobody's behalf; release it and exit instead. This is the other
        // half of the orphaned-reader fix (the timeout-side half lives in
        // `timing::reader_ready_timeout`) — treating a failed send here
        // exactly like the various `Err` branches above that already bail
        // out before this point.
        if ready_tx.send(Ok(())).is_err() {
            info!(
                "[SharedTuner] Ready receiver dropped for {:?} (caller gave up waiting); not entering read loop",
                shared.key
            );
            shared.stop_and_release_slot();
            return;
        }

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
            // timing::WAIT_TS_STREAM_POLL_MS (100 ms) instead of 1000 ms so
            // the stop-check at the top of the loop is reached quickly after
            // stop_reader() sets the state to Stopping.  This makes channel
            // switches faster and keeps stop_reader()'s own join timeout
            // (timing::STOP_READER_TIMEOUT_MS) comfortably longer than one
            // iteration.
            let wait_result = tuner.wait_ts_stream(timing::WAIT_TS_STREAM_POLL_MS as u32);
            if !wait_result {
                consecutive_empty = consecutive_empty.saturating_add(1);
                if should_log_empty_streak(consecutive_empty) {
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

                    // Broadcast to all subscribers.
                    //
                    // Logo (SDT/CDT) and EPG (EIT) collection used to run
                    // here, on the read-rate-limiting thread, once per chunk
                    // (P3 §2.2-10). They now run in their own task fed by the
                    // same broadcast — see `spawn_si_collector`.
                    // 4K: what the driver handed over is MMT/TLV, not TS.
                    // Convert first; everything below (B25, the analyzer, the
                    // broadcast) only ever sees the converted TS.
                    let converted;
                    let raw: &[u8] = match mmt.as_mut() {
                        Some(pipe) => match pipe.push(&buf[..n]) {
                            Ok(ts) => {
                                if ts.is_empty() {
                                    // Normal at start-up and whenever the
                                    // converter is still filling: nothing to
                                    // publish yet, but the tuner read must keep
                                    // running.
                                    continue;
                                }
                                converted = ts;
                                &converted
                            }
                            Err(e) => {
                                error!(
                                    "[SharedTuner] MMT/TLV conversion failed for {:?}: {}",
                                    shared.key, e
                                );
                                break;
                            }
                        },
                        None => &buf[..n],
                    };

                    // 起動時はB25の可否がまだ分かっていないことがある
                    // (`init_b25_with_deadline` 参照)。判定が「使える」で
                    // 確定したら、ここで一度だけデコーダを作る。これをやらないと、
                    // 判定中に始まったリーダーは以後ずっとスクランブルされたままの
                    // TSを流し続ける (視聴者には「映像が出ない」としか見えない)。
                    if startup_config.b25_enabled
                        && b25.is_none()
                        && !b25_init_retried
                        && b25_known_available()
                    {
                        b25_init_retried = true;
                        b25 = init_b25_with_deadline(DecoderOptions {
                            strip: true,
                            emm: true,
                            simd: true,
                            round: 4,
                            enable_working_key: false,
                        });
                    }

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
                        if should_log_empty_streak(consecutive_empty) && !reader_first_read {
                            info!("[SharedTuner] get_ts_stream WouldBlock ({} times), total_bytes={}", consecutive_empty, total_bytes_read);
                        }
                        let max_attempts = if reader_first_read { 40000 } else { 1000 };
                        if consecutive_empty > max_attempts {
                            error!("[SharedTuner] Too many WouldBlock errors ({} times), stopping reader for {:?}", consecutive_empty, shared.key);
                            shared.set_stop_reason(StopReason::ReaderFailed);
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
                        shared.set_stop_reason(StopReason::ReaderFailed);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(panic_err) => {
                    error!("[SharedTuner] PANIC during get_ts_stream: {:?}", panic_err);
                    shared.set_stop_reason(StopReason::ReaderFailed);
                    shared.stop_and_release_slot();
                    break;
                }
            }
        }

        shared.stop_and_release_slot();
        info!("[SharedTuner] Reader task stopped for {:?}, total bytes: {}", shared.key, total_bytes_read);
    }

    /// Single entry point for starting this tuner's reader
    /// (docs/TUNER_PIPELINE_REDESIGN.md P2a item 2/3) — cold-opens a fresh
    /// BonDriver, or activates an already-open `warm` handle if one is
    /// supplied and holds the right path, whichever applies. This replaces
    /// the previous two independently-called entry points
    /// (`start_bondriver_reader` for cold, `WarmTunerHandle::activate` for
    /// warm) that callers (`server/session.rs`, `server/channel_resolve.rs`)
    /// used to choose between themselves, each with its own copy of the
    /// permit bookkeeping, DLL-lock acquisition, and ready-timeout value.
    ///
    /// `tuner_pool` is only used to acquire the per-DLL init lock (see
    /// below) — this function does not touch pool membership itself.
    ///
    /// `permit` is this entry's [`SlotPermit`] (docs/TUNER_PIPELINE_REDESIGN.md
    /// P1b) — a reader cannot be started without one, enforced here at the
    /// type level. In the common case it is the very permit
    /// `TunerPool::get_or_create` stored on this same `SharedTuner` when it
    /// was created, handed back in by the caller via `take_slot_permit()`
    /// (see that method's doc comment); when this call is instead an
    /// in-place channel restart on an already-`Running` tuner, `permit` is
    /// that same still-live reservation being passed straight back through —
    /// this entry never stopped occupying its slot, so there is nothing new
    /// to reserve. Either way, this function stores `permit` onto `self` via
    /// `set_slot_permit` before attempting anything fallible, so every
    /// failure path releases it the same way (`stop_and_release_slot`).
    ///
    /// `warm`, if `Some`, is only used when it holds a BonDriver already open
    /// on `tuner_path` — a mismatched warm handle is shut down and this
    /// falls back to a cold open. Callers are expected to have already
    /// decided *which* permit to use (their own vs. the warm handle's own
    /// reservation — see `server/session.rs::acquire_slot_preferring_warm`)
    /// before calling this; that selection is deliberately left outside this
    /// function since it depends on session-local bookkeeping this generic
    /// entry point has no business knowing about.
    pub async fn start_reader(
        self: &Arc<Self>,
        tuner_pool: &TunerPool,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        permit: SlotPermit,
        warm: Option<WarmTunerHandle>,
    ) -> Result<(), std::io::Error> {
        // Per-DLL init lock now lives here rather than at each call site
        // (docs/TUNER_PIPELINE_REDESIGN.md P2a item 3) — previously both
        // `session.rs::start_reader_with_warm` and
        // `channel_resolve::start_tuner_for_service` had to remember to take
        // it themselves before calling into the (formerly two) start
        // functions; folding it in here makes "forgot to lock" impossible
        // for any future caller. Held across the whole cold-open-or-warm-
        // activate attempt below, released once this function returns.
        //
        // `stop_reader()` deliberately does *not* take this lock: P1b's slot
        // permits already guarantee an old reader keeps holding this exact
        // slot until it reaches `Stopped` (see `stop_and_release_slot`), so a
        // new open by definition cannot start until the old permit is freed
        // — there is nothing left for a lock on the stop side to protect
        // against, only latency to add.
        let _dll_guard = tuner_pool.acquire_dll_init_lock(&tuner_path).await;

        // In-place restart on this same `SharedTuner` (same DLL instance,
        // new channel): the old reader must be confirmed stopped before a
        // new one opens (docs/TUNER_PIPELINE_REDESIGN.md §2.1-3).
        self.stop_existing_reader_before_restart().await?;

        self.set_slot_permit(permit);

        // Mark this entry as occupied *synchronously*, before `spawn_blocking`
        // even schedules the background thread — closes the window
        // `is_reclaimable()`/M8 fixes (docs/TUNER_PIPELINE_REDESIGN.md §4 P1):
        // a concurrent `TunerPool::get_or_create`/`cleanup`/`evict_idle_on_path`
        // call on another task must never see this entry as `Idle` between
        // "caller decided to start a reader" and "the blocking thread got
        // scheduled and reached its own `set_state(Starting)`".
        self.set_state(ReaderState::Starting);

        let ready_timeout = timing::reader_ready_timeout(startup_config.set_channel_retry_timeout_ms);

        let started = self.dispatch_reader_start(warm, tuner_path, space, channel, startup_config, ready_timeout).await;
        if started.is_ok() {
            // Fed by the same broadcast the clients read; see its doc comment.
            self.spawn_si_collector();
        }
        started
    }

    /// Cold-open or warm-activate, whichever `warm` allows. Split out of
    /// [`Self::start_reader`] purely so that function has one success point
    /// to hang post-start work off.
    async fn dispatch_reader_start(
        self: &Arc<Self>,
        warm: Option<WarmTunerHandle>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_timeout: Duration,
    ) -> Result<(), std::io::Error> {
        match warm {
            Some(warm) if warm.path() == tuner_path => {
                self.start_reader_warm(warm, tuner_path, space, channel, startup_config, ready_timeout).await
            }
            Some(warm) => {
                // Warm handle for a different DLL path — not usable for this
                // request; shut it down (releasing its own permit) and cold
                // start instead.
                warm.shutdown().await;
                self.start_reader_cold(tuner_path, space, channel, startup_config, ready_timeout).await
            }
            None => self.start_reader_cold(tuner_path, space, channel, startup_config, ready_timeout).await,
        }
    }

    /// Activate `warm` (an already-open BonDriver) against `self`.
    ///
    /// Called only from [`Self::start_reader`] once `self` already holds the
    /// permit and is in `ReaderState::Starting` — see that function's doc
    /// comment for the parts of the sequence this relies on having already
    /// happened (DLL-lock acquisition, old-reader stop, permit storage).
    async fn start_reader_warm(
        self: &Arc<Self>,
        warm: WarmTunerHandle,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_timeout: Duration,
    ) -> Result<(), std::io::Error> {
        let mut warm = warm;
        let tuner_path_for_fallback = tuner_path.clone();
        // Cloned for the cold-open fallback below: the config is no longer
        // `Copy` now that it carries the converter settings.
        let startup_config_for_fallback = startup_config.clone();
        match warm
            .activate(Arc::clone(self), tuner_path, space, channel, startup_config, ready_timeout)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!("[SharedTuner] Warm tuner activation failed for {:?}: {}", self.key, e);
                let warm_thread_gone = e.kind() == std::io::ErrorKind::NotConnected;
                warm.shutdown().await;

                // Cold fallback (restored behaviour): a warm handle whose
                // thread already exited — overwhelmingly the common case,
                // since `prewarm_timeout_secs` (default 30s) expires while a
                // client browses the channel list — holds no DLL handle and
                // left its permit on us. Opening cold on that same slot is
                // exactly what the pre-P2a code did, and failing the whole
                // selection instead would turn a routine prewarm expiry into
                // a user-visible tuning failure.
                //
                // Only for `NotConnected`: after a ready-wait timeout the
                // warm thread may still be mid-`SetChannel` with the DLL
                // open, so a cold open would be a double open (see
                // `WarmTunerHandle::activate`'s doc comment).
                if warm_thread_gone {
                    if let Some(permit) = self.take_slot_permit() {
                        info!(
                            "[SharedTuner] Warm thread was already gone for {:?}; falling back to a cold open on the same slot",
                            self.key
                        );
                        self.set_slot_permit(permit);
                        self.set_state(ReaderState::Starting);
                        return self
                            .start_reader_cold(tuner_path_for_fallback, space, channel, startup_config_for_fallback, ready_timeout)
                            .await;
                    }
                    // No permit to retry with (someone else took it): give
                    // the slot accounting a definite answer rather than
                    // leaving the entry `Starting` forever.
                    self.stop_and_release_slot();
                }
                Err(e)
            }
        }
    }

    /// Cold-open a fresh `BonDriverTuner` on `tuner_path` and run its reader.
    ///
    /// Called only from [`Self::start_reader`] — see that function's doc
    /// comment for the parts of the sequence this relies on having already
    /// happened (DLL-lock acquisition, old-reader stop, permit storage,
    /// `Starting` state).
    async fn start_reader_cold(
        self: &Arc<Self>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_timeout: Duration,
    ) -> Result<(), std::io::Error> {
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
                        shared.set_stop_reason(StopReason::ReaderFailed);
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
                    shared.set_stop_reason(StopReason::ReaderFailed);
                    shared.stop_and_release_slot();
                }
            }
        });

        // Store the handle and spawn a cleanup task
        *self.reader_handle.lock().await = Some(handle);

        // Wait for the reader to signal it's ready (BonDriver opened, channel set).
        // `ready_timeout` (docs/TUNER_PIPELINE_REDESIGN.md §2.1-1) is always
        // strictly longer than the reader's own `set_channel_retry_timeout_ms`
        // budget, so this side can never time out and walk away while the
        // reader is still inside a retry it could yet succeed at — see
        // `timing::reader_ready_timeout`. If this timeout *does* fire, the
        // reader is responsible for noticing (its own `ready_tx.send()`
        // failing) and releasing the slot itself rather than entering the
        // read loop.
        match tokio::time::timeout(ready_timeout, ready_rx).await {
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

    /// Run SDT/CDT (logo) and EIT (EPG) collection for this tuner in its own
    /// task, fed by the same broadcast the clients read
    /// (docs/TUNER_PIPELINE_REDESIGN.md P3 §2.2-10).
    ///
    /// This used to run inline in the reader loop, on the very thread whose
    /// throughput determines whether TS data is read fast enough — every
    /// chunk paid a full PSI scan before it could be broadcast. Moving it to
    /// a consumer keeps the read path to "read, decode, broadcast".
    ///
    /// Subscribes *untracked*: this is a parasitic consumer, and must not
    /// keep the tuner alive or perturb the session-driven keep-alive
    /// accounting. It holds a `Weak`, so the task falls out as soon as the
    /// tuner is dropped, and stops as soon as the reader leaves `Running`.
    ///
    /// Note that it now sees the *decoded* stream rather than the raw one.
    /// SI tables (SDT/CDT/EIT) are never scrambled, and B25 `strip` only
    /// drops null packets, so the tables this collects are unaffected — and
    /// on the fallback path where B25 is unavailable the bytes are the raw
    /// ones anyway.
    pub(crate) fn spawn_si_collector(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut rx = self.subscribe_untracked();
        let mut state_rx = self.subscribe_state();
        let key = self.key.clone();

        tokio::spawn(async move {
            let mut logo_collector = ChannelLogoCollector::new();
            let mut epg_collector = EpgCollector::new();
            let mut nit_collector = NitCollector::new();
            let mut scramble_watch = ScrambleWatch::new();

            loop {
                tokio::select! {
                    chunk = rx.recv() => match chunk {
                        Ok(data) => {
                            logo_collector.process_ts_chunk(&data);
                            epg_collector.process_ts_chunk(&data);
                            nit_collector.process_ts_chunk(&data);
                            scramble_watch.observe(&key, &data);
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            // SI tables repeat, so a gap costs at most a
                            // delayed table; never worth slowing the reader.
                            debug!("[SI collector] {:?}: lagged {} chunk(s)", key, skipped);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    changed = state_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        if *state_rx.borrow_and_update() != ReaderState::Running {
                            break;
                        }
                    }
                }

                if weak.upgrade().is_none() {
                    break;
                }
            }

            debug!("[SI collector] {:?}: stopped", key);
        });
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
        b25_enabled: true,
        mmt_converter: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通常のストリーミング(空振りが単発で終わる)では1行も出さない。
    /// 旧実装(`streak % 50 == 1`)はここで毎回 true を返していた。
    #[test]
    fn single_empty_polls_between_chunks_are_never_logged() {
        for _ in 0..10_000 {
            assert!(!should_log_empty_streak(1));
        }
        assert!(!should_log_empty_streak(2));
        assert!(!should_log_empty_streak(EMPTY_STREAK_LOG_THRESHOLD - 1));
    }

    /// 本当に無データが続いたときだけ、しきい値ごとに1行出す。
    #[test]
    fn sustained_starvation_logs_once_per_threshold() {
        assert!(should_log_empty_streak(EMPTY_STREAK_LOG_THRESHOLD));
        assert!(should_log_empty_streak(EMPTY_STREAK_LOG_THRESHOLD * 2));
        assert!(!should_log_empty_streak(EMPTY_STREAK_LOG_THRESHOLD + 1));

        // 5秒相当(50回)無データが1分続いても12行まで。
        let logged = (1..=600).filter(|n| should_log_empty_streak(*n)).count();
        assert_eq!(logged, 12);
    }

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

    // -----------------------------------------------------------------
    // P2a: orphaned-reader fix (§2.1-1) and restart-must-wait fix (§2.1-3).
    // -----------------------------------------------------------------

    /// Core §2.1-1 regression test: if the caller waiting on the ready
    /// signal gives up (its `ready_rx`/timeout future dropped) before the
    /// reader finishes `SetChannel`, the reader must notice its
    /// `ready_tx.send()` failing and bail out *before* entering the read
    /// loop — not silently start streaming into a broadcast channel nobody
    /// is listening to while permanently occupying its driver slot.
    #[tokio::test]
    async fn ready_send_failure_after_success_releases_slot_without_entering_read_loop() {
        let key = ChannelKey::simple("/dev/orphan-test", 1);
        let shared = SharedTuner::new(key, 2);

        // A real slot permit, exactly like `start_reader` would store via
        // `set_slot_permit` before spawning — needed so we can observe it
        // being released (a second `acquire_slot` on a 1-capacity path only
        // succeeds once the first permit is actually dropped).
        let pool = crate::tuner::pool::TunerPool::new(4);
        let permit = pool
            .acquire_slot("/dev/orphan-test", 1)
            .await
            .expect("first permit must be free");
        shared.set_slot_permit(permit);

        // Startup delay wide enough that we can reliably drop `ready_rx`
        // while the fake reader is still inside `set_channel`, well before
        // it would attempt to signal ready.
        let source = FakeTsSource::new()
            .with_startup_delay(std::time::Duration::from_millis(200))
            .with_chunk(vec![0u8; 188]);
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;
        assert_eq!(shared.state(), ReaderState::Starting);

        // Simulate the caller's own ready-wait timing out and giving up —
        // in real code this is `tokio::time::timeout(...)` elapsing and
        // dropping the wrapped `ready_rx` future.
        drop(ready_rx);

        // Poll (rather than a single fixed sleep) for the fake reader's
        // blocking thread to run past its startup delay, the fixed
        // post-SetChannel stabilization sleep
        // (`timing::SET_CHANNEL_STABILIZATION_SLEEP_MS`, 500 ms), and real
        // (if failing, in this test environment) B25 decoder library-load
        // probing, transition to Running, and attempt `ready_tx.send(Ok(()))`
        // — which must now fail since the receiver is gone. Polling up to a
        // generous ceiling avoids the test being sensitive to exactly how
        // long that probing takes under parallel test-suite load, while
        // still failing promptly if the reader gets stuck forever.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if shared.state() == ReaderState::Stopped {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reader did not self-terminate within 10s of the ready receiver being dropped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert_eq!(
            shared.state(),
            ReaderState::Stopped,
            "reader must self-terminate instead of entering the read loop when nobody is listening for ready"
        );

        // The permit must have been released — a second acquire on the same
        // (max_instances=1) path must now succeed.
        assert!(
            pool.acquire_slot("/dev/orphan-test", 1).await.is_some(),
            "slot permit must be released when the reader bails out before the read loop"
        );

        // Every test that spawns a fake reader must join its blocking task
        // before ending (see other tests' comments) — safe to call even
        // though the reader has already self-terminated.
        shared.stop_reader().await;
    }

    /// `timing::reader_ready_timeout` is exercised in isolation in
    /// `tuner::timing`'s own tests; this confirms the value actually plumbed
    /// through from a `ReaderStartupConfig` matches it, so the two don't
    /// silently drift apart.
    #[test]
    fn ready_timeout_derived_from_startup_config_matches_timing_module() {
        let config = ReaderStartupConfig {
            set_channel_retry_interval_ms: 500,
            set_channel_retry_timeout_ms: 10_000,
            signal_poll_interval_ms: 500,
            signal_wait_timeout_ms: 10_000,
            b25_enabled: true,
            mmt_converter: None,
        };
        assert_eq!(
            timing::reader_ready_timeout(config.set_channel_retry_timeout_ms),
            timing::reader_ready_timeout(10_000)
        );
        assert!(
            timing::reader_ready_timeout(config.set_channel_retry_timeout_ms)
                > Duration::from_millis(config.set_channel_retry_timeout_ms)
        );
    }

    /// §2.1-3: restarting a reader that is still alive must wait for it to
    /// actually stop, not give up after a fixed delay and proceed anyway.
    #[tokio::test]
    async fn stop_existing_reader_before_restart_waits_for_a_healthy_reader() {
        let key = ChannelKey::simple("/dev/restart-test", 1);
        let shared = SharedTuner::new(key, 2);

        let source = FakeTsSource::new().with_chunk(vec![0u8; 188]);
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;
        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_ok());
        assert_eq!(shared.state(), ReaderState::Running);

        assert!(
            shared.stop_existing_reader_before_restart().await.is_ok(),
            "a healthy reader must stop within stop_reader's own timeout"
        );
        assert_eq!(shared.state(), ReaderState::Stopped);
    }

    /// §2.1-3: if the old reader is stuck inside a blocking DLL call and
    /// does not exit within `stop_reader`'s own timeout, the restart must be
    /// refused with an error rather than silently proceeding to open a
    /// second instance on top of the still-running one.
    #[tokio::test]
    async fn stop_existing_reader_before_restart_errors_if_reader_does_not_stop_in_time() {
        let key = ChannelKey::simple("/dev/stuck-test", 1);
        let shared = SharedTuner::new(key, 2);

        // `get_ts_stream` blocks on this gate until the test releases it —
        // simulating a hung DLL call that never returns. `wait_until_entered`
        // below (rather than a fixed sleep before calling `stop_reader()`)
        // is what makes this deterministic: without it, `stop_reader()`
        // could set `Stopping` before the reader loop even reaches its first
        // `get_ts_stream` call (it does some setup work — buffer allocation,
        // collector construction, a signal-level log — between becoming
        // `Running` and its first loop iteration), in which case the loop's
        // very first stop-flag check exits immediately without ever
        // blocking on the gate at all, which is correct fast-stop behavior
        // but not what this test means to exercise. See [`BlockingGate`]'s
        // doc comment — this was caught as a genuinely (not just rarely)
        // flaky test during review.
        let gate = crate::tuner::ts_source::BlockingGate::new();
        let source = FakeTsSource::new()
            .with_chunk(vec![0u8; 188])
            .with_get_ts_stream_gate(gate.clone());
        let ready_rx = shared.spawn_fake_reader(source, 0, 1, test_startup_config()).await;
        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal timed out")
            .expect("ready channel closed unexpectedly");
        assert!(ready.is_ok());
        assert_eq!(shared.state(), ReaderState::Running);

        tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_until_entered())
            .await
            .expect("reader never reached the blocking get_ts_stream call");

        let result = shared.stop_existing_reader_before_restart().await;
        assert!(
            result.is_err(),
            "a reader stuck past stop_reader's timeout must surface as an error, not be silently ignored"
        );
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );

        // `stop_and_release_slot` inside `stop_reader` unconditionally marks
        // this `Stopped` even on a timeout (see that method's doc comment on
        // why) — the error return above, not this state, is what a caller
        // must act on to avoid proceeding with a new open.
        assert_eq!(shared.state(), ReaderState::Stopped);

        // Release the stuck blocking thread so it can actually finish (it
        // will then observe `state() != Running` and exit) before the test
        // ends — otherwise the runtime's own shutdown would block on it
        // indefinitely instead of just until the gate opens.
        gate.release();
    }
}

/// 配信しているTSが実際にスクランブル解除できているかを見張る。
///
/// 「B25 decoder enabled」のログは**初期化が通った**ことしか意味しない。
/// libaribb25 はデータを受け取り続けるが、ECM (数秒ごとに更新される鍵情報) の
/// 処理がカードとの APDU に依存しており、カードリーダーが遅い/相性が悪いと
/// そこだけ失敗する。結果、デコーダは「有効」でバイト数も流れているのに、
/// 出ていくTSはスクランブルされたまま — 利用者からは「映像が出ない」としか
/// 見えず、原因の切り分けに TS を取得してビットを数える必要があった。
///
/// 判定は配信されるデータそのものから行う。TSヘッダ4バイト目の上位2ビット
/// (transport_scrambling_control) が 0 以外なら、そのパケットは暗号化されている。
///
/// 読み取りループではなく broadcast の購読側で動かす (CLAUDE.md の不変条件:
/// 読み取りループに毎チャンクの処理を足さない)。
struct ScrambleWatch {
    started: std::time::Instant,
    scrambled: u64,
    clear: u64,
    reported: bool,
}

/// 判定を出すまでの猶予。選局直後は鍵の取得が済んでいないので、しばらくは
/// スクランブルされたパケットが流れるのが正常。
const SCRAMBLE_WATCH_GRACE: Duration = Duration::from_secs(20);

/// この割合以上が暗号化されたままなら「復号できていない」とみなす。
/// 有料放送の一部だけが暗号化されている構成もあるため、多数決ではなく
/// 明らかに大半が暗号化されている場合だけを対象にする。
const SCRAMBLE_WATCH_THRESHOLD: f64 = 0.8;

impl ScrambleWatch {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            scrambled: 0,
            clear: 0,
            reported: false,
        }
    }

    fn observe(&mut self, key: &ChannelKey, chunk: &[u8]) {
        if self.reported {
            return;
        }

        // チャンク全体を舐める必要はない。先頭の数パケットで十分に代表できる。
        for packet in chunk.chunks_exact(188).take(32) {
            if packet[0] != 0x47 {
                continue; // 同期がずれているチャンクは判定材料にしない
            }
            if (packet[3] >> 6) & 0x3 == 0 {
                self.clear += 1;
            } else {
                self.scrambled += 1;
            }
        }

        let total = self.scrambled + self.clear;
        if self.started.elapsed() < SCRAMBLE_WATCH_GRACE || total < 1000 {
            return;
        }

        self.reported = true;
        let ratio = self.scrambled as f64 / total as f64;
        if ratio >= SCRAMBLE_WATCH_THRESHOLD {
            warn!(
                "[B25] {:?}: 配信中のTSの {:.0}% がスクランブルされたままです。\
                 B-CASカードの鍵処理が効いていません (カードリーダーの相性や応答速度が原因のことが多い)。\
                 このままではブラウザでの再生や録画後の視聴で映像が出ません",
                key,
                ratio * 100.0
            );
        } else {
            debug!(
                "[B25] {:?}: スクランブル解除は機能しています (暗号化されたまま {:.1}%)",
                key,
                ratio * 100.0
            );
        }
    }
}

#[cfg(test)]
mod scramble_watch_tests {
    use super::*;

    fn packet(scrambling_control: u8) -> Vec<u8> {
        let mut p = vec![0u8; 188];
        p[0] = 0x47;
        p[3] = scrambling_control << 6;
        p
    }

    fn chunk(scrambled: usize, clear: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..scrambled {
            out.extend_from_slice(&packet(3));
        }
        for _ in 0..clear {
            out.extend_from_slice(&packet(0));
        }
        out
    }

    #[test]
    fn stays_quiet_until_enough_data_and_time_have_passed() {
        // 選局直後は鍵の取得前でスクランブルされたパケットが流れるのが正常。
        // ここで警告を出すと毎回の選局で誤検知になる。
        let mut watch = ScrambleWatch::new();
        let key = ChannelKey::simple("/dev/test", 1);
        for _ in 0..100 {
            watch.observe(&key, &chunk(32, 0));
        }
        assert!(!watch.reported, "猶予時間内に判定してはいけない");
    }

    #[test]
    fn reports_once_and_then_stops_counting() {
        let mut watch = ScrambleWatch::new();
        watch.started = std::time::Instant::now() - SCRAMBLE_WATCH_GRACE - Duration::from_secs(1);
        let key = ChannelKey::simple("/dev/test", 1);
        for _ in 0..40 {
            watch.observe(&key, &chunk(32, 0));
        }
        assert!(watch.reported);

        // 判定後は数えない (ログを繰り返さないし、無駄な走査もしない)。
        let counted = watch.scrambled;
        watch.observe(&key, &chunk(32, 0));
        assert_eq!(watch.scrambled, counted);
    }

    #[test]
    fn a_clear_broadcast_is_not_flagged() {
        let mut watch = ScrambleWatch::new();
        watch.started = std::time::Instant::now() - SCRAMBLE_WATCH_GRACE - Duration::from_secs(1);
        let key = ChannelKey::simple("/dev/test", 1);
        for _ in 0..40 {
            watch.observe(&key, &chunk(0, 32));
        }
        assert!(watch.reported);
        assert_eq!(watch.scrambled, 0);
    }

    #[test]
    fn ignores_packets_that_lost_sync() {
        // 同期バイトが無いチャンクを数えると、比率がでたらめになる。
        let mut watch = ScrambleWatch::new();
        let key = ChannelKey::simple("/dev/test", 1);
        watch.observe(&key, &vec![0u8; 188 * 4]);
        assert_eq!(watch.scrambled + watch.clear, 0);
    }
}
