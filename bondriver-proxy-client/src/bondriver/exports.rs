//! BonDriver exported functions.
#![allow(dead_code, static_mut_refs)]
//!
//! This module implements the BonDriver interface functions that are called
//! by the host application (e.g., TVTest).

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use log::{debug, error, info, trace};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use crate::bondriver::interface::*;
use crate::client::buffer::TS_PACKET_SIZE;
use crate::client::{Connection, ConnectionConfig, ConnectionState};
use crate::file_log;

/// Global state for the BonDriver instance.
struct BonDriverState {
    /// Connection to the proxy server.
    connection: Arc<Connection>,
    /// Current tuning space.
    cur_space: u32,
    /// Current channel.
    cur_channel: u32,
    /// Cached tuner name.
    tuner_name: Option<Vec<u16>>,
    /// Cached space names.
    space_names: Vec<Option<Vec<u16>>>,
    /// Cached channel names (space -> channels).
    channel_names: Vec<Vec<Option<Vec<u16>>>>,

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

/// Global instance.
static INSTANCE: OnceCell<Mutex<BonDriverState>> = OnceCell::new();

/// Get or create the global instance.
fn get_instance() -> &'static Mutex<BonDriverState> {
    INSTANCE.get_or_init(|| {
        file_log!(info, "get_instance: Initializing global state...");

        // Initialize logging
        let _ = env_logger::try_init();

        // Load configuration from INI file
        file_log!(info, "get_instance: Loading configuration...");
        let config = load_config();
        info!("BonDriver_NetworkProxy initialized");
        file_log!(info, "get_instance: Server address: {}", config.server_addr);
        debug!("Server: {}", config.server_addr);

        file_log!(info, "get_instance: Creating BonDriverState...");
        Mutex::new(BonDriverState::new(config))
    })
}

/// Load configuration from INI file or environment.
fn load_config() -> ConnectionConfig {
    crate::config::load_config()
}

// =============================================================================
// IBonDriver methods
// =============================================================================

/// Open the tuner.
pub unsafe extern "system" fn open_tuner(_this: *mut c_void) -> BOOL {
    file_log!(info, "OpenTuner called");
    debug!("OpenTuner called");

    file_log!(debug, "OpenTuner: Getting instance lock...");
    let state = get_instance().lock();
    file_log!(debug, "OpenTuner: Got instance lock");

    // Connect to server if not connected
    let conn_state = state.connection.state();
    file_log!(debug, "OpenTuner: Connection state = {:?}", conn_state);

    if conn_state == ConnectionState::Disconnected {
        file_log!(info, "OpenTuner: Connecting to server...");
        if !state.connection.connect() {
            file_log!(error, "OpenTuner: Failed to connect to server");
            error!("Failed to connect to server");
            return 0;
        }
        file_log!(info, "OpenTuner: Connected to server");
    }

    // Open tuner
    file_log!(info, "OpenTuner: Opening tuner...");
    if state.connection.open_tuner() {
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
pub unsafe extern "system" fn close_tuner(_this: *mut c_void) {
    file_log!(info, "CloseTuner called");
    debug!("CloseTuner called");
    let state = get_instance().lock();
    state.connection.close_tuner();
    file_log!(info, "CloseTuner: Tuner closed");
    info!("Tuner closed");
}

/// Set channel (IBonDriver v1).
pub unsafe extern "system" fn set_channel(_this: *mut c_void, channel: BYTE) -> BOOL {
    debug!("SetChannel called: channel={}", channel);
    let mut state = get_instance().lock();

    if state.connection.set_channel(channel, false) {
        state.cur_channel = channel as u32;
        state.cur_space = 0;
        1
    } else {
        0
    }
}

/// Get signal level.
pub unsafe extern "system" fn get_signal_level(_this: *mut c_void) -> f32 {
    trace!("GetSignalLevel called");
    let state = get_instance().lock();
    state.connection.get_signal_level()
}

/// Wait for TS stream to become available.
pub unsafe extern "system" fn wait_ts_stream(_this: *mut c_void, timeout_ms: DWORD) -> DWORD {
    file_log!(debug, "WaitTsStream called: timeout={}ms", timeout_ms);

    // ロックは短く、connection を clone して使う
    let connection = {
        let state = get_instance().lock();
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

    // 通常待機
    let timeout = Duration::from_millis(timeout_ms as u64);
    let start = Instant::now();

    loop {
        let avail = buffer.available();
        if avail >= TS_PACKET_SIZE {
            let ready = avail / TS_PACKET_SIZE;
            return ready.min(DWORD::MAX as usize) as DWORD; // 0でない＝準備OK
        }

        if start.elapsed() >= timeout {
            return 0; // timeout
        }

        // 応答性優先（10msだとTVTestのポーリングに負けることがある）
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Get the number of ready TS packets.
pub unsafe extern "system" fn get_ready_count(_this: *mut c_void) -> DWORD {
    // connection を clone してロックを短くする
    let connection = {
        let state = get_instance().lock();
        state.connection.clone()
    };

    let buffer = connection.buffer();
    let ready = buffer.available() / TS_PACKET_SIZE;
    ready.min(DWORD::MAX as usize) as DWORD
}

/// Default buffer size for GetTsStream copy version.
/// TVTest typically allocates 200KB+ buffer but doesn't pass the size.
/// We use 64KB as a safe default that works with most implementations.
const DEFAULT_TS_READ_SIZE: usize = 65536; // 64KB

/// Maximum buffer size for GetTsStream (16MB limit for safety).
const MAX_TS_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Get TS stream data.
/// Note: In standard BonDriver interface, size is OUTPUT only.
/// TVTest passes 0 or garbage for size, so we use a default read size.

pub unsafe extern "system" fn get_ts_stream(
    _this: *mut c_void,
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

    // ログ間引き用カウンタ
    static LOG_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let count = LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // IN/OUT：呼び出し側が *size に「dst バッファ容量」を入れて渡す前提で扱う
    // （TVTestは通常 ptr版を使うが、互換性のため copy版も正しくしておく）
    let in_cap = *size as usize;

    // connection を clone（ロック時間短縮）
    let connection = {
        let state = get_instance().lock();
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

    // 安全策：読み出し上限（異常に大きい値やゴミ値対策）
    // ※「呼び出し側容量」in_cap を超えて書くことは絶対にしない
    let mut cap = in_cap.min(DEFAULT_TS_READ_SIZE);

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
    _this: *mut c_void,
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

    // TVTest は *size を 0 で呼ぶので「入力値」としては使わない
    // 1回に返す最大サイズ（TVTest側 DataBuffer=0x10000 に合わせるのが無難）
    const DEFAULT_CHUNK: usize = 0x10000; // 64KB
    let max_len = DEFAULT_CHUNK.min(MAX_TS_BUFFER_SIZE);

    // ===== まず connection を clone（ロック時間短縮） =====
    let connection = {
        let state = get_instance().lock();
        state.connection.clone()
    };
    let buffer = connection.buffer();

    let avail = buffer.available();

    // ===== ログ間引き用カウンタ =====
    static LOG_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let count = LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // たまに呼び出し状況を出す（重いので間引き）
    if count % 200 == 0 {
        crate::file_log!(
            debug,
            "GetTsStream(ptr) call#{}: in_size={} avail={} state={:?}",
            count,
            *size,
            avail,
            connection.state()
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

    // ===== state.ts_out を使うのでここでロック =====
    let mut state = get_instance().lock();
    state.ts_out.resize(to_read, 0);

    // バッファからコピー
    let (read_count, remaining) = buffer.read_into(&mut state.ts_out[..]);

    if read_count > 0 {
        buffer.consume(read_count);

        *dst = state.ts_out.as_mut_ptr();
        *size = read_count as DWORD;
        *remain = (remaining.min(u32::MAX as usize)) as DWORD;

        // 先頭バイト確認（TS同期なら 0x47 が見えることが多い）
        let first = state.ts_out.get(0).copied().unwrap_or(0);

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
pub unsafe extern "system" fn purge_ts_stream(_this: *mut c_void) {
    debug!("PurgeTsStream called");
    let state = get_instance().lock();
    state.connection.purge_stream();
}

/// Release the BonDriver instance.
pub unsafe extern "system" fn release(_this: *mut c_void) {
    file_log!(info, "Release called");
    debug!("Release called");
    let state = get_instance().lock();
    file_log!(info, "Release: Disconnecting...");
    state.connection.disconnect();
    file_log!(info, "Release: Disconnected");
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
pub unsafe extern "system" fn is_tuner_opening(_this: *mut c_void) -> BOOL {
    trace!("IsTunerOpening called");
    let state = get_instance().lock();
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
pub unsafe extern "system" fn enum_tuning_space(_this: *mut c_void, space: DWORD) -> LPCTSTR {
    file_log!(debug, "EnumTuningSpace called: space={}", space);
    debug!("EnumTuningSpace called: space={}", space);

    // Bounds check to prevent excessive memory allocation
    if space as usize >= MAX_SPACES {
        file_log!(debug, "EnumTuningSpace: space {} exceeds maximum {}", space, MAX_SPACES);
        debug!("EnumTuningSpace: space {} exceeds maximum {}", space, MAX_SPACES);
        return std::ptr::null();
    }

    let mut state = get_instance().lock();

    // Check cache first
    if (space as usize) < state.space_names.len() {
        if let Some(ref name) = state.space_names[space as usize] {
            file_log!(debug, "EnumTuningSpace: returning cached value for space {}", space);
            return name.as_ptr();
        }
    }

    // Query server
    file_log!(debug, "EnumTuningSpace: querying server for space {}", space);
    match state.connection.enum_tuning_space(space) {
        Some(name) => {
            file_log!(debug, "EnumTuningSpace: got name '{}' for space {}", name, space);
            let wide = to_wide_string(&name);
            // Extend cache if needed
            while state.space_names.len() <= space as usize {
                state.space_names.push(None);
            }
            state.space_names[space as usize] = Some(wide);
            state.space_names[space as usize].as_ref().unwrap().as_ptr()
        }
        None => {
            file_log!(debug, "EnumTuningSpace: no name for space {}", space);
            std::ptr::null()
        }
    }
}

/// Enumerate channel names.
pub unsafe extern "system" fn enum_channel_name(
    _this: *mut c_void,
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

    let mut state = get_instance().lock();

    // Check cache first
    if (space as usize) < state.channel_names.len() {
        if (channel as usize) < state.channel_names[space as usize].len() {
            if let Some(ref name) = state.channel_names[space as usize][channel as usize] {
                return name.as_ptr();
            }
        }
    }

    // Query server
    match state.connection.enum_channel_name(space, channel) {
        Some(name) => {
            let wide = to_wide_string(&name);
            // Extend cache if needed
            while state.channel_names.len() <= space as usize {
                state.channel_names.push(Vec::new());
            }
            while state.channel_names[space as usize].len() <= channel as usize {
                state.channel_names[space as usize].push(None);
            }
            state.channel_names[space as usize][channel as usize] = Some(wide);
            state.channel_names[space as usize][channel as usize]
                .as_ref()
                .unwrap()
                .as_ptr()
        }
        None => std::ptr::null(),
    }
}

/// Set channel by space (IBonDriver2).
pub unsafe extern "system" fn set_channel2(
    _this: *mut c_void,
    space: DWORD,
    channel: DWORD,
) -> BOOL {
    file_log!(info, "SetChannel2 called: space={}, channel={}", space, channel);
    debug!("SetChannel2 called: space={}, channel={}", space, channel);
    let mut state = get_instance().lock();

    file_log!(debug, "SetChannel2: Calling connection.set_channel_space...");

    let priority = state.connection.default_priority();
    let exclusive = state.connection.default_exclusive();
    file_log!(debug, "SetChannel2: priority={}, exclusive={}", priority, exclusive);

    if state.connection.set_channel_space(space, channel, priority, exclusive) {
        state.cur_space = space;
        state.cur_channel = channel;

        // ★切替時にバッファ破棄（任意だが推奨）
        state.connection.purge_stream();

        // ★ここでストリーム開始（WaitTsStream に依存しない）
        let _ = state.connection.start_stream();

        file_log!(info, "SetChannel2: Success");
        1
    } else {
        file_log!(error, "SetChannel2: Failed");
        0
    }
}

/// Get current tuning space.
pub unsafe extern "system" fn get_cur_space(_this: *mut c_void) -> DWORD {
    trace!("GetCurSpace called");
    let state = get_instance().lock();
    state.cur_space
}

/// Get current channel.
pub unsafe extern "system" fn get_cur_channel(_this: *mut c_void) -> DWORD {
    trace!("GetCurChannel called");
    let state = get_instance().lock();
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
pub unsafe extern "system" fn get_active_device_num(_this: *mut c_void) -> DWORD {
    debug!("GetActiveDeviceNum called");
    let state = get_instance().lock();
    match state.connection.state() {
        ConnectionState::TunerOpen | ConnectionState::Streaming => 1,
        _ => 0,
    }
}

/// Set LNB power.
pub unsafe extern "system" fn set_lnb_power(_this: *mut c_void, enable: BOOL) -> BOOL {
    debug!("SetLnbPower called: enable={}", enable);
    let state = get_instance().lock();

    if state.connection.set_lnb_power(enable != 0) {
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
fn make_type_name(name: &[u8]) -> [u8; 32] {
    let mut arr = [0u8; 32];
    let len = name.len().min(31);
    arr[..len].copy_from_slice(&name[..len]);
    arr
}

/// PMD for simple single inheritance (no vbtable).
const PMD_SIMPLE: PMD = PMD {
    mdisp: 0,
    pdisp: -1,  // -1 means no vbtable
    vdisp: 0,
};

/// Static RTTI data - RVAs will be fixed up at runtime.
/// We use a mutable static because RVAs depend on module base address.
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
static RTTI_INITIALIZED: AtomicBool = AtomicBool::new(false);
static RTTI_INIT: Once = Once::new();

/// Calculate RVA from a pointer given the image base.
fn calc_rva(ptr: *const u8, image_base: usize) -> i32 {
    (ptr as usize - image_base) as i32
}

/// Initialize RTTI data with correct RVAs.
/// Must be called before the vtable is used.
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

#[cfg(not(windows))]
fn get_module_base() -> usize {
    0
}

/// Get pointer to the Complete Object Locator.
pub fn get_rtti_locator_ptr() -> *const RTTICompleteObjectLocator {
    init_rtti();
    unsafe { &RTTI_DATA.complete_object_locator }
}

/// Mutable vtable with RTTI header - the RTTI pointer will be fixed up at runtime.
/// Initialized with null RTTI pointer, fixed up in init_rtti().
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
static VTABLE_RTTI_INIT: Once = Once::new();

/// Get a pointer to the vtable portion of IBONDRIVER3_VTBL_WITH_RTTI.
/// This is what the object's vfptr should point to - it allows vtable[-1] to access RTTI.
pub fn get_vtable_ptr() -> *const IBonDriver3Vtbl {
    // Initialize RTTI data first (calculates RVAs)
    init_rtti();

    // Fix up the vtable's RTTI pointer
    VTABLE_RTTI_INIT.call_once(|| {
        unsafe {
            let rtti_ptr = &RTTI_DATA.complete_object_locator as *const RTTICompleteObjectLocator;
            file_log!(info, "get_vtable_ptr: Fixing up RTTI locator pointer to {:p}", rtti_ptr);

            // We need to cast away the const-ness to fix up the pointer
            let vtbl_ptr = &mut IBONDRIVER3_VTBL_WITH_RTTI as *mut IBonDriver3VtblWithRTTI;
            (*vtbl_ptr).rtti_locator_ptr = rtti_ptr;

            file_log!(info, "get_vtable_ptr: RTTI locator pointer fixed up");
        }
    });

    unsafe { &IBONDRIVER3_VTBL_WITH_RTTI.vtable }
}
