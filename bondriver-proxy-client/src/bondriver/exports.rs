//! BonDriver exported functions.
#![allow(dead_code, static_mut_refs)]
//!
//! This module implements the BonDriver interface functions that are called
//! by the host application (e.g., TVTest).

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Once;
use std::time::Duration;

use log::{debug, error, info, trace};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use crate::bondriver::interface::*;
use crate::client::buffer::TS_PACKET_SIZE;
use crate::client::{Connection, ConnectionConfig, ConnectionState};
use crate::file_log;

/// Per-instance state for one BonDriver object.
///
/// **One state per `CreateBonDriver()` object — never a process-wide
/// singleton.**  A host may create several BonDriver objects from the same DLL
/// (recisdb-proxy does exactly this when a driver's `max_instances > 1`, and
/// EDCB does it for multiple tuners).  Sharing one `Connection`/`TsRingBuffer`
/// between them makes each object consume TS bytes belonging to the other:
/// the streams interleave, continuity counters break and PMT/ECM packets go
/// missing — which the viewer reports as Drop / Error / Scramble.
struct BonDriverState {
    /// Connection to the proxy server.
    connection: Arc<Connection>,
    /// Current tuning space.
    cur_space: u32,
    /// Current channel.
    cur_channel: u32,
    /// Cached tuner name.
    tuner_name: Option<Vec<u16>>,
    /// Cached space names (interned, see [`intern_wide`]).
    space_names: Vec<Option<&'static [u16]>>,
    /// Cached channel names (space -> channels), interned.
    channel_names: Vec<Vec<Option<&'static [u16]>>>,

    // ★追加：ポインタ版 GetTsStream 用の保持バッファ
    ts_out: Vec<u8>,
}

impl BonDriverState {
    fn new(config: ConnectionConfig) -> Self {
        Self {
            connection: Connection::new(config),
            cur_space: 0xFFFFFFFF,
            cur_channel: 0xFFFFFFFF,
            tuner_name: None,
            space_names: Vec::new(),
            channel_names: Vec::new(),
            ts_out: vec![0u8; 0], // 後で reserve でもOK
        }
    }
}

/// A BonDriver object as the host application sees it.
///
/// `#[repr(C)]` with `vtbl` first so a `*mut BonDriverInstance` is a valid
/// `IBonDriver*` for C++ virtual dispatch: every exported method receives this
/// pointer as `this` and resolves its own state from it.
#[repr(C)]
pub struct BonDriverInstance {
    /// vtable pointer — MUST stay the first field.
    vtbl: *const IBonDriver3Vtbl,
    /// State belonging to this object alone.
    state: Mutex<BonDriverState>,
}

// Safety: the vtable is static and the state is behind a mutex.
unsafe impl Send for BonDriverInstance {}
unsafe impl Sync for BonDriverInstance {}

/// Addresses of the instances that are currently alive.
///
/// Every exported method validates its `this` against this set before
/// dereferencing it, so a stale pointer (a host calling into an instance it
/// already released, or a null `this`) turns into a benign failure return
/// instead of undefined behaviour.  A panic must never cross the
/// `extern "system"` boundary, so the FFI surface can afford no unchecked
/// dereference.
static LIVE_INSTANCES: OnceCell<Mutex<HashSet<usize>>> = OnceCell::new();

fn live_instances() -> &'static Mutex<HashSet<usize>> {
    LIVE_INSTANCES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Process-wide intern table for the wide strings handed back by
/// `EnumTuningSpace` / `EnumChannelName`.
///
/// Those functions return a raw `LPCTSTR` into our own storage, and the host
/// decides when to read it.  EDCB in particular enumerates spaces/channels
/// during a channel scan and may keep the pointers past `Release` (and may
/// `CreateBonDriver` again afterwards).  Since `Release` now frees the
/// instance, per-instance storage would dangle — so the strings live for the
/// process instead.  Interning by content bounds the cost: the table holds one
/// entry per *distinct* name no matter how many instances are created and
/// destroyed, and the name space is already capped by `MAX_SPACES` /
/// `MAX_CHANNELS_PER_SPACE`.
static NAME_INTERN: OnceCell<Mutex<std::collections::HashMap<String, &'static [u16]>>> =
    OnceCell::new();

fn intern_wide(s: &str) -> &'static [u16] {
    let table = NAME_INTERN.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut table = table.lock();
    if let Some(existing) = table.get(s) {
        return existing;
    }
    let leaked: &'static [u16] = Box::leak(to_wide_string(s).into_boxed_slice());
    table.insert(s.to_string(), leaked);
    leaked
}

/// One-time process setup (logging + log level).  Shared by every instance;
/// only the *session* state is per-instance.
fn init_process_once() {
    static PROCESS_INIT: OnceCell<()> = OnceCell::new();
    PROCESS_INIT.get_or_init(|| {
        let log_level = crate::config::load_log_level();
        crate::logging::set_file_log_level(log_level);
        let _ = env_logger::Builder::new().filter_level(log_level).try_init();
        file_log!(info, "init_process_once: logging initialized (log_level={:?})", log_level);
    });
}

/// Create a new, independent BonDriver instance and return it as an
/// `IBonDriver*`.  Each call yields its own connection, ring buffer and
/// channel state.
pub fn create_instance() -> *mut IBonDriver {
    init_process_once();
    create_instance_with_config(load_config())
}

/// Create an instance from an explicit configuration.
///
/// Split out so tests can pick timeouts instead of inheriting whatever the
/// INI/environment says.
fn create_instance_with_config(config: ConnectionConfig) -> *mut IBonDriver {
    info!("BonDriver_NetworkProxy instance created");
    file_log!(info, "create_instance: server address: {}", config.server_addr);
    debug!("Server: {}", config.server_addr);

    let instance = Box::new(BonDriverInstance {
        vtbl: get_vtable_ptr(),
        state: Mutex::new(BonDriverState::new(config)),
    });
    let ptr = Box::into_raw(instance);
    live_instances().lock().insert(ptr as usize);
    file_log!(info, "create_instance: new instance at {:p}", ptr);

    ptr as *mut IBonDriver
}

/// Resolve the instance a call belongs to, or `None` if `this` is null or no
/// longer alive.
unsafe fn instance_of<'a>(this: *mut c_void) -> Option<&'a BonDriverInstance> {
    if this.is_null() {
        file_log!(error, "FFI call with null `this`; ignoring");
        return None;
    }
    if !live_instances().lock().contains(&(this as usize)) {
        file_log!(error, "FFI call on released/unknown instance {:p}; ignoring", this);
        return None;
    }
    Some(&*(this as *const BonDriverInstance))
}

/// Load configuration from INI file or environment.
fn load_config() -> ConnectionConfig {
    crate::config::load_config()
}

// =============================================================================
// IBonDriver methods
// =============================================================================

/// Open the tuner.
pub unsafe extern "system" fn open_tuner(this: *mut c_void) -> BOOL {
    file_log!(info, "OpenTuner called");
    debug!("OpenTuner called");

    let Some(instance) = instance_of(this) else { return 0 };

    // Take the connection out from under the lock and release it before any
    // network round-trip. Holding the instance lock across an RPC blocks every
    // other export on the same object — including GetTsStream on the host's
    // streaming thread — for as long as the server takes to answer.
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };

    // Connect to server if not connected
    let conn_state = connection.state();
    file_log!(debug, "OpenTuner: Connection state = {:?}", conn_state);

    // `Error` is retryable: a host that calls OpenTuner again after a failure
    // (TVTest's retry button, EDCB rebuilding a tuner between scans) must get a
    // fresh attempt rather than a permanently dead instance.
    if matches!(
        conn_state,
        ConnectionState::Disconnected | ConnectionState::Error
    ) {
        file_log!(info, "OpenTuner: Connecting to server...");
        if !connection.connect() {
            file_log!(error, "OpenTuner: Failed to connect to server");
            error!("Failed to connect to server");
            return 0;
        }
        file_log!(info, "OpenTuner: Connected to server");
    }

    // Open tuner
    file_log!(info, "OpenTuner: Opening tuner...");
    if connection.open_tuner() {
        file_log!(info, "OpenTuner: Tuner opened successfully");
        info!("Tuner opened successfully");
        1
    } else {
        file_log!(error, "OpenTuner: Failed to open tuner");
        error!("Failed to open tuner");
        0
    }
}

/// Close the tuner.
pub unsafe extern "system" fn close_tuner(this: *mut c_void) {
    file_log!(info, "CloseTuner called");
    debug!("CloseTuner called");
    let Some(instance) = instance_of(this) else { return };
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };
    connection.close_tuner();
    file_log!(info, "CloseTuner: Tuner closed");
    info!("Tuner closed");
}

/// Set channel (IBonDriver v1).
pub unsafe extern "system" fn set_channel(this: *mut c_void, channel: BYTE) -> BOOL {
    debug!("SetChannel called: channel={}", channel);
    let Some(instance) = instance_of(this) else { return 0 };
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };

    // RPC without the instance lock (see `open_tuner`).
    if connection.set_channel(channel, false) {
        let mut state = instance.state.lock();
        state.cur_channel = channel as u32;
        state.cur_space = 0;
        1
    } else {
        0
    }
}

/// Get signal level.
pub unsafe extern "system" fn get_signal_level(this: *mut c_void) -> f32 {
    trace!("GetSignalLevel called");
    let Some(instance) = instance_of(this) else { return 0.0 };
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };
    connection.get_signal_level()
}

/// Wait for TS stream to become available.
///
/// Mirrors the `WaitForMultipleObjects` call in BonDriverProxy(Ex):
/// instead of spinning with `Sleep(2)`, we block on the ring buffer's
/// Condvar and are woken immediately when the network receiver writes data.
pub unsafe extern "system" fn wait_ts_stream(this: *mut c_void, timeout_ms: DWORD) -> DWORD {
    file_log!(debug, "WaitTsStream called: timeout={}ms", timeout_ms);

    let Some(instance) = instance_of(this) else { return 0 };

    // ロックは短く、connection を clone して使う
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };

    // ストリーミング開始（必要な時だけ）
    if connection.state() == ConnectionState::TunerOpen {
        if !connection.start_stream() {
            file_log!(warn, "WaitTsStream: start_stream failed");
            return 0;
        }
    }

    let buffer = connection.buffer();

    // timeout==0 は「待たずに即返す」扱い（ポーリング）
    if timeout_ms == 0 {
        let ready = buffer.available() / TS_PACKET_SIZE;
        return ready.min(DWORD::MAX as usize) as DWORD;
    }

    // Block until data arrives or timeout — no spin loop, no sleep().
    let timeout = Duration::from_millis(timeout_ms as u64);
    if buffer.wait_data(timeout) {
        let ready = buffer.available() / TS_PACKET_SIZE;
        ready.min(DWORD::MAX as usize) as DWORD
    } else {
        0 // timeout
    }
}

/// Get the number of ready TS packets.
pub unsafe extern "system" fn get_ready_count(this: *mut c_void) -> DWORD {
    let Some(instance) = instance_of(this) else { return 0 };

    // connection を clone してロックを短くする
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };

    let buffer = connection.buffer();
    let ready = buffer.available() / TS_PACKET_SIZE;
    ready.min(DWORD::MAX as usize) as DWORD
}

/// Maximum buffer size for GetTsStream (16MB limit for safety).
const MAX_TS_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Most this overload will ever write in one call.
///
/// The `BYTE*` overload of `GetTsStream` has no way to learn the caller's
/// buffer size: `pdwSize` is OUT-only in the BonDriver interface. This
/// implementation used to read `*pdwSize` as if it were the capacity, while its
/// own comment admitted that "TVTest passes 0 or garbage" — a large garbage
/// value meant writing up to 64 KB into a buffer that might be far smaller, i.e.
/// corrupting the host's heap.
///
/// There is no signal that distinguishes a genuine capacity from garbage, so
/// the only sound bound is one that every plausible caller can hold: a single
/// TS packet. No host passes a TS buffer smaller than one packet. `pdwRemain`
/// still reports what is left, so a caller loops and drains at the same rate;
/// it just needs more calls.
///
/// Hosts that want throughput should use the `BYTE**` overload
/// ([`get_ts_stream_ptr`]), which hands back a pointer into our own buffer and
/// has no such ambiguity. recisdb-proxy requires it (see CLAUDE.md).
const COPY_OVERLOAD_MAX_WRITE: usize = TS_PACKET_SIZE;

/// Get TS stream data (`BYTE*` overload — copies into the caller's buffer).

pub unsafe extern "system" fn get_ts_stream(
    this: *mut c_void,
    dst: *mut BYTE,
    size: *mut DWORD,
    remain: *mut DWORD,
) -> BOOL {
    const TRUE: BOOL = 1;
    const FALSE: BOOL = 0;

    // --- 引数チェック ---
    if size.is_null() || remain.is_null() {
        crate::file_log!(error, "GetTsStream(copy): invalid args size/remain is null");
        return FALSE;
    }

    let Some(instance) = instance_of(this) else {
        *size = 0;
        *remain = 0;
        return FALSE;
    };

    // ログ間引き用カウンタ
    static LOG_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let count = LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // IN/OUT：呼び出し側が *size に「dst バッファ容量」を入れて渡す前提で扱う
    // （TVTestは通常 ptr版を使うが、互換性のため copy版も正しくしておく）
    let in_cap = *size as usize;

    // connection を clone（ロック時間短縮）
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };
    let buffer = connection.buffer();

    let avail = buffer.available();

    // たまに呼び出し状況をログ
    if count % 200 == 0 {
        crate::file_log!(
            debug,
            "GetTsStream(copy) call#{}: in_cap={} avail={} state={:?} dst_null={}",
            count,
            in_cap,
            avail,
            connection.state(),
            dst.is_null()
        );
    }

    // dst が null か、in_cap==0 の場合でも remain は返す（問い合わせ呼び出し対策）
    if dst.is_null() || in_cap == 0 {
        *size = 0;
        *remain = (avail.min(u32::MAX as usize)) as DWORD;

        if count % 200 == 0 {
            crate::file_log!(
                debug,
                "GetTsStream(copy) call#{}: QUERY -> out_size=0 remain={}",
                count,
                *remain
            );
        }
        return TRUE;
    }

    // `in_cap` はゴミの可能性があるので上限としてしか使わない。実際に書く量は
    // COPY_OVERLOAD_MAX_WRITE (TSパケット1個) で頭打ちにする。理由はその定数の
    // ドキュメントを参照。
    let mut cap = in_cap.min(COPY_OVERLOAD_MAX_WRITE);

    // TSパケット境界（188の倍数）に揃える（同期しやすくする）
    cap = (cap / TS_PACKET_SIZE) * TS_PACKET_SIZE;
    if cap == 0 {
        *size = 0;
        *remain = (avail.min(u32::MAX as usize)) as DWORD;
        return TRUE;
    }

    // avail も 188 単位で丸めて読む（余りは次回へ）
    let mut to_read = cap.min(avail);
    to_read = (to_read / TS_PACKET_SIZE) * TS_PACKET_SIZE;

    if to_read == 0 {
        *size = 0;
        *remain = (avail.min(u32::MAX as usize)) as DWORD;

        if count % 200 == 0 {
            crate::file_log!(
                debug,
                "GetTsStream(copy) call#{}: NO DATA -> out_size=0 remain={}",
                count,
                *remain
            );
        }
        return TRUE; // ★重要：データがなくても TRUE
    }

    // このオーバーロードを使っているホストには一度だけ助言する。
    static COPY_OVERLOAD_NOTICE: std::sync::Once = std::sync::Once::new();
    COPY_OVERLOAD_NOTICE.call_once(|| {
        crate::file_log!(
            warn,
            "GetTsStream(copy) in use: this overload cannot learn the caller's buffer size, \
             so it returns at most one TS packet per call. Use GetTsStream(BYTE**) for throughput."
        );
    });

    // コピー先スライス作成（to_read だけ確保済み領域に書く）
    let dest = std::slice::from_raw_parts_mut(dst, to_read);

    // 読み出し
    let (read_count, remaining) = buffer.read_into(dest);

    if read_count > 0 {
        buffer.consume(read_count);
    }

    *size = read_count as DWORD;
    *remain = (remaining.min(u32::MAX as usize)) as DWORD;

    // ログ（間引き）
    if count % 200 == 0 {
        let first = if read_count > 0 { dest[0] } else { 0 };
        crate::file_log!(
            debug,
            "GetTsStream(copy) call#{}: OK read={} remain={} to_read={} first=0x{:02X}",
            count,
            read_count,
            remaining,
            to_read,
            first
        );
    }

    // ★重要：read_count==0 でも TRUE（致命エラーでない限り）
    TRUE
}


/// Get TS stream data - pointer version (second overload).
/// Returns a pointer to internal buffer instead of copying.
pub unsafe extern "system" fn get_ts_stream_ptr(
    this: *mut c_void,
    dst: *mut *mut BYTE,
    size: *mut DWORD,
    remain: *mut DWORD,
) -> BOOL {
    const TRUE: BOOL = 1;
    const FALSE: BOOL = 0;

    // ===== 引数チェック =====
    if dst.is_null() || size.is_null() || remain.is_null() {
        crate::file_log!(error, "GetTsStream(ptr): invalid args dst/size/remain is null");
        return FALSE;
    }

    let Some(instance) = instance_of(this) else {
        *dst = std::ptr::null_mut();
        *size = 0;
        *remain = 0;
        return FALSE;
    };

    // TVTest は *size を 0 で呼ぶので「入力値」としては使わない
    // 1回に返す最大サイズ（TVTest側 DataBuffer=0x10000 に合わせるのが無難）
    const DEFAULT_CHUNK: usize = 0x10000; // 64KB
    let max_len = DEFAULT_CHUNK.min(MAX_TS_BUFFER_SIZE);

    // ===== ログ間引き用カウンタ =====
    static LOG_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let count = LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Single lock for the entire function.
    // Previously this function took the lock twice (once to clone the
    // connection, then again to access ts_out), causing two lock/unlock
    // round-trips per GetTsStream call.  Now we hold it once and access both
    // the ring buffer (via the Arc inside state) and ts_out together.
    let mut state = instance.state.lock();
    let buffer = Arc::clone(state.connection.buffer());

    let avail = buffer.available();

    if count % 200 == 0 {
        crate::file_log!(
            debug,
            "GetTsStream(ptr) call#{}: in_size={} avail={} state={:?}",
            count,
            *size,
            avail,
            state.connection.state()
        );
    }

    // ===== データが無い場合でも TRUE を返し remain を返す（TVTestが待ち時間を決める） =====
    if avail < TS_PACKET_SIZE {
        *dst = std::ptr::null_mut();
        *size = 0;
        *remain = (avail.min(u32::MAX as usize)) as DWORD;

        if count % 200 == 0 {
            crate::file_log!(
                debug,
                "GetTsStream(ptr) call#{}: NO DATA -> size=0 remain={}",
                count,
                *remain
            );
        }
        return TRUE;
    }

    // ===== 読み出しサイズ決定（188境界に揃える） =====
    let mut to_read = avail.min(max_len);
    to_read = (to_read / TS_PACKET_SIZE) * TS_PACKET_SIZE;

    if to_read == 0 {
        *dst = std::ptr::null_mut();
        *size = 0;
        *remain = (avail.min(u32::MAX as usize)) as DWORD;

        if count % 200 == 0 {
            crate::file_log!(
                debug,
                "GetTsStream(ptr) call#{}: to_read=0 -> size=0 remain={}",
                count,
                *remain
            );
        }
        return TRUE;
    }

    state.ts_out.resize(to_read, 0);

    // バッファからコピー
    let (read_count, remaining) = buffer.read_into(&mut state.ts_out[..]);

    if read_count > 0 {
        buffer.consume(read_count);

        *dst = state.ts_out.as_mut_ptr();
        *size = read_count as DWORD;
        *remain = (remaining.min(u32::MAX as usize)) as DWORD;

        let first = state.ts_out.first().copied().unwrap_or(0);

        if count % 200 == 0 {
            crate::file_log!(
                debug,
                "GetTsStream(ptr) call#{}: OK read={} remain={} to_read={} first=0x{:02X} ptr={:p}",
                count,
                read_count,
                remaining,
                to_read,
                first,
                *dst
            );
        }
    } else {
        // 読めなかった場合も TRUE（TVTestは size==0 を見て待つ）
        *dst = std::ptr::null_mut();
        *size = 0;
        *remain = (avail.min(u32::MAX as usize)) as DWORD;

        if count % 200 == 0 {
            crate::file_log!(
                warn,
                "GetTsStream(ptr) call#{}: READ ZERO (avail={} to_read={}) -> size=0 remain={}",
                count,
                avail,
                to_read,
                *remain
            );
        }
    }

    TRUE
}

/// Purge the TS stream buffer.
pub unsafe extern "system" fn purge_ts_stream(this: *mut c_void) {
    debug!("PurgeTsStream called");
    let Some(instance) = instance_of(this) else { return };
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };
    connection.purge_stream();
}

/// Release the BonDriver instance.
///
/// Per the BonDriver contract this destroys the object, so the instance is
/// deregistered and freed here.  Deregistering first means a second `Release`
/// (or any late call on the same pointer) is rejected by `instance_of` instead
/// of touching freed memory.
pub unsafe extern "system" fn release(this: *mut c_void) {
    file_log!(info, "Release called");
    debug!("Release called");

    if this.is_null() {
        file_log!(error, "Release with null `this`; ignoring");
        return;
    }
    if !live_instances().lock().remove(&(this as usize)) {
        file_log!(error, "Release on released/unknown instance {:p}; ignoring", this);
        return;
    }

    let instance = Box::from_raw(this as *mut BonDriverInstance);
    file_log!(info, "Release: Disconnecting...");
    // `Connection::drop` also disconnects, but do it explicitly so the server
    // sees the teardown before the allocation goes away.
    instance.state.lock().connection.disconnect();
    drop(instance);
    file_log!(info, "Release: Disconnected and instance freed");
}

// =============================================================================
// IBonDriver2 methods
// =============================================================================

/// Get the tuner name.
pub unsafe extern "system" fn get_tuner_name(_this: *mut c_void) -> LPCTSTR {
    file_log!(debug, "GetTunerName called");
    debug!("GetTunerName called");
    // Return a static name
    static NAME: OnceCell<Vec<u16>> = OnceCell::new();
    let name = NAME.get_or_init(|| to_wide_string("BonDriver_NetworkProxy"));
    file_log!(debug, "GetTunerName: returning pointer {:p}", name.as_ptr());
    name.as_ptr()
}

/// Check if the tuner is open.
pub unsafe extern "system" fn is_tuner_opening(this: *mut c_void) -> BOOL {
    trace!("IsTunerOpening called");
    let Some(instance) = instance_of(this) else { return 0 };
    let state = instance.state.lock();
    match state.connection.state() {
        ConnectionState::TunerOpen | ConnectionState::Streaming => 1,
        _ => 0,
    }
}

/// Maximum number of tuning spaces to cache.
const MAX_SPACES: usize = 256;

/// Maximum number of channels per space to cache.
const MAX_CHANNELS_PER_SPACE: usize = 1024;

/// Enumerate tuning space names.
pub unsafe extern "system" fn enum_tuning_space(this: *mut c_void, space: DWORD) -> LPCTSTR {
    file_log!(debug, "EnumTuningSpace called: space={}", space);
    debug!("EnumTuningSpace called: space={}", space);

    // Bounds check to prevent excessive memory allocation
    if space as usize >= MAX_SPACES {
        file_log!(debug, "EnumTuningSpace: space {} exceeds maximum {}", space, MAX_SPACES);
        debug!("EnumTuningSpace: space {} exceeds maximum {}", space, MAX_SPACES);
        return std::ptr::null();
    }

    let Some(instance) = instance_of(this) else { return std::ptr::null() };

    // Check cache first
    let connection = {
        let state = instance.state.lock();
        if (space as usize) < state.space_names.len() {
            if let Some(name) = state.space_names[space as usize] {
                file_log!(debug, "EnumTuningSpace: returning cached value for space {}", space);
                return name.as_ptr();
            }
        }
        state.connection.clone()
    };

    // Query server with the instance lock released (see `open_tuner`). A racing
    // caller may query the same space twice; interning makes that harmless.
    file_log!(debug, "EnumTuningSpace: querying server for space {}", space);
    match connection.enum_tuning_space(space) {
        Some(name) => {
            file_log!(debug, "EnumTuningSpace: got name '{}' for space {}", name, space);
            let wide = intern_wide(&name);
            let mut state = instance.state.lock();
            // Extend cache if needed
            while state.space_names.len() <= space as usize {
                state.space_names.push(None);
            }
            state.space_names[space as usize] = Some(wide);
            wide.as_ptr()
        }
        None => {
            file_log!(debug, "EnumTuningSpace: no name for space {}", space);
            std::ptr::null()
        }
    }
}

/// Enumerate channel names.
pub unsafe extern "system" fn enum_channel_name(
    this: *mut c_void,
    space: DWORD,
    channel: DWORD,
) -> LPCTSTR {
    debug!("EnumChannelName called: space={}, channel={}", space, channel);

    // Bounds check to prevent excessive memory allocation
    if space as usize >= MAX_SPACES {
        debug!("EnumChannelName: space {} exceeds maximum {}", space, MAX_SPACES);
        return std::ptr::null();
    }
    if channel as usize >= MAX_CHANNELS_PER_SPACE {
        debug!("EnumChannelName: channel {} exceeds maximum {}", channel, MAX_CHANNELS_PER_SPACE);
        return std::ptr::null();
    }

    let Some(instance) = instance_of(this) else { return std::ptr::null() };

    // Check cache first
    let connection = {
        let state = instance.state.lock();
        if (space as usize) < state.channel_names.len() {
            if (channel as usize) < state.channel_names[space as usize].len() {
                if let Some(name) = state.channel_names[space as usize][channel as usize] {
                    return name.as_ptr();
                }
            }
        }
        state.connection.clone()
    };

    // Query server with the instance lock released (see `open_tuner`).
    match connection.enum_channel_name(space, channel) {
        Some(name) => {
            let wide = intern_wide(&name);
            let mut state = instance.state.lock();
            // Extend cache if needed
            while state.channel_names.len() <= space as usize {
                state.channel_names.push(Vec::new());
            }
            while state.channel_names[space as usize].len() <= channel as usize {
                state.channel_names[space as usize].push(None);
            }
            state.channel_names[space as usize][channel as usize] = Some(wide);
            wide.as_ptr()
        }
        None => std::ptr::null(),
    }
}

/// Set channel by space (IBonDriver2).
pub unsafe extern "system" fn set_channel2(
    this: *mut c_void,
    space: DWORD,
    channel: DWORD,
) -> BOOL {
    file_log!(info, "SetChannel2 called: space={}, channel={}", space, channel);
    debug!("SetChannel2 called: space={}, channel={}", space, channel);
    let Some(instance) = instance_of(this) else { return 0 };

    // Tuning is up to three sequential round-trips (SetChannelSpace, then
    // PurgeStream and StartStream). Doing them under the instance lock froze
    // the host's streaming thread for the whole sequence.
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };

    file_log!(debug, "SetChannel2: Calling connection.set_channel_space...");

    let priority = connection.default_priority();
    let exclusive = connection.default_exclusive();
    file_log!(debug, "SetChannel2: priority={}, exclusive={}", priority, exclusive);

    if connection.set_channel_space(space, channel, priority, exclusive) {
        {
            let mut state = instance.state.lock();
            state.cur_space = space;
            state.cur_channel = channel;
        }

        // ★切替時にバッファ破棄（任意だが推奨）
        connection.purge_stream();

        // ★ここでストリーム開始（WaitTsStream に依存しない）
        let _ = connection.start_stream();

        file_log!(info, "SetChannel2: Success");
        1
    } else {
        file_log!(error, "SetChannel2: Failed");
        0
    }
}

/// Get current tuning space.
pub unsafe extern "system" fn get_cur_space(this: *mut c_void) -> DWORD {
    trace!("GetCurSpace called");
    let Some(instance) = instance_of(this) else { return 0xFFFFFFFF };
    let state = instance.state.lock();
    state.cur_space
}

/// Get current channel.
pub unsafe extern "system" fn get_cur_channel(this: *mut c_void) -> DWORD {
    trace!("GetCurChannel called");
    let Some(instance) = instance_of(this) else { return 0xFFFFFFFF };
    let state = instance.state.lock();
    state.cur_channel
}

// =============================================================================
// IBonDriver3 methods
// =============================================================================

/// Get total device count.
pub unsafe extern "system" fn get_total_device_num(_this: *mut c_void) -> DWORD {
    debug!("GetTotalDeviceNum called");
    // Return 1 as we only support one device through the proxy
    1
}

/// Get active device count.
pub unsafe extern "system" fn get_active_device_num(this: *mut c_void) -> DWORD {
    debug!("GetActiveDeviceNum called");
    let Some(instance) = instance_of(this) else { return 0 };
    let state = instance.state.lock();
    match state.connection.state() {
        ConnectionState::TunerOpen | ConnectionState::Streaming => 1,
        _ => 0,
    }
}

/// Set LNB power.
pub unsafe extern "system" fn set_lnb_power(this: *mut c_void, enable: BOOL) -> BOOL {
    debug!("SetLnbPower called: enable={}", enable);
    let Some(instance) = instance_of(this) else { return 0 };
    let connection = {
        let state = instance.state.lock();
        state.connection.clone()
    };

    if connection.set_lnb_power(enable != 0) {
        1
    } else {
        0
    }
}

// =============================================================================
// Vtable definitions
// =============================================================================

/// Static vtable for IBonDriver.
pub static IBONDRIVER_VTBL: IBonDriverVtbl = IBonDriverVtbl {
    open_tuner: Some(open_tuner),
    close_tuner: Some(close_tuner),
    set_channel: Some(set_channel),
    get_signal_level: Some(get_signal_level),
    wait_ts_stream: Some(wait_ts_stream),
    get_ready_count: Some(get_ready_count),
    get_ts_stream: Some(get_ts_stream),
    get_ts_stream_ptr: Some(get_ts_stream_ptr),
    purge_ts_stream: Some(purge_ts_stream),
    release: Some(release),
};

/// Static vtable for IBonDriver2.
pub static IBONDRIVER2_VTBL: IBonDriver2Vtbl = IBonDriver2Vtbl {
    base: IBONDRIVER_VTBL,
    get_tuner_name: Some(get_tuner_name),
    is_tuner_opening: Some(is_tuner_opening),
    enum_tuning_space: Some(enum_tuning_space),
    enum_channel_name: Some(enum_channel_name),
    set_channel2: Some(set_channel2),
    get_cur_space: Some(get_cur_space),
    get_cur_channel: Some(get_cur_channel),
};

/// Static vtable for IBonDriver3.
pub static IBONDRIVER3_VTBL: IBonDriver3Vtbl = IBonDriver3Vtbl {
    base: IBONDRIVER2_VTBL,
    get_total_device_num: Some(get_total_device_num),
    get_active_device_num: Some(get_active_device_num),
    set_lnb_power: Some(set_lnb_power),
};

/// Helper to create a mangled type name array.
/// MSVC mangled names look like: .?AVIBonDriver@@
#[cfg(windows)]
fn make_type_name(name: &[u8]) -> [u8; 32] {
    let mut arr = [0u8; 32];
    let len = name.len().min(31);
    arr[..len].copy_from_slice(&name[..len]);
    arr
}

/// PMD for simple single inheritance (no vbtable).
#[cfg(windows)]
const PMD_SIMPLE: PMD = PMD {
    mdisp: 0,
    pdisp: -1,  // -1 means no vbtable
    vdisp: 0,
};

/// Static RTTI data - RVAs will be fixed up at runtime.
/// We use a mutable static because RVAs depend on module base address.
#[cfg(windows)]
static mut RTTI_DATA: IBonDriver3RTTI = IBonDriver3RTTI {
    // Type descriptors with mangled names
    type_desc_ibondriver: RTTITypeDescriptor {
        p_vftable: std::ptr::null(),
        spare: std::ptr::null_mut(),
        name: [
            b'.', b'?', b'A', b'V', b'I', b'B', b'o', b'n',
            b'D', b'r', b'i', b'v', b'e', b'r', b'@', b'@',
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    },
    type_desc_ibondriver2: RTTITypeDescriptor {
        p_vftable: std::ptr::null(),
        spare: std::ptr::null_mut(),
        name: [
            b'.', b'?', b'A', b'V', b'I', b'B', b'o', b'n',
            b'D', b'r', b'i', b'v', b'e', b'r', b'2', b'@',
            b'@', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    },
    type_desc_ibondriver3: RTTITypeDescriptor {
        p_vftable: std::ptr::null(),
        spare: std::ptr::null_mut(),
        name: [
            b'.', b'?', b'A', b'V', b'I', b'B', b'o', b'n',
            b'D', b'r', b'i', b'v', b'e', b'r', b'3', b'@',
            b'@', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    },

    // Base class descriptors (RVAs will be fixed up)
    base_class_desc_ibondriver: RTTIBaseClassDescriptor {
        p_type_descriptor: 0,  // Will be fixed up
        num_contained_bases: 0,
        where_: PMD_SIMPLE,
        attributes: 0,
        p_class_hierarchy_descriptor: 0,  // Will be fixed up
    },
    base_class_desc_ibondriver2: RTTIBaseClassDescriptor {
        p_type_descriptor: 0,  // Will be fixed up
        num_contained_bases: 1,  // IBonDriver2 has 1 base (IBonDriver)
        where_: PMD_SIMPLE,
        attributes: 0,
        p_class_hierarchy_descriptor: 0,  // Will be fixed up
    },
    base_class_desc_ibondriver3: RTTIBaseClassDescriptor {
        p_type_descriptor: 0,  // Will be fixed up
        num_contained_bases: 2,  // IBonDriver3 has 2 bases (IBonDriver2, IBonDriver)
        where_: PMD_SIMPLE,
        attributes: 0,
        p_class_hierarchy_descriptor: 0,  // Will be fixed up
    },

    // Base class array (RVAs will be fixed up)
    base_class_array: RTTIBaseClassArray3 {
        entries: [0, 0, 0],  // Will be fixed up
    },

    // Class hierarchy descriptor
    class_hierarchy_ibondriver3: RTTIClassHierarchyDescriptor {
        signature: 1,  // x64
        attributes: 0,  // Single inheritance, no virtual bases
        num_base_classes: 3,  // IBonDriver3, IBonDriver2, IBonDriver
        p_base_class_array: 0,  // Will be fixed up
    },

    // Complete object locator
    complete_object_locator: RTTICompleteObjectLocator {
        signature: 1,  // x64
        offset: 0,
        cd_offset: 0,
        p_type_descriptor: 0,  // Will be fixed up
        p_class_hierarchy_descriptor: 0,  // Will be fixed up
        p_self: 0,  // Will be fixed up
    },
};

/// Flag to track if RTTI has been initialized.
#[cfg(windows)]
static RTTI_INITIALIZED: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static RTTI_INIT: Once = Once::new();

/// Calculate RVA from a pointer given the image base.
#[cfg(windows)]
fn calc_rva(ptr: *const u8, image_base: usize) -> i32 {
    (ptr as usize - image_base) as i32
}

/// Initialize RTTI data with correct RVAs.
/// Must be called before the vtable is used.
#[cfg(windows)]
fn init_rtti() {
    RTTI_INIT.call_once(|| {
        unsafe {
            // Get the module base address
            let image_base = get_module_base();
            file_log!(info, "init_rtti: Image base = 0x{:016x}", image_base);

            let rtti_ptr = &mut RTTI_DATA as *mut IBonDriver3RTTI;

            // Calculate RVAs for type descriptors
            let td_ibondriver_rva = calc_rva(
                &(*rtti_ptr).type_desc_ibondriver as *const _ as *const u8,
                image_base,
            );
            let td_ibondriver2_rva = calc_rva(
                &(*rtti_ptr).type_desc_ibondriver2 as *const _ as *const u8,
                image_base,
            );
            let td_ibondriver3_rva = calc_rva(
                &(*rtti_ptr).type_desc_ibondriver3 as *const _ as *const u8,
                image_base,
            );

            file_log!(info, "init_rtti: TypeDescriptor RVAs: IBonDriver=0x{:08x}, IBonDriver2=0x{:08x}, IBonDriver3=0x{:08x}",
                td_ibondriver_rva, td_ibondriver2_rva, td_ibondriver3_rva);

            // Calculate RVAs for base class descriptors
            let bcd_ibondriver_rva = calc_rva(
                &(*rtti_ptr).base_class_desc_ibondriver as *const _ as *const u8,
                image_base,
            );
            let bcd_ibondriver2_rva = calc_rva(
                &(*rtti_ptr).base_class_desc_ibondriver2 as *const _ as *const u8,
                image_base,
            );
            let bcd_ibondriver3_rva = calc_rva(
                &(*rtti_ptr).base_class_desc_ibondriver3 as *const _ as *const u8,
                image_base,
            );

            // Calculate RVA for class hierarchy
            let chd_rva = calc_rva(
                &(*rtti_ptr).class_hierarchy_ibondriver3 as *const _ as *const u8,
                image_base,
            );

            // Calculate RVA for base class array
            let bca_rva = calc_rva(
                &(*rtti_ptr).base_class_array as *const _ as *const u8,
                image_base,
            );

            // Calculate RVA for complete object locator
            let col_rva = calc_rva(
                &(*rtti_ptr).complete_object_locator as *const _ as *const u8,
                image_base,
            );

            file_log!(info, "init_rtti: CHD RVA=0x{:08x}, BCA RVA=0x{:08x}, COL RVA=0x{:08x}",
                chd_rva, bca_rva, col_rva);

            // Fix up base class descriptors
            (*rtti_ptr).base_class_desc_ibondriver.p_type_descriptor = td_ibondriver_rva;
            (*rtti_ptr).base_class_desc_ibondriver.p_class_hierarchy_descriptor = chd_rva;

            (*rtti_ptr).base_class_desc_ibondriver2.p_type_descriptor = td_ibondriver2_rva;
            (*rtti_ptr).base_class_desc_ibondriver2.p_class_hierarchy_descriptor = chd_rva;

            (*rtti_ptr).base_class_desc_ibondriver3.p_type_descriptor = td_ibondriver3_rva;
            (*rtti_ptr).base_class_desc_ibondriver3.p_class_hierarchy_descriptor = chd_rva;

            // Fix up base class array (order: derived first, then bases)
            (*rtti_ptr).base_class_array.entries[0] = bcd_ibondriver3_rva;
            (*rtti_ptr).base_class_array.entries[1] = bcd_ibondriver2_rva;
            (*rtti_ptr).base_class_array.entries[2] = bcd_ibondriver_rva;

            // Fix up class hierarchy descriptor
            (*rtti_ptr).class_hierarchy_ibondriver3.p_base_class_array = bca_rva;

            // Fix up complete object locator
            (*rtti_ptr).complete_object_locator.p_type_descriptor = td_ibondriver3_rva;
            (*rtti_ptr).complete_object_locator.p_class_hierarchy_descriptor = chd_rva;
            (*rtti_ptr).complete_object_locator.p_self = col_rva;

            file_log!(info, "init_rtti: RTTI fixup complete");
            file_log!(info, "init_rtti: COL at {:p}: sig={}, offset={}, cd_offset={}, p_type_desc=0x{:08x}, p_chd=0x{:08x}, p_self=0x{:08x}",
                &(*rtti_ptr).complete_object_locator,
                (*rtti_ptr).complete_object_locator.signature,
                (*rtti_ptr).complete_object_locator.offset,
                (*rtti_ptr).complete_object_locator.cd_offset,
                (*rtti_ptr).complete_object_locator.p_type_descriptor,
                (*rtti_ptr).complete_object_locator.p_class_hierarchy_descriptor,
                (*rtti_ptr).complete_object_locator.p_self);

            RTTI_INITIALIZED.store(true, Ordering::Release);
        }
    });
}

/// Get the module base address for this DLL.
#[cfg(windows)]
fn get_module_base() -> usize {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(lpModuleName: *const u16) -> *mut c_void;
    }

    // Get handle to our own DLL
    // We pass the DLL name to get our specific module
    let dll_name: Vec<u16> = "BonDriver_NetworkProxy.dll\0"
        .encode_utf16()
        .collect();

    let handle = unsafe { GetModuleHandleW(dll_name.as_ptr()) };
    if handle.is_null() {
        // Fallback: try to get by NULL (main executable, but won't work for DLL)
        // This is just for safety, shouldn't happen
        file_log!(error, "get_module_base: GetModuleHandleW failed for DLL name, trying NULL");
        let handle = unsafe { GetModuleHandleW(std::ptr::null()) };
        handle as usize
    } else {
        handle as usize
    }
}

/// Get pointer to the Complete Object Locator.
#[cfg(windows)]
pub fn get_rtti_locator_ptr() -> *const RTTICompleteObjectLocator {
    init_rtti();
    unsafe { &RTTI_DATA.complete_object_locator }
}

/// Mutable vtable with RTTI header - the RTTI pointer will be fixed up at runtime.
/// Initialized with null RTTI pointer, fixed up in init_rtti().
#[cfg(windows)]
static mut IBONDRIVER3_VTBL_WITH_RTTI: IBonDriver3VtblWithRTTI = IBonDriver3VtblWithRTTI {
    rtti_locator_ptr: std::ptr::null(),  // Will be fixed up at runtime
    vtable: IBonDriver3Vtbl {
        base: IBonDriver2Vtbl {
            base: IBonDriverVtbl {
                open_tuner: Some(open_tuner),
                close_tuner: Some(close_tuner),
                set_channel: Some(set_channel),
                get_signal_level: Some(get_signal_level),
                wait_ts_stream: Some(wait_ts_stream),
                get_ready_count: Some(get_ready_count),
                get_ts_stream: Some(get_ts_stream),
                get_ts_stream_ptr: Some(get_ts_stream_ptr),
                purge_ts_stream: Some(purge_ts_stream),
                release: Some(release),
            },
            get_tuner_name: Some(get_tuner_name),
            is_tuner_opening: Some(is_tuner_opening),
            enum_tuning_space: Some(enum_tuning_space),
            enum_channel_name: Some(enum_channel_name),
            set_channel2: Some(set_channel2),
            get_cur_space: Some(get_cur_space),
            get_cur_channel: Some(get_cur_channel),
        },
        get_total_device_num: Some(get_total_device_num),
        get_active_device_num: Some(get_active_device_num),
        set_lnb_power: Some(set_lnb_power),
    },
};

/// Flag to track if vtable RTTI pointer has been fixed up.
#[cfg(windows)]
static VTABLE_RTTI_INIT: Once = Once::new();

/// Get a pointer to the vtable for use as the object's vfptr.
#[cfg(windows)]
pub fn get_vtable_ptr() -> *const IBonDriver3Vtbl {
    // Initialize RTTI data first (calculates RVAs)
    init_rtti();

    // Fix up the vtable's RTTI pointer
    VTABLE_RTTI_INIT.call_once(|| {
        unsafe {
            let rtti_ptr = &RTTI_DATA.complete_object_locator as *const RTTICompleteObjectLocator;
            file_log!(info, "get_vtable_ptr: Fixing up RTTI locator pointer to {:p}", rtti_ptr);

            let vtbl_ptr = &mut IBONDRIVER3_VTBL_WITH_RTTI as *mut IBonDriver3VtblWithRTTI;
            (*vtbl_ptr).rtti_locator_ptr = rtti_ptr;

            file_log!(info, "get_vtable_ptr: RTTI locator pointer fixed up");
        }
    });

    unsafe { &IBONDRIVER3_VTBL_WITH_RTTI.vtable }
}

/// Get a pointer to the vtable for use as the object's vfptr.
/// On Linux, no RTTI overhead is needed — return the static vtable directly.
#[cfg(not(windows))]
pub fn get_vtable_ptr() -> *const IBonDriver3Vtbl {
    &IBONDRIVER3_VTBL as *const _
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EDCB opens one BonDriver object per tuner and scans channels on each of
    /// them concurrently.  Every object must carry its own channel state — a
    /// shared one makes the scan record whatever the *other* tuner last tuned.
    #[test]
    fn instances_do_not_share_channel_state() {
        let a = create_instance() as *mut c_void;
        let b = create_instance() as *mut c_void;
        assert_ne!(a, b);

        unsafe {
            // Nothing tuned yet on either object.
            assert_eq!(get_cur_space(a), 0xFFFFFFFF);
            assert_eq!(get_cur_channel(a), 0xFFFFFFFF);

            // Write state directly: set_channel2 would need a live server, and
            // what matters here is that the two objects address different
            // storage.
            {
                let mut st = instance_of(a).unwrap().state.lock();
                st.cur_space = 3;
                st.cur_channel = 11;
            }

            assert_eq!(get_cur_space(a), 3);
            assert_eq!(get_cur_channel(a), 11);
            assert_eq!(get_cur_space(b), 0xFFFFFFFF, "instance B must be untouched");
            assert_eq!(get_cur_channel(b), 0xFFFFFFFF, "instance B must be untouched");

            release(a);
            release(b);
        }
    }

    /// Tuning takes up to three sequential round-trips. Holding the instance
    /// lock across them froze the host's streaming thread for the whole
    /// sequence — on a WAN link that is seconds of black screen per channel
    /// change, not milliseconds.
    #[test]
    fn tuning_does_not_block_the_streaming_path() {
        use std::time::{Duration, Instant};

        let inst_ptr = create_instance_with_config(ConnectionConfig {
            // Long enough that the RPC is unmistakably still in flight while we
            // measure, short enough that the test finishes quickly.
            read_timeout: Duration::from_millis(600),
            ..ConnectionConfig::default()
        });
        let inst = inst_ptr as *mut c_void;

        unsafe {
            // Nobody answers the request, so SetChannel2 blocks in the RPC.
            let _req_rx = {
                let state = instance_of(inst).unwrap().state.lock();
                state.connection.buffer().write(&vec![0x47u8; TS_PACKET_SIZE * 4]);
                state.connection.stub_unanswered_rpc()
            };

            let addr = inst as usize;
            let tuning = std::thread::spawn(move || {
                set_channel2(addr as *mut c_void, 0, 0)
            });

            // Give the tuning thread time to enter the RPC and block.
            std::thread::sleep(Duration::from_millis(100));

            let start = Instant::now();
            let mut dst: *mut BYTE = std::ptr::null_mut();
            let mut size: DWORD = 0;
            let mut remain: DWORD = 0;
            let ok = get_ts_stream_ptr(inst, &mut dst, &mut size, &mut remain);
            let elapsed = start.elapsed();

            assert_eq!(ok, 1);
            assert!(size > 0, "buffered TS must still be readable during tuning");
            assert!(
                elapsed < Duration::from_millis(200),
                "GetTsStream waited {elapsed:?} on the tuning RPC's lock"
            );

            let _ = tuning.join();
            release(inst);
        }
    }

    /// `pdwSize` is OUT-only in the BonDriver interface, so a value coming in is
    /// not a capacity — the previous code treated it as one and would write up
    /// to 64 KB into whatever the host handed over. Nothing may be written past
    /// one TS packet no matter what the caller claims.
    #[test]
    fn the_copy_overload_never_writes_past_one_ts_packet() {
        let inst = create_instance() as *mut c_void;
        unsafe {
            // Put more than one packet in the ring buffer so the read is not
            // limited by availability.
            {
                let state = instance_of(inst).unwrap().state.lock();
                state.connection.buffer().write(&vec![0x47u8; TS_PACKET_SIZE * 8]);
            }

            // A generous destination with a canary past the packet boundary, and
            // a caller claiming a huge capacity.
            const CANARY: u8 = 0xCD;
            let mut dest = vec![CANARY; TS_PACKET_SIZE * 8];
            let mut size: DWORD = DWORD::MAX;
            let mut remain: DWORD = 0;

            let ok = get_ts_stream(inst, dest.as_mut_ptr(), &mut size, &mut remain);
            assert_eq!(ok, 1);
            assert!(
                size as usize <= TS_PACKET_SIZE,
                "wrote {size} bytes; must never exceed one TS packet"
            );
            assert_eq!(size as usize, TS_PACKET_SIZE, "a full packet was available");
            assert!(
                dest[TS_PACKET_SIZE..].iter().all(|&b| b == CANARY),
                "must not touch anything past what it reported"
            );
            // The rest is still queued, so a looping caller drains it.
            assert!(remain > 0);

            release(inst);
        }
    }

    /// A scan ends when enumeration returns null.  Out-of-range indices must
    /// terminate locally (without a server round-trip) so a host that keeps
    /// walking cannot loop forever.
    #[test]
    fn enumeration_terminates_at_the_bounds() {
        let inst = create_instance() as *mut c_void;
        unsafe {
            assert!(enum_tuning_space(inst, MAX_SPACES as DWORD).is_null());
            assert!(enum_channel_name(inst, MAX_SPACES as DWORD, 0).is_null());
            assert!(
                enum_channel_name(inst, 0, MAX_CHANNELS_PER_SPACE as DWORD).is_null()
            );
            release(inst);
        }
    }

    /// Calls arriving after `Release` (or with a null `this`) must fail
    /// benignly.  A panic or a use-after-free here would take the host process
    /// down — EDCB tears tuners down and rebuilds them between scans.
    #[test]
    fn calls_after_release_and_on_null_this_are_rejected() {
        let inst = create_instance() as *mut c_void;
        unsafe {
            release(inst);

            // Same pointer, now dead.
            assert_eq!(open_tuner(inst), 0);
            assert_eq!(is_tuner_opening(inst), 0);
            assert_eq!(get_cur_space(inst), 0xFFFFFFFF);
            assert!(enum_tuning_space(inst, 0).is_null());
            assert_eq!(get_ready_count(inst), 0);
            release(inst); // double release is ignored

            let null = std::ptr::null_mut();
            assert_eq!(open_tuner(null), 0);
            assert_eq!(get_signal_level(null), 0.0);
            assert_eq!(get_active_device_num(null), 0);
            release(null);
        }
    }

    /// Enumeration hands the host a raw pointer whose lifetime it does not
    /// control.  Interning keeps it valid for the whole process, so a name read
    /// after the instance that produced it was released is still sound, and
    /// re-interning the same name does not grow the table.
    #[test]
    fn interned_names_are_stable_and_outlive_their_instance() {
        let first = intern_wide("関東");
        let again = intern_wide("関東");
        assert_eq!(first.as_ptr(), again.as_ptr(), "same name must intern once");

        let expected: Vec<u16> = to_wide_string("関東");
        assert_eq!(first, expected.as_slice());
        assert_eq!(*first.last().unwrap(), 0, "must stay NUL-terminated");
    }
}
