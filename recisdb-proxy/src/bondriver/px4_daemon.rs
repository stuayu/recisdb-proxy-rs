//! Client for px4_drv's user-space daemon (`DriverHost_PX4`), used on macOS.
//!
//! # Why this exists
//!
//! On Linux, px4_drv is a kernel driver exposing `/dev/px4videoN` character
//! devices, which [`super::unix::CharDevTuner`] drives with `ioctl`. macOS
//! cannot create `/dev/*` nodes from user space without a kernel extension,
//! so the macOS port of px4_drv is instead a daemon that owns the USB device
//! and exposes it over two UNIX domain sockets:
//!
//! - **control** (`/tmp/px4_ctrl_pipe.sock`) — fixed-layout command structs,
//!   request/response, one receiver per connection
//! - **data** (`/tmp/px4_data_pipe.sock`) — raw TS or TLV, after the client claims a
//!   `data_id` handed out by `OPEN`
//!
//! The wire format is the C++ `px4::command` structs from px4_drv
//! (`winusb/src/common/command.hpp`, shared with the macOS build), laid out
//! with `#pragma pack(8)`. The sizes and offsets encoded below were taken
//! from that header compiled with the macOS build's own include paths, not
//! derived by hand — `wchar_t` is 4 bytes there, which is what makes
//! `ReceiverInfo` 812 bytes rather than the 428 a 2-byte `wchar_t` would give.
//!
//! # Session sequence
//!
//! Mirrors px4_drv's own `px4rec` reference client:
//!
//! ```text
//! connect(ctrl) → GET_VERSION → OPEN{systems, index} ─→ data_id
//!   → connect(data) → SET_DATA_ID{data_id}
//!   → SET_PARAMS{system, freq_kHz} → TUNE{timeout} → PURGE
//!   → SET_CAPTURE{true} → read TS or TLV …
//! teardown: SET_CAPTURE{false} → CLOSE
//! ```
//!
//! # CATV / raw TLV
//!
//! Space 3 is CATV C13–C63 (ISDB-C), or J.83B H01–H77 with `+j83b`.
//! Receiver suffixes `+tsmf=0..14`, `+tlv`, `+qam=64|256`, and
//! `+coffset=<signed kHz>` may be combined with `+lnb` in any order.
//! `+j83b` is exclusive of those suffixes other than `+lnb` and requires
//! daemon-wide `ExperimentalJ83B=true`.
//! `+tlv` implies TSMF relative stream 0 unless explicitly selected.
//! Space 4 (CATV-TLV) tunes the same carriers with TLV output, and space 5
//! (CATV-8K) combines a multi-carrier group (BS8K) from several receivers;
//! see [`super::tsmf_bond`].
//! Register raw TLV drivers with `bon_drivers.stream_format='mmttlv'`;
//! the reader must run dantto4k before TS parsing or broadcast. Raw TLV
//! cannot pass through processing that assumes 188-byte TS boundaries.
//!
//! # Prerequisite
//!
//! `DriverHost_PX4` must already be running. This module deliberately does
//! not spawn it: `px4rec` auto-starts a sibling daemon because it is a
//! short-lived CLI, but a long-running server silently launching a background
//! process that owns the hardware is a side effect an operator should opt
//! into, not inherit. If the control socket cannot be connected, tuner open
//! fails with a message naming the daemon.

use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use super::tsmf_bond::{self, BondCombiner, ExtHeader};

/// Default control socket, matching px4_drv's macOS daemon.
const DEFAULT_CTRL_SOCK: &str = "/tmp/px4_ctrl_pipe.sock";
/// Default data socket.
const DEFAULT_DATA_SOCK: &str = "/tmp/px4_data_pipe.sock";

/// Tuner path prefix that selects this backend, e.g. `px4daemon:0`.
pub const PATH_PREFIX: &str = "px4daemon:";

// ---------------------------------------------------------------------
// Wire protocol (px4::command, #pragma pack(8), little-endian)
// ---------------------------------------------------------------------

mod cmd {
    pub const GET_VERSION: u32 = 1;
    pub const OPEN: u32 = 8;
    pub const CLOSE: u32 = 9;
    pub const SET_CAPTURE: u32 = 11;
    pub const GET_PARAMS: u32 = 16;
    pub const SET_PARAMS: u32 = 17;
    pub const TUNE: u32 = 19;
    pub const CHECK_LOCK: u32 = 20;
    pub const SET_LNB_VOLTAGE: u32 = 24;
    pub const READ_STATS: u32 = 32;
}

mod status {
    pub const NONE: u32 = 0;
    pub const SUCCEEDED: u32 = 1;
}

mod data_cmd {
    pub const SET_DATA_ID: u32 = 1;
    pub const PURGE: u32 = 8;
}

mod system {
    pub const ISDB_T: u32 = 0x10;
    pub const ISDB_S: u32 = 0x20;
    pub const ISDB_C: u32 = 0x40;
}

/// Sentinel for "no `data_id` yet"; outside the `u32` range the wire uses.
const NO_DATA_ID: u64 = u64::MAX;

/// `StatType::CNR`.
const STAT_CNR: u32 = 2;

// Sizes/offsets measured from the real header (see module doc comment).
const HEADER_SIZE: usize = 8;
const RECEIVER_INFO_SIZE: usize = 812;
const OPEN_CMD_SIZE: usize = HEADER_SIZE + RECEIVER_INFO_SIZE; // 820
const RI_SYSTEMS_OFF: usize = 800;
const RI_INDEX_OFF: usize = 804;
const RI_DATA_ID_OFF: usize = 808;

const VERSION_CMD_SIZE: usize = 16;
const CLOSE_CMD_SIZE: usize = 8;
const CAPTURE_CMD_SIZE: usize = 12;
const PARAMS_CMD_SIZE: usize = 28;
const TUNE_CMD_SIZE: usize = 12;
const CHECK_LOCK_CMD_SIZE: usize = 12;
const LNB_CMD_SIZE: usize = 12;
const STATS_CMD_SIZE: usize = 20;
const DATA_CMD_SIZE: usize = 8;

/// How long the daemon may spend acquiring a lock before `TUNE` gives up.
///
/// px4rec uses 30 s, which suits a CLI that was told exactly which channel to
/// record. A server spends most of its `TUNE` calls on channels that are not
/// receivable at all — a channel scan walks every UHF channel plus BS/CS
/// whether or not an antenna is attached — and there the timeout is pure
/// dead time. A real lock completes well inside a second; 5 s leaves ample
/// margin for a weak signal while keeping a full-band scan minutes rather
/// than half an hour.
const TUNE_TIMEOUT_MS: u32 = 5_000;

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[off..off + 4]);
    u32::from_le_bytes(b)
}

fn get_i32(buf: &[u8], off: usize) -> i32 {
    get_u32(buf, off) as i32
}

fn header(cmd: u32) -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    put_u32(&mut h, 0, cmd);
    put_u32(&mut h, 4, status::NONE);
    h
}

// ---------------------------------------------------------------------
// Channel → frequency
// ---------------------------------------------------------------------

// Source: px4_drv/winusb/pkg/BonDriver_PX4/BonDriver_PX4-C.ChSet.txt,
// CATV space, seventh column (kHz), C13..C63. These are defaults; operators
// can replace them with +coffset for a different cable network.
const CATV_OFFSETS_KHZ: [i32; 51] = [
    -200, -225, -225, -250, -250, -250, -250, -250, -275, -275, -350, -350, -350, -350, -375, -375,
    -375, -400, -400, -400, -400, -425, -425, -425, -425, -450, -450, -450, -475, -475, -475, -475,
    -500, -500, -500, -500, -525, -525, -525, -525, -525, -525, -550, -550, -550, -550, -575, -575,
    -575, -600, -600,
];

fn catv_frequency(channel: u32, offset: Option<i32>) -> Result<u32, io::Error> {
    let default = CATV_OFFSETS_KHZ.get(channel as usize).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "CATV channel out of range (0-50)",
        )
    })?;
    let physical = if channel <= 9 {
        channel + 3
    } else {
        channel + 12
    };
    let center = 93_000i64 + i64::from(physical) * 6_000 + if physical == 12 { 2_000 } else { 0 };
    let frequency = center + i64::from(offset.unwrap_or(*default));
    if frequency <= 0 || frequency > i64::from(u32::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "adjusted CATV frequency out of range",
        ));
    }
    Ok(frequency as u32)
}

/// Number of CATV channels in spaces 3/4: C13–C63 (0..=50), then the UHF
/// band U13–U62 (51..=100). Cable operators put 256QAM carriers — including
/// BS4K/CS4K TLV transmodulation — above C63, on the UHF raster.
const CATV_CHANNELS: u32 = 101;
/// First UHF-band channel index in the CATV plan.
const CATV_UHF_FIRST: u32 = 51;

/// Space index of the CATV raw-TLV plan (same carriers as space 3, tuned with
/// TSMF relative stream 0 and TLV output). Lets one receiver path serve both
/// the normal ISDB-C TS channels (space 3) and the 4K MMT/TLV channels.
pub const CATV_TLV_SPACE: u32 = 4;

/// Space index of CATV multi-carrier groups (BS8K). A channel is the
/// frequency of the group's first carrier (`carrier_sequence` 1); the other
/// carriers are found from their TSMF headers and received on further
/// receivers of the same daemon, then combined into one TLV stream.
pub const CATV_BOND_SPACE: u32 = 5;

/// How long to wait for a TSMF header after TUNE. Frames repeat every ~2 ms;
/// a carrier with no header in this time is plain TS or not TSMF at all.
const TSMF_HEADER_WAIT: Duration = Duration::from_millis(800);

/// How far from the first carrier to look for the rest of a group. Groups are
/// laid out on adjacent channels in practice (BS8K: 569/575/581 MHz); every
/// probe of an empty frequency costs a TUNE timeout.
const BOND_SEARCH_RANGE_KHZ: u32 = 60_000;

/// Raw bytes to read before judging a carrier's TSMF header (~0.1 s at 38 Mbps).
const HEADER_SAMPLE_BYTES: usize = 512 * 1024;

/// Centre frequency (kHz) of CATV plan channel `channel`.
///
/// C13–C63 apply the ChSet default offset (or `+coffset`). The UHF part has
/// no ChSet table; the measured carriers sit exactly on the 6 MHz raster
/// (e.g. U32 = 587 MHz), so `+coffset` does not apply there.
fn catv_plan_frequency(channel: u32, offset: Option<i32>) -> Result<u32, io::Error> {
    if channel < CATV_UHF_FIRST {
        return catv_frequency(channel, offset);
    }
    if channel >= CATV_CHANNELS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "CATV channel {} out of range (0-{})",
                channel,
                CATV_CHANNELS - 1
            ),
        ));
    }
    Ok(473_000 + (channel - CATV_UHF_FIRST) * 6_000)
}

fn catv_channel_label(channel: u32) -> String {
    if channel < CATV_UHF_FIRST {
        format!("C{}", channel + 13)
    } else {
        format!("U{}", channel - CATV_UHF_FIRST + 13)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ReceiverOptions {
    lnb: bool,
    tsmf: Option<u32>,
    tlv: bool,
    qam: Option<u32>,
    coffset: Option<i32>,
    /// 空間 3 を J.83 Annex B の周波数配置として扱う (`+j83b`)。
    j83b: bool,
}

/// J.83 Annex B (スカパー!プレミアムサービス光) の搬送波数。
/// 93.25 MHz から 6.5 MHz 間隔で 587.25 MHz まで。
const J83B_CHANNELS: u32 = 77;

/// Space 3 frequency layout selected by a px4daemon path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space3Plan {
    Catv,
    J83b,
}

/// Return the configured space 3 layout for a px4daemon path.
///
/// Parsing the full path keeps this platform-independent and ensures invalid
/// suffix combinations do not accidentally trigger a channel-table reset.
pub fn space_channel_plan(path: &str) -> Option<Space3Plan> {
    let (_, options, _, _) = parse_path(path).ok()?;
    Some(if options.j83b {
        Space3Plan::J83b
    } else {
        Space3Plan::Catv
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TuneTarget {
    system: u32,
    freq_khz: u32,
    modulation: u32,
    /// The tune delivers raw TLV (TSMF relative stream, or a combined group).
    tlv: bool,
    /// Combine a multi-carrier group; the daemon outputs every slot unfiltered.
    bonded: bool,
}

/// Whether `options` expose the CATV TLV space (4). A `+tlv` receiver is
/// already TLV-only on space 3, and J.83B carries no TSMF.
fn has_tlv_space(options: &ReceiverOptions) -> bool {
    !options.tlv && !options.j83b
}

/// Resolve all channel bounds and tuning parameters from one pure plan.
fn tune_target(
    space: u32,
    channel: u32,
    options: &ReceiverOptions,
) -> Result<TuneTarget, io::Error> {
    // Raw TLV is carried over TSMF on ISDB-C only. Tuning GR/BS/CS with +tlv
    // yields no usable stream, and a scan would waste minutes per channel.
    if options.tlv && space != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "px4daemon +tlv is only valid for the CATV space (3)",
        ));
    }
    let bond_space = space == CATV_BOND_SPACE && has_tlv_space(options);
    let tlv_space = (space == CATV_TLV_SPACE && has_tlv_space(options)) || bond_space;
    if space == 3 || tlv_space {
        let (freq_khz, modulation) = if options.j83b {
            (j83b_frequency(channel)?, 256)
        } else if tlv_space {
            // TLV transmodulation is always 256QAM; skipping the 64QAM probe
            // saves a disposable receiver session per carrier.
            (
                catv_plan_frequency(channel, options.coffset)?,
                options.qam.unwrap_or(256),
            )
        } else {
            (
                catv_plan_frequency(channel, options.coffset)?,
                options.qam.unwrap_or(0),
            )
        };
        return Ok(TuneTarget {
            system: system::ISDB_C,
            freq_khz,
            modulation,
            tlv: options.tlv || tlv_space,
            bonded: bond_space,
        });
    }
    let (sys, freq) = space_channel_to_freq(space, channel)?;
    Ok(TuneTarget {
        system: sys,
        freq_khz: freq,
        modulation: options.qam.unwrap_or(0),
        tlv: false,
        bonded: false,
    })
}

fn channel_name(space: u32, channel: u32, options: &ReceiverOptions) -> Option<String> {
    tune_target(space, channel, options).ok()?;
    match space {
        0 => Some(format!("GR{}", channel + 13)),
        1 => Some(format!("BS{}", channel * 2 + 1)),
        2 => Some(format!("CS{}", channel * 2 + 2)),
        3 if options.j83b => Some(format!("H{:02}", channel + 1)),
        3 | CATV_TLV_SPACE | CATV_BOND_SPACE => Some(catv_channel_label(channel)),
        _ => None,
    }
}

/// `+j83b` の空間 3 のチャンネル番号 (0..77) を中心周波数 kHz へ。
///
/// CATV の C チャンネル (6 MHz 間隔) とは配置が違い、px4_drv の J.83B は
/// ISDB-C 用の周波数補正も使わない。配置は利用者の受信報告 (2026-10、東京) と
/// px4_drv 同梱の `BonDriver_PX4-C.ChSet.SPHD-J83B.txt` に合わせてある。
/// daemon 側で `ExperimentalJ83B=true` にしていないと、ISDB-C として選局され
/// ロックしない。J.83B の信号がある環境では未検証。
fn j83b_frequency(channel: u32) -> Result<u32, io::Error> {
    if channel >= J83B_CHANNELS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "J.83B channel {} out of range (0-{})",
                channel,
                J83B_CHANNELS - 1
            ),
        ));
    }
    Ok(93_250 + 6_500 * channel)
}

// ParameterType values from px4_drv/winusb/src/common/command.hpp.
fn params_command(code: u32, sys: u32, freq: u32, params: &[(u32, u32)]) -> Vec<u8> {
    // ParameterSet contains params[1] even when num=0. Additional entries
    // extend CtrlParamsCmd by 8 bytes each, without trailing padding.
    let mut buf = vec![0; PARAMS_CMD_SIZE + params.len().saturating_sub(1) * 8];
    buf[..HEADER_SIZE].copy_from_slice(&header(code));
    put_u32(&mut buf, 8, sys);
    put_u32(&mut buf, 12, freq);
    put_u32(&mut buf, 16, params.len() as u32);
    for (i, &(kind, value)) in params.iter().enumerate() {
        put_u32(&mut buf, 20 + i * 8, kind);
        put_u32(&mut buf, 24 + i * 8, value);
    }
    buf
}

fn tuning_params(
    sys: u32,
    options: &ReceiverOptions,
    modulation: u32,
    probe: bool,
    raw: bool,
) -> Vec<(u32, u32)> {
    if sys != system::ISDB_C {
        return Vec::new();
    }
    let mut params = vec![(1, 6), (2, 5274), (3, modulation)];
    if raw && !probe {
        // TSMF_MODE=2: every slot, TSMF headers and nulls included, for
        // combining a multi-carrier group (px4_drv ReceiverBase).
        params.push((4, 2));
    } else if !probe {
        if let Some(relative) = options.tsmf.or(options.tlv.then_some(0)) {
            params.extend([(4, 1), (5, relative), (6, 0)]);
            if options.tlv {
                params.push((7, 1));
            }
        }
    }
    params
}

/// Convert this crate's `(space, channel)` indices to the daemon's
/// `(system, freq_kHz)`.
///
/// The space/channel convention is the same one
/// [`super::unix::CharDevTuner`] uses, so a `channels` row scanned on Linux
/// addresses the same physical channel here:
/// space 0 = GR (channel 0..49 → UHF 13..62), 1 = BS, 2 = CS110, 3 = CATV (channel 0..50 → C13..C63).
///
/// The frequency formulas are px4_drv's own (`px4rec.cpp`).
pub(super) fn space_channel_to_freq(space: u32, channel: u32) -> Result<(u32, u32), io::Error> {
    match space {
        0 => {
            if channel > 49 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("GR channel {} out of range (0-49)", channel),
                ));
            }
            let uhf = channel + 13; // UHF 13..62
            Ok((system::ISDB_T, 395_143 + uhf * 6_000))
        }
        1 => {
            if channel > 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("BS channel {} out of range (0-11)", channel),
                ));
            }
            Ok((system::ISDB_S, 1_049_480 + 38_360 * channel))
        }
        2 => {
            if channel > 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("CS channel {} out of range (0-11)", channel),
                ));
            }
            Ok((system::ISDB_S, 1_613_000 + 40_000 * channel))
        }
        3 => Ok((system::ISDB_C, catv_frequency(channel, None)?)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown tuning space {}", space),
        )),
    }
}

/// Whether tuning `space` on this px4daemon path yields raw MMT/TLV rather
/// than TS (a `+tlv` receiver's space 3, or the CATV TLV space 4). Decided
/// per tune so one receiver can carry both TS and 4K channels.
pub fn tune_delivers_tlv(path: &str, space: u32) -> bool {
    let Ok((_, options, _, _)) = parse_path(path) else {
        return false;
    };
    // Any in-range channel works: TLV-ness is a property of the space.
    tune_target(space, 0, &options).is_ok_and(|target| target.tlv)
}

/// Parse `px4daemon:<index>` (or `px4daemon:any`), optionally followed by
/// `@<ctrl_sock>` for a non-default socket path.
/// Whether two px4daemon paths talk to the same DriverHost (same control
/// socket). Such paths share physical receivers even when registered as
/// separate drivers, so one can be refused while the other holds a receiver.
pub fn same_daemon(a: &str, b: &str) -> bool {
    match (parse_path(a), parse_path(b)) {
        (Ok((_, _, ctrl_a, _)), Ok((_, _, ctrl_b, _))) => ctrl_a == ctrl_b,
        _ => false,
    }
}

fn parse_path(path: &str) -> Result<(i32, ReceiverOptions, String, String), io::Error> {
    let rest = path.strip_prefix(PATH_PREFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a px4daemon path: {}", path),
        )
    })?;

    let (index_part, ctrl_sock) = match rest.split_once('@') {
        Some((i, sock)) => (i, sock.to_string()),
        None => (rest, DEFAULT_CTRL_SOCK.to_string()),
    };

    let mut parts = index_part.split('+');
    let index_part = parts.next().unwrap_or_default();
    let mut options = ReceiverOptions::default();
    let mut seen = std::collections::HashSet::new();
    for suffix in parts {
        let key = suffix.split('=').next().unwrap_or_default();
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid or duplicate px4daemon suffix '{suffix}'"),
            )
        };
        if !seen.insert(key) {
            return Err(invalid());
        }
        match suffix {
            "lnb" => options.lnb = true,
            "tlv" => options.tlv = true,
            "j83b" => options.j83b = true,
            _ if suffix.starts_with("tsmf=") => {
                let value = suffix[5..].parse::<u32>().map_err(|_| invalid())?;
                if value > 14 {
                    return Err(invalid());
                }
                options.tsmf = Some(value);
            }
            _ if suffix.starts_with("qam=") => {
                let value = suffix[4..].parse::<u32>().map_err(|_| invalid())?;
                if value != 64 && value != 256 {
                    return Err(invalid());
                }
                options.qam = Some(value);
            }
            _ if suffix.starts_with("coffset=") => {
                options.coffset = Some(suffix[8..].parse::<i32>().map_err(|_| invalid())?);
            }
            _ => return Err(invalid()),
        }
    }

    // J.83B は TSMF を使わず、256QAM 固定で、ISDB-C 用の周波数補正も掛けない。
    // px4_drv は TSMF 指定つきの選局を拒否するので、選局時ではなくここで断る。
    if options.j83b
        && (options.tsmf.is_some()
            || options.tlv
            || options.qam.is_some()
            || options.coffset.is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "px4daemon suffix '+j83b' cannot be combined with +tsmf, +tlv, +qam or +coffset",
        ));
    }

    let index = if index_part.eq_ignore_ascii_case("any") || index_part.is_empty() {
        -1
    } else {
        index_part.parse::<i32>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid receiver index '{}' in {}", index_part, path),
            )
        })?
    };

    // The data socket sits next to the control socket; derive it so a custom
    // ctrl path keeps working without a second parameter.
    let data_sock = if ctrl_sock == DEFAULT_CTRL_SOCK {
        DEFAULT_DATA_SOCK.to_string()
    } else {
        ctrl_sock.replace("ctrl", "data")
    };

    Ok((index, options, ctrl_sock, data_sock))
}

// ---------------------------------------------------------------------
// Tuner
// ---------------------------------------------------------------------

/// One receiver claimed from the daemon.
pub struct Px4DaemonTuner {
    ctrl: Mutex<UnixStream>,
    /// Connected and given its `data_id` before capture starts.
    data: Mutex<Option<UnixStream>>,
    data_sock_path: String,
    /// Receiver index requested (-1 = any).
    index: i32,
    /// Per-receiver path options; CATV options apply only to space 3.
    options: ReceiverOptions,
    /// Control socket path; keys the process-wide QAM cache and the parking lot.
    ctrl_sock_path: String,
    /// Set on a parked receiver about to be closed for good, so its `Drop`
    /// closes instead of parking again.
    no_park: AtomicBool,
    /// A control/data socket operation failed; never hand this session on.
    broken: AtomicBool,
    /// Whether we currently have LNB power switched on. The daemon
    /// reference-counts it and asserts the on/off calls balance, so this must
    /// track our own state exactly — including on drop.
    lnb_on: AtomicBool,
    /// `data_id` from `OPEN`; identifies which stream to claim on the data
    /// socket.
    ///
    /// Held as a `u64` with [`NO_DATA_ID`] as the "unset" sentinel because the
    /// wire value is a full `u32`: the daemon hands out ids with the high bit
    /// set, so anything that squeezes it into an `i32` and treats negatives as
    /// "unset" breaks for half the id space (it did — intermittently, since
    /// whether an id happens to be large is pure luck).
    data_id: AtomicU64,
    /// Which `SystemType` the current `OPEN` was made for. Switching between
    /// terrestrial, satellite and CATV needs a fresh `OPEN`.
    opened_system: AtomicI32,
    /// 直近の READ_STATS(CNR) を daemon が「未対応」で断ったか。
    /// [`Self::signal_measurement_unsupported`] を参照。
    cnr_unsupported: AtomicBool,
    capturing: AtomicBool,
    current_space: AtomicI32,
    /// The other carriers of a multi-carrier group (space 5) and the combiner.
    bond: Mutex<Option<Bond>>,
}

/// Receivers and state for one combined multi-carrier tune.
struct Bond {
    /// Carriers 2..=n in `carrier_sequence` order; this tuner is carrier 1.
    helpers: Vec<Px4DaemonTuner>,
    combiner: BondCombiner,
    scratch: Vec<u8>,
    last_log: Instant,
}

/// Frequencies of carriers 2..=n per (control socket, first carrier kHz).
fn bond_groups() -> &'static Mutex<std::collections::HashMap<(String, u32), Vec<u32>>> {
    static GROUPS: std::sync::OnceLock<Mutex<std::collections::HashMap<(String, u32), Vec<u32>>>> =
        std::sync::OnceLock::new();
    GROUPS.get_or_init(Default::default)
}

// ---------------------------------------------------------------------
// Process-wide state shared by every receiver on a daemon
// ---------------------------------------------------------------------

/// Detected CATV QAM per (control socket, frequency kHz).
///
/// Auto QAM costs a disposable receiver session (release, OPEN, TUNE with a
/// 64→256 fallback, CLOSE) — seconds per carrier. Each reader creates a fresh
/// tuner, so a per-instance cache re-probed on every channel switch.
fn qam_cache() -> &'static Mutex<std::collections::HashMap<(String, u32), u32>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::HashMap<(String, u32), u32>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Wait after PURGE before draining (see `purge_ts_stream`).
const PURGE_SETTLE: Duration = Duration::from_millis(50);

/// How long a released receiver stays open for the next tuner to take over.
///
/// A channel switch drops the old reader's tuner and opens a new one. Closing
/// and re-opening the receiver costs CLOSE + OPEN (backend power cycle and
/// demodulator init, ~0.4 s on XIT-SQR100) plus the caller's EALREADY retry
/// when the new OPEN races the old CLOSE. Parking keeps the receiver, its
/// data connection and capture alive so the new tuner retunes in place —
/// the same path a BonDriver `SetChannel` takes.
const PARK_TTL: Duration = Duration::from_secs(3);
/// How long an OPEN refused with EALREADY waits for a receiver being parked.
const PARK_WAIT: Duration = Duration::from_millis(1_500);

struct ParkedReceiver {
    id: u64,
    tuner: Px4DaemonTuner,
}

fn parking_lot() -> &'static Mutex<std::collections::HashMap<String, Vec<ParkedReceiver>>> {
    static LOT: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Vec<ParkedReceiver>>>> =
        std::sync::OnceLock::new();
    LOT.get_or_init(Default::default)
}

/// Take the most recently parked receiver on `ctrl_sock` that `index` may use.
fn take_parked(ctrl_sock: &str, index: i32) -> Option<Px4DaemonTuner> {
    let mut lot = parking_lot().lock().unwrap_or_else(|e| e.into_inner());
    let entries = lot.get_mut(ctrl_sock)?;
    let pos = entries
        .iter()
        .rposition(|p| index < 0 || p.tuner.index == index)?;
    Some(entries.remove(pos).tuner)
}

fn park(tuner: Px4DaemonTuner) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let key = tuner.ctrl_sock_path.clone();
    debug!(
        "[px4daemon] parked receiver (system=0x{:02x}) for {:?}",
        tuner.opened_system.load(Ordering::Acquire),
        PARK_TTL
    );
    parking_lot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.clone())
        .or_default()
        .push(ParkedReceiver { id, tuner });
    let reaper_key = key.clone();
    let spawned = std::thread::Builder::new()
        .name("px4-park-reaper".into())
        .spawn(move || {
            let key = reaper_key;
            std::thread::sleep(PARK_TTL);
            let expired = {
                let mut lot = parking_lot().lock().unwrap_or_else(|e| e.into_inner());
                lot.get_mut(&key).and_then(|entries| {
                    let pos = entries.iter().position(|p| p.id == id)?;
                    Some(entries.remove(pos).tuner)
                })
            };
            if let Some(tuner) = expired {
                tuner.no_park.store(true, Ordering::Release);
                debug!(
                    "[px4daemon] closing parked receiver after {:?} idle",
                    PARK_TTL
                );
                drop(tuner);
            }
        });
    if let Err(e) = spawned {
        // Without a reaper the receiver would stay open forever; close now.
        warn!("[px4daemon] cannot spawn park reaper ({e}); closing receiver");
        if let Some(tuner) = take_parked_by_id(&key, id) {
            tuner.no_park.store(true, Ordering::Release);
        }
    }
}

fn take_parked_by_id(ctrl_sock: &str, id: u64) -> Option<Px4DaemonTuner> {
    let mut lot = parking_lot().lock().unwrap_or_else(|e| e.into_inner());
    let entries = lot.get_mut(ctrl_sock)?;
    let pos = entries.iter().position(|p| p.id == id)?;
    Some(entries.remove(pos).tuner)
}

impl Px4DaemonTuner {
    pub fn new(path: &str) -> Result<Self, io::Error> {
        let (index, options, ctrl_sock, data_sock) = parse_path(path)?;

        let ctrl = UnixStream::connect(&ctrl_sock).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot connect to the px4_drv daemon at {} ({}). \
                     Start DriverHost_PX4 before using {}",
                    ctrl_sock, e, path
                ),
            )
        })?;
        ctrl.set_read_timeout(Some(Duration::from_secs(35)))?;
        ctrl.set_write_timeout(Some(Duration::from_secs(5)))?;

        let tuner = Self::with_ctrl(ctrl, ctrl_sock.clone(), data_sock, index, options);

        // Handshake, so a mismatched daemon fails here rather than midway
        // through a channel change.
        let mut buf = vec![0u8; VERSION_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::GET_VERSION));
        tuner.transact(&mut buf)?;
        let driver_version = get_u32(&buf, 8);
        let cmd_version = get_u32(&buf, 12);
        info!(
            "[px4daemon] connected to {} (driver 0x{:08x}, protocol 0x{:08x}), receiver index {}",
            ctrl_sock, driver_version, cmd_version, index
        );

        // A receiver released moments ago (channel switch) is still open;
        // take it over instead of closing and re-opening the hardware.
        if let Some(parked) = take_parked(&tuner.ctrl_sock_path, tuner.index) {
            tuner.adopt(parked);
        }

        Ok(tuner)
    }

    fn with_ctrl(
        ctrl: UnixStream,
        ctrl_sock_path: String,
        data_sock_path: String,
        index: i32,
        options: ReceiverOptions,
    ) -> Self {
        Self {
            ctrl: Mutex::new(ctrl),
            data: Mutex::new(None),
            data_sock_path,
            index,
            options,
            ctrl_sock_path,
            no_park: AtomicBool::new(false),
            broken: AtomicBool::new(false),
            lnb_on: AtomicBool::new(false),
            data_id: AtomicU64::new(NO_DATA_ID),
            opened_system: AtomicI32::new(0),
            cnr_unsupported: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            current_space: AtomicI32::new(0),
            bond: Mutex::new(None),
        }
    }

    /// Swap every piece of receiver state with `other`.
    ///
    /// The receiver belongs to the control *connection* on the daemon side,
    /// so moving the connection moves the receiver. Afterwards `other` holds
    /// this tuner's unopened connection and closes nothing when dropped.
    fn swap_state(&self, other: &Self) {
        {
            let mut a = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
            let mut b = other.ctrl.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::swap(&mut *a, &mut *b);
        }
        {
            let mut a = self.data.lock().unwrap_or_else(|e| e.into_inner());
            let mut b = other.data.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::swap(&mut *a, &mut *b);
        }
        let swap_u64 = |a: &AtomicU64, b: &AtomicU64| {
            a.store(
                b.swap(a.load(Ordering::Acquire), Ordering::AcqRel),
                Ordering::Release,
            )
        };
        let swap_i32 = |a: &AtomicI32, b: &AtomicI32| {
            a.store(
                b.swap(a.load(Ordering::Acquire), Ordering::AcqRel),
                Ordering::Release,
            )
        };
        let swap_bool = |a: &AtomicBool, b: &AtomicBool| {
            a.store(
                b.swap(a.load(Ordering::Acquire), Ordering::AcqRel),
                Ordering::Release,
            )
        };
        swap_u64(&self.data_id, &other.data_id);
        swap_i32(&self.opened_system, &other.opened_system);
        swap_i32(&self.current_space, &other.current_space);
        swap_bool(&self.capturing, &other.capturing);
        swap_bool(&self.lnb_on, &other.lnb_on);
        swap_bool(&self.cnr_unsupported, &other.cnr_unsupported);
        swap_bool(&self.broken, &other.broken);
    }

    /// Take over a parked receiver if its control connection still answers.
    /// Returns whether it was adopted; an unusable one is closed.
    fn adopt(&self, parked: Px4DaemonTuner) -> bool {
        let mut probe = vec![0u8; CHECK_LOCK_CMD_SIZE];
        probe[..HEADER_SIZE].copy_from_slice(&header(cmd::CHECK_LOCK));
        if let Err(e) = parked.transact(&mut probe) {
            debug!("[px4daemon] parked receiver is gone ({e}); opening a new one");
            parked.no_park.store(true, Ordering::Release);
            return false;
        }
        self.swap_state(&parked);
        // `parked` now holds our fresh, unopened connection.
        parked.no_park.store(true, Ordering::Release);
        info!(
            "[px4daemon] reusing open receiver (system=0x{:02x}); skipping CLOSE/OPEN",
            self.opened_system.load(Ordering::Acquire)
        );
        true
    }

    /// Whether this session is in a state the next tuner can retune in place.
    fn parkable(&self) -> bool {
        !self.no_park.load(Ordering::Acquire)
            && !self.broken.load(Ordering::Acquire)
            && self.opened_system.load(Ordering::Acquire) != 0
            && self.capturing.load(Ordering::Acquire)
            && !self.lnb_on.load(Ordering::Acquire)
            && self.data.lock().map(|g| g.is_some()).unwrap_or(false)
            && self.bond.lock().map(|g| g.is_none()).unwrap_or(false)
    }

    /// OPEN, and if another tuner of ours still holds the receiver because it
    /// is being released right now, wait for it to be parked and take it.
    ///
    /// `reuse = false` (QAM probe) closes the parked receiver and opens a
    /// fresh one, since the probe needs a session without a data connection.
    fn open_or_adopt(&self, sys: u32, reuse: bool) -> Result<(), io::Error> {
        let err = match self.open_receiver(sys) {
            Err(e) if e.raw_os_error() == Some(libc::EALREADY) => e,
            other => return other,
        };
        let deadline = std::time::Instant::now() + PARK_WAIT;
        loop {
            if let Some(parked) = take_parked(&self.ctrl_sock_path, self.index) {
                if reuse && self.adopt(parked) {
                    if self.opened_system.load(Ordering::Acquire) as u32 != sys {
                        self.release_receiver()?;
                        self.open_receiver(sys)?;
                    }
                    return Ok(());
                }
                // Not reusable (or dropped above as dead): closed by now.
                return self.open_receiver(sys);
            }
            if std::time::Instant::now() >= deadline {
                return Err(err);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Send a command and read the same-sized response in place.
    ///
    /// The daemon mutates and echoes the command at the same size. Params
    /// commands extend the base struct by 8 bytes per extra parameter.
    fn transact(&self, buf: &mut [u8]) -> Result<(), io::Error> {
        let mut ctrl = self
            .ctrl
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "px4daemon ctrl lock poisoned"))?;
        let result = (|| {
            ctrl.write_all(buf)?;
            ctrl.flush()?;
            ctrl.read_exact(buf)
        })();
        if result.is_err() {
            self.broken.store(true, Ordering::Release);
        }
        result
    }

    /// `transact`, and fail unless the daemon reported success.
    fn transact_checked(&self, buf: &mut [u8], what: &str) -> Result<(), io::Error> {
        self.transact(buf)?;
        let st = get_u32(buf, 4);
        if st != status::SUCCEEDED {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("px4daemon {} failed (status {})", what, st),
            ));
        }
        Ok(())
    }

    fn open_receiver(&self, sys: u32) -> Result<(), io::Error> {
        let mut buf = vec![0u8; OPEN_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::OPEN));
        put_u32(&mut buf, HEADER_SIZE + RI_SYSTEMS_OFF, sys);
        put_u32(&mut buf, HEADER_SIZE + RI_INDEX_OFF, self.index as u32);

        self.transact(&mut buf)?;
        if get_u32(&buf, 4) != status::SUCCEEDED {
            // The daemon reports "no receiver available" this way, which is
            // the same condition a busy character device would report as
            // EALREADY on Linux — map it accordingly so the pool's
            // single-open handling (channel_resolve's EALREADY retry) still
            // applies.
            return Err(io::Error::from_raw_os_error(libc::EALREADY));
        }

        let data_id = get_u32(&buf, HEADER_SIZE + RI_DATA_ID_OFF);
        let reported_index = get_i32(&buf, HEADER_SIZE + RI_INDEX_OFF);
        self.data_id.store(data_id as u64, Ordering::Release);
        self.opened_system.store(sys as i32, Ordering::Release);
        info!(
            "[px4daemon] OPEN ok: requested index={} → receiver index={} system=0x{:02x} data_id={}",
            self.index, reported_index, sys, data_id
        );
        Ok(())
    }

    fn close_receiver(&self) -> Result<(), io::Error> {
        let mut buf = vec![0u8; CLOSE_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::CLOSE));
        self.transact_checked(&mut buf, "CLOSE")?;
        self.opened_system.store(0, Ordering::Release);
        self.data_id.store(NO_DATA_ID, Ordering::Release);
        self.capturing.store(false, Ordering::Release);
        Ok(())
    }

    fn set_capture(&self, on: bool) -> Result<(), io::Error> {
        let mut buf = vec![0u8; CAPTURE_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::SET_CAPTURE));
        buf[8] = on as u8;
        self.transact_checked(
            &mut buf,
            if on {
                "SET_CAPTURE(true)"
            } else {
                "SET_CAPTURE(false)"
            },
        )?;
        self.capturing.store(on, Ordering::Release);
        Ok(())
    }

    /// Switch LNB power to match `sys`: 15 V for satellite, off otherwise.
    /// Mirrors px4_drv's own BonDriver (`bon_driver.cpp`).
    fn apply_lnb_power(&self, sys: u32) -> Result<(), io::Error> {
        if !self.options.lnb {
            return Ok(());
        }
        let want = sys == system::ISDB_S;
        if want == self.lnb_on.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut buf = vec![0u8; LNB_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::SET_LNB_VOLTAGE));
        put_u32(&mut buf, 8, if want { 15 } else { 0 });
        self.transact_checked(&mut buf, "SET_LNB_VOLTAGE")?;
        self.lnb_on.store(want, Ordering::Release);
        info!(
            "[px4daemon] LNB power {}",
            if want { "on (15V)" } else { "off" }
        );
        Ok(())
    }

    fn set_params(
        &self,
        sys: u32,
        freq_khz: u32,
        modulation: u32,
        tlv: bool,
        probe: bool,
        raw: bool,
    ) -> Result<(), io::Error> {
        let mut options = self.options.clone();
        options.tlv = tlv;
        let params = tuning_params(sys, &options, modulation, probe, raw);
        let mut buf = params_command(cmd::SET_PARAMS, sys, freq_khz, &params);
        self.transact_checked(&mut buf, "SET_PARAMS")
    }

    fn probe_catv_modulation(&self, freq_khz: u32) -> Result<u32, io::Error> {
        // Auto QAM needs USB input packets, hence capture before TUNE. Such
        // a tune leaks old data even after PURGE. Use a disposable receiver
        // with no data connection, then CLOSE and OPEN the production stream
        // with fixed QAM. No old data thread can later StopRequest() the new
        // stream. Input counting precedes TSMF selection, so probing needs
        // neither TSMF nor TLV. Unlike px4rec, no probe data socket is needed:
        // StreamBuffer::Write counts input before ring writes, and
        // PxMltReceiver::SetCapture starts USB capture independently of readers.
        // This reader-free probe still needs verification on real hardware.
        // Never toggle capture on a live data connection.
        self.open_or_adopt(system::ISDB_C, false)?;
        let result = (|| {
            self.set_capture(true)?;
            self.set_params(system::ISDB_C, freq_khz, 0, false, true, false)?;
            self.tune()?;
            let mut buf = params_command(cmd::GET_PARAMS, 0, 0, &[(3, 0)]);
            self.transact_checked(&mut buf, "GET_PARAMS")?;
            let modulation = get_u32(&buf, 24);
            if modulation != 64 && modulation != 256 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid detected QAM: {modulation}"),
                ));
            }
            Ok(modulation)
        })();
        let stopped = if self.capturing.load(Ordering::Acquire) {
            self.set_capture(false)
        } else {
            Ok(())
        };
        let closed = self.close_receiver();
        closed?;
        let modulation = result?;
        stopped?;
        Ok(modulation)
    }

    fn tune(&self) -> Result<(), io::Error> {
        let mut buf = vec![0u8; TUNE_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::TUNE));
        put_u32(&mut buf, 8, TUNE_TIMEOUT_MS);
        self.transact(&mut buf)?;
        if get_u32(&buf, 4) != status::SUCCEEDED {
            // No lock: the signal is absent or too weak for this channel.
            // `AddrNotAvailable` is what the rest of the proxy already treats
            // as "channel unavailable" rather than a driver malfunction.
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "px4daemon TUNE failed (no lock)",
            ));
        }
        Ok(())
    }

    /// Connect the data socket and claim this receiver's stream.
    fn attach_data_socket(&self) -> Result<(), io::Error> {
        let data_id = self.data_id.load(Ordering::Acquire);
        if data_id == NO_DATA_ID {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "px4daemon: no data_id; the receiver is not open",
            ));
        }

        let sock = UnixStream::connect(&self.data_sock_path)?;
        let mut cmd_buf = [0u8; DATA_CMD_SIZE];
        put_u32(&mut cmd_buf, 0, data_cmd::SET_DATA_ID);
        put_u32(&mut cmd_buf, 4, data_id as u32);
        (&sock).write_all(&cmd_buf)?;

        // Non-blocking from here on: the reader loop polls with
        // `wait_ts_stream` and expects `WouldBlock` rather than a stall when
        // nothing has arrived yet.
        sock.set_nonblocking(true)?;

        let mut guard = self
            .data
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "px4daemon data lock poisoned"))?;
        *guard = Some(sock);
        Ok(())
    }

    fn drop_data_socket(&self) {
        if let Ok(mut guard) = self.data.lock() {
            *guard = None;
        }
    }

    fn release_receiver(&self) -> Result<(), io::Error> {
        if self.opened_system.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        self.apply_lnb_power(system::ISDB_T)?;
        let stopped = if self.capturing.load(Ordering::Acquire) {
            self.set_capture(false)
        } else {
            Ok(())
        };
        self.drop_data_socket();
        self.close_receiver()?;
        stopped
    }

    // -----------------------------------------------------------------
    // BonDriverTuner-shaped API
    // -----------------------------------------------------------------

    pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
        let target = tune_target(space, channel, &self.options)?;
        // Any retune ends a previous combined tune and frees its receivers.
        let previous_bond = self.bond.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(previous_bond);
        let sys = target.system;
        let freq_khz = target.freq_khz;
        let j83b = space == 3 && self.options.j83b;
        // J.83B は daemon が 256QAM へ固定する。64/256 の自動判定 (使い捨ての
        // 受信機セッション) は意味がなく、77 波ぶんの時間だけ掛かる。
        let mut modulation = target.modulation;
        if sys == system::ISDB_C && modulation == 0 {
            let key = (self.ctrl_sock_path.clone(), freq_khz);
            modulation = qam_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .copied()
                .unwrap_or(0);
            if modulation == 0 {
                // Release the current receiver before the disposable probe.
                // Cached carriers subsequently retain the live retune path.
                self.release_receiver()?;
                modulation = self.probe_catv_modulation(freq_khz)?;
                qam_cache()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, modulation);
            }
        }

        self.tune_prepared(sys, freq_khz, modulation, target.tlv, target.bonded, j83b)?;
        if target.bonded {
            self.setup_bond(freq_khz, modulation)?;
        }

        self.current_space.store(space as i32, Ordering::Release);
        info!(
            "[px4daemon] tuned: space={} channel={} → system=0x{:02x} {} kHz{}",
            space,
            channel,
            sys,
            freq_khz,
            if target.tlv { " (TLV)" } else { "" }
        );
        Ok(())
    }

    /// Tune with QAM already resolved: (re)OPEN when the system changes,
    /// attach the data socket, SET_PARAMS + TUNE, purge, start capture.
    fn tune_prepared(
        &self,
        sys: u32,
        freq_khz: u32,
        modulation: u32,
        tlv: bool,
        raw: bool,
        j83b: bool,
    ) -> Result<(), io::Error> {
        // Deliberately *not* stopping capture here when the receiver stays
        // open. Capture toggling is what makes re-tuning fragile on this
        // daemon:
        //
        // - `SET_CAPTURE(false)` calls `StreamBuffer::Stop()`, which ends the
        //   data connection's `HandleRead` and retires its streaming thread
        //   for good. That thread is only started by `SET_DATA_ID`, and a
        //   second `SET_DATA_ID` on the same connection is ignored
        //   (`if (receiver) break;` in `stream_server.cpp`), so re-enabling
        //   capture leaves the socket permanently silent.
        // - Reconnecting the data socket instead does not help either: when
        //   the *old* connection finishes tearing down it calls
        //   `StopRequest()` on the receiver's (shared) stream buffer, which
        //   kills the stream the new connection just started.
        //
        // Retuning in place — SET_PARAMS + TUNE while capture stays on, then
        // a purge to drop the previous channel's bytes — keeps one stream
        // thread alive for the whole life of the receiver. This matches what
        // a BonDriver `SetChannel` does anyway.

        // A receiver is opened for one system at a time; crossing between
        // terrestrial, satellite and CATV means re-opening it, and that *does*
        // invalidate the data socket (a new `OPEN` yields a new `data_id`).
        let opened = self.opened_system.load(Ordering::Acquire) as u32;
        if opened != sys {
            self.release_receiver()?;
            self.open_or_adopt(sys, true)?;
        }

        self.apply_lnb_power(sys)?;
        let needs_data = self.data.lock().map(|g| g.is_none()).unwrap_or(true);
        if needs_data {
            // Attach before capture: doing this afterwards loses the first
            // daemon-buffer-sized chunk during simultaneous tuner startup.
            self.attach_data_socket()?;
        }
        self.set_params(sys, freq_khz, modulation, tlv && !raw, false, raw)?;
        if let Err(error) = self.tune() {
            if j83b && error.kind() == io::ErrorKind::AddrNotAvailable {
                // ISDB-C tuning parameters intentionally retain symbol rate 5274;
                // the daemon must override it globally for J.83B operation.
                return Err(io::Error::new(
                    error.kind(),
                    "px4daemon J.83B TUNE failed (no lock); daemon ExperimentalJ83B=true is required and applies to the entire device",
                ));
            }
            return Err(error);
        }
        self.purge_data();
        if !self.capturing.load(Ordering::Acquire) {
            self.set_capture(true)?;
        }

        Ok(())
    }

    /// Read raw TS until a TSMF header arrives (`None` after `wait`).
    fn read_tsmf_header(&self, wait: Duration) -> Result<Option<ExtHeader>, io::Error> {
        let deadline = Instant::now() + wait;
        let mut acc = Vec::new();
        let mut buf = vec![0u8; 65536];
        while Instant::now() < deadline {
            if !self.wait_data(20) {
                continue;
            }
            match self.read_data(&mut buf) {
                Ok((n, _)) => acc.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            }
            // About 0.1 s of a 256QAM carrier. The first bytes after a retune
            // can still be the previous frequency's (PURGE races data in
            // flight), so judge by the last header, not the first.
            if acc.len() >= HEADER_SAMPLE_BYTES {
                return Ok(tsmf_bond::find_last_header(&acc));
            }
        }
        Ok(tsmf_bond::find_last_header(&acc))
    }

    /// A second receiver on the same daemon for another carrier of a group.
    fn open_helper(&self, carriers: u8) -> Result<Px4DaemonTuner, io::Error> {
        let ctrl = UnixStream::connect(&self.ctrl_sock_path)?;
        ctrl.set_read_timeout(Some(Duration::from_secs(35)))?;
        ctrl.set_write_timeout(Some(Duration::from_secs(5)))?;
        let helper = Self::with_ctrl(
            ctrl,
            self.ctrl_sock_path.clone(),
            self.data_sock_path.clone(),
            -1,
            self.options.clone(),
        );
        // Helpers live and die with the combined tune; never hand one on.
        helper.no_park.store(true, Ordering::Release);
        match helper.open_or_adopt(system::ISDB_C, true) {
            Err(e) if e.raw_os_error() == Some(libc::EALREADY) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "px4daemon: a {carriers}-carrier group needs {carriers} free ISDB-C receivers on this DriverHost"
                ),
            )),
            other => other.map(|()| helper),
        }
    }

    /// Tune a helper to `freq_khz` and return the carrier's TSMF header.
    fn tune_member(&self, freq_khz: u32, modulation: u32) -> Result<Option<ExtHeader>, io::Error> {
        self.tune_prepared(system::ISDB_C, freq_khz, modulation, false, true, false)?;
        self.read_tsmf_header(TSMF_HEADER_WAIT)
    }

    /// Find carriers 2..=n of the group whose first carrier this tuner is
    /// tuned to, on helpers, and start combining them.
    fn setup_bond(&self, head_khz: u32, modulation: u32) -> Result<(), io::Error> {
        let started = Instant::now();
        let head = self.read_tsmf_header(TSMF_HEADER_WAIT)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "px4daemon: no TSMF header on this carrier",
            )
        })?;
        if !head.is_multi_carrier() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "px4daemon: not a multi-carrier (extended TSMF) carrier",
            ));
        }
        if head.carrier_sequence != 1 {
            // Each group is registered once, on its first carrier.
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!(
                    "px4daemon: carrier {}/{} of group {}; the group is tuned on its first carrier",
                    head.carrier_sequence, head.number_of_carriers, head.group_id
                ),
            ));
        }
        let carriers = head.number_of_carriers;
        let members = carriers as usize - 1;
        let key = (self.ctrl_sock_path.clone(), head_khz);
        let cached = bond_groups()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();

        let matches = |h: &Option<ExtHeader>, seq: usize| {
            h.as_ref().is_some_and(|h| {
                h.is_multi_carrier()
                    && h.group_id == head.group_id
                    && h.number_of_carriers == carriers
                    && h.carrier_sequence as usize == seq
            })
        };

        let mut helpers: Vec<Option<Px4DaemonTuner>> = (0..members).map(|_| None).collect();
        let mut freqs = vec![0u32; members];
        if let Some(known) = cached.filter(|f| f.len() == members) {
            for (i, &freq) in known.iter().enumerate() {
                let helper = self.open_helper(carriers)?;
                let header = helper.tune_member(freq, modulation).or_else(|e| {
                    if e.kind() == io::ErrorKind::AddrNotAvailable {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                })?;
                if !matches(&header, i + 2) {
                    // The plant changed; forget and search again below.
                    bond_groups()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&key);
                    helpers.iter_mut().for_each(|h| *h = None);
                    drop(helper);
                    break;
                }
                freqs[i] = freq;
                helpers[i] = Some(helper);
            }
        }
        if helpers.iter().any(Option::is_none) {
            helpers.iter_mut().for_each(|h| *h = None);
            let mut candidates: Vec<u32> = (0..CATV_CHANNELS)
                .filter_map(|ch| catv_plan_frequency(ch, self.options.coffset).ok())
                .filter(|&f| f != head_khz && f.abs_diff(head_khz) <= BOND_SEARCH_RANGE_KHZ)
                .collect();
            // Nearest first; above before below at the same distance.
            candidates.sort_by_key(|&f| (f.abs_diff(head_khz), f < head_khz));
            let mut probe: Option<Px4DaemonTuner> = None;
            for freq in candidates {
                if helpers.iter().all(Option::is_some) {
                    break;
                }
                let helper = match probe.take() {
                    Some(h) => h,
                    None => self.open_helper(carriers)?,
                };
                let header = match helper.tune_member(freq, modulation) {
                    Ok(h) => h,
                    Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => None,
                    Err(e) => return Err(e),
                };
                let seq = header.as_ref().map_or(0, |h| h.carrier_sequence as usize);
                if seq >= 2 && matches(&header, seq) && helpers[seq - 2].is_none() {
                    info!(
                        "[px4daemon] group {} carrier {}/{} at {} kHz",
                        head.group_id, seq, carriers, freq
                    );
                    freqs[seq - 2] = freq;
                    helpers[seq - 2] = Some(helper);
                } else {
                    probe = Some(helper);
                }
            }
            if helpers.iter().any(Option::is_none) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!(
                        "px4daemon: group {} has {} carriers but only {} were found within {} MHz of {} kHz",
                        head.group_id,
                        carriers,
                        1 + helpers.iter().filter(|h| h.is_some()).count(),
                        BOND_SEARCH_RANGE_KHZ / 1000,
                        head_khz
                    ),
                ));
            }
            bond_groups()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, freqs.clone());
        }

        let helpers: Vec<Px4DaemonTuner> = helpers.into_iter().map(Option::unwrap).collect();
        // Discovery took a while; start every carrier from fresh data.
        self.purge_data();
        for h in &helpers {
            h.purge_data();
        }
        info!(
            "[px4daemon] combining group {} ({} carriers: {} kHz + {:?} kHz) after {:?}",
            head.group_id,
            carriers,
            head_khz,
            freqs,
            started.elapsed()
        );
        *self.bond.lock().unwrap_or_else(|e| e.into_inner()) = Some(Bond {
            combiner: BondCombiner::new(helpers.len() + 1),
            helpers,
            scratch: vec![0u8; 256 * 1024],
            last_log: Instant::now(),
        });
        Ok(())
    }

    /// CNR in dB, as reported by the demodulator.
    pub fn get_signal_level(&self) -> f32 {
        let mut buf = vec![0u8; STATS_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::READ_STATS));
        put_u32(&mut buf, 8, 1); // stat_set.num
        put_u32(&mut buf, 12, STAT_CNR); // stat_set.data[0].type
        if self.transact(&mut buf).is_err() {
            return 0.0;
        }
        // 通信は成立したのに daemon が統計を断った = その受信方式では C/N を
        // 出せない。0.0 を「信号なし」と区別できるよう覚えておく。
        let refused = get_u32(&buf, 4) != status::SUCCEEDED;
        self.cnr_unsupported.store(refused, Ordering::Release);
        if refused {
            return 0.0;
        }
        // px4_drv reports CNR in millibels (×1000 dB), same as the Linux
        // driver's PTX_GET_CNR.
        get_i32(&buf, 16) as f32 / 1000.0
    }

    /// ロックしているのに C/N を取得できない状態か。
    ///
    /// px4_drv は実験的な J.83 Annex B モード (`ExperimentalJ83B=true`) で
    /// C/N の換算式を持たず、READ_STATS(CNR) を意図的に失敗させる。
    /// [`Self::get_signal_level`] はそのとき 0.0 を返すので、呼び出し側が
    /// 「信号なし」と取り違えるとロック済みのチャンネルを全部捨ててしまう
    /// (チャンネルスキャンが 1 局も登録できない)。直近の C/N 取得が断られ、
    /// かつ復調器がロックを報告しているときだけ true。
    ///
    /// J.83B の信号がある環境では未検証 (px4_drv 側の実装からの判断)。
    pub fn signal_measurement_unsupported(&self) -> bool {
        self.cnr_unsupported.load(Ordering::Acquire) && self.check_lock()
    }

    /// Whether the demodulator currently reports lock.
    pub fn check_lock(&self) -> bool {
        let mut buf = vec![0u8; CHECK_LOCK_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::CHECK_LOCK));
        if self.transact(&mut buf).is_err() || get_u32(&buf, 4) != status::SUCCEEDED {
            return false;
        }
        buf[8] != 0
    }

    fn wait_data(&self, timeout_ms: u32) -> bool {
        use nix::poll::{poll, PollFd, PollFlags};

        let guard = match self.data.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(sock) = guard.as_ref() else {
            return false;
        };

        let fd = sock.as_raw_fd();
        // SAFETY: fd stays valid while `guard` holds the socket open.
        let mut fds = [PollFd::new(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            PollFlags::POLLIN,
        )];
        match poll(&mut fds, timeout_ms.min(u16::MAX as u32) as u16) {
            Ok(n) if n > 0 => fds[0]
                .revents()
                .map(|r| r.contains(PollFlags::POLLIN))
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Read raw TS or TLV without packet alignment or truncation.
    /// Returns `(bytes_read, remaining)`; the daemon does not report
    /// a backlog, so `remaining` is always 0.
    fn read_data(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
        let mut guard = self
            .data
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "px4daemon data lock poisoned"))?;
        let Some(sock) = guard.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "px4daemon: data socket not attached",
            ));
        };
        match sock.read(buf) {
            Ok(0) => {
                self.broken.store(true, Ordering::Release);
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "px4daemon: data socket closed by the daemon",
                ))
            }
            Ok(n) => Ok((n, 0)),
            Err(e) => {
                if !matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    self.broken.store(true, Ordering::Release);
                }
                Err(e)
            }
        }
    }

    fn purge_data(&self) {
        // Ask the daemon to drop what it has buffered for us...
        if let Ok(guard) = self.data.lock() {
            if let Some(sock) = guard.as_ref() {
                let mut cmd_buf = [0u8; DATA_CMD_SIZE];
                put_u32(&mut cmd_buf, 0, data_cmd::PURGE);
                if let Err(e) = (&*sock).write_all(&cmd_buf) {
                    debug!("[px4daemon] PURGE write failed: {}", e);
                }
            }
        }

        // ...and drain whatever is already in flight towards us.
        //
        // PURGE has no reply and is handled by the connection's command
        // thread, while a separate writer thread may already hold a chunk
        // taken from the buffer before the purge. Draining at once leaves
        // that tail to arrive *after* we start reading, followed by the
        // post-purge data: a gap inside the delivered stream (continuity
        // errors on every PID ~0.3 s into each tune, measured). Give the
        // daemon a moment to act on the purge and flush the tail first.
        std::thread::sleep(PURGE_SETTLE);
        let mut discard = vec![0u8; 65536];
        for _ in 0..64 {
            if !self.wait_data(0) {
                break;
            }
            match self.read_data(&mut discard) {
                Ok((n, _)) if n > 0 => continue,
                _ => break,
            }
        }
    }

    fn data_fd(&self) -> Option<std::os::unix::io::RawFd> {
        self.data.lock().ok()?.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        let fds: Vec<std::os::unix::io::RawFd> = {
            let guard = self.bond.lock().unwrap_or_else(|e| e.into_inner());
            let Some(bond) = guard.as_ref() else {
                drop(guard);
                return self.wait_data(timeout_ms);
            };
            if bond.combiner.pending() > 0 {
                return true;
            }
            std::iter::once(self.data_fd())
                .chain(bond.helpers.iter().map(|h| h.data_fd()))
                .flatten()
                .collect()
        };
        let mut pfds: Vec<libc::pollfd> = fds
            .iter()
            .map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: the descriptors belong to sockets owned by this tuner and its
        // helpers; a socket closed meanwhile only makes poll report POLLNVAL.
        let n = unsafe {
            libc::poll(
                pfds.as_mut_ptr(),
                pfds.len() as libc::nfds_t,
                timeout_ms.min(i32::MAX as u32) as i32,
            )
        };
        n > 0 && pfds.iter().any(|p| p.revents & libc::POLLIN != 0)
    }

    /// Read raw TS or TLV without packet alignment or truncation. A combined
    /// group (space 5) returns whole TLV packets once the carriers are aligned.
    pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
        let mut guard = self.bond.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bond) = guard.as_mut() else {
            drop(guard);
            return self.read_data(buf);
        };
        let Bond {
            helpers,
            combiner,
            scratch,
            last_log,
        } = bond;
        let now = Instant::now();
        for carrier in 0..=helpers.len() {
            let source = if carrier == 0 {
                self
            } else {
                &helpers[carrier - 1]
            };
            for _ in 0..16 {
                match source.read_data(scratch) {
                    Ok((n, _)) => combiner.push(carrier, &scratch[..n], now),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        break
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        combiner.pump();
        if last_log.elapsed() >= Duration::from_secs(10) {
            let stats = combiner.stats();
            info!(
                "[px4daemon] multi-carrier: synced={} resyncs={} tlv ok={} broken={}",
                stats.synced, stats.resyncs, stats.tlv_good, stats.tlv_bad
            );
            *last_log = Instant::now();
        }
        match combiner.read(buf) {
            0 => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            n => Ok((n, combiner.pending())),
        }
    }

    pub fn purge_ts_stream(&self) {
        let mut guard = self.bond.lock().unwrap_or_else(|e| e.into_inner());
        self.purge_data();
        if let Some(bond) = guard.as_mut() {
            for h in &bond.helpers {
                h.purge_data();
            }
            bond.combiner.clear();
        }
    }

    pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
        // Use the plan's range validation so enumeration and SetChannel agree.
        // With +tlv, spaces 0-2 keep their names (so space 3 keeps its index)
        // but expose no channels.
        if !(self.options.tlv && space < 3) {
            tune_target(space, 0, &self.options).ok()?;
        }
        match space {
            0 => Some("GR".to_string()),
            1 => Some("BS".to_string()),
            2 => Some("CS".to_string()),
            3 if self.options.j83b => Some("J.83B".to_string()),
            3 => Some("CATV".to_string()),
            CATV_TLV_SPACE => Some("CATV-TLV".to_string()),
            CATV_BOND_SPACE => Some("CATV-8K".to_string()),
            _ => None,
        }
    }

    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
        channel_name(space, channel, &self.options)
    }

    pub fn version(&self) -> u8 {
        2
    }
}

impl Drop for Px4DaemonTuner {
    fn drop(&mut self) {
        // Deliberately *not* stopping capture here when the receiver stays
        // open. Capture toggling is what makes re-tuning fragile on this
        // daemon:
        //
        // - `SET_CAPTURE(false)` calls `StreamBuffer::Stop()`, which ends the
        //   data connection's `HandleRead` and retires its streaming thread
        //   for good. That thread is only started by `SET_DATA_ID`, and a
        //   second `SET_DATA_ID` on the same connection is ignored
        //   (`if (receiver) break;` in `stream_server.cpp`), so re-enabling
        //   capture leaves the socket permanently silent.
        // - Reconnecting the data socket instead does not help either: when
        //   the *old* connection finishes tearing down it calls
        //   `StopRequest()` on the receiver's (shared) stream buffer, which
        //   kills the stream the new connection just started.
        //
        // Retuning in place — SET_PARAMS + TUNE while capture stays on, then
        // a purge to drop the previous channel's bytes — keeps one stream
        // thread alive for the whole life of the receiver. This matches what
        // a BonDriver `SetChannel` does anyway.
        if self.parkable() {
            // Hand the open receiver to the next tuner instead of closing it
            // (see `PARK_TTL`). A socket pair stands in for the connection the
            // shell never uses.
            if let Ok((dummy, _peer)) = UnixStream::pair() {
                let shell = Self::with_ctrl(
                    dummy,
                    self.ctrl_sock_path.clone(),
                    self.data_sock_path.clone(),
                    self.index,
                    self.options.clone(),
                );
                shell.swap_state(self);
                park(shell);
                return;
            }
        }
        self.drop_data_socket();
        if self.lnb_on.load(Ordering::Acquire) {
            // The daemon reference-counts LNB power and asserts the calls
            // balance, so this must not be skipped.
            let _ = self.apply_lnb_power(system::ISDB_T);
        }
        if self.opened_system.load(Ordering::Acquire) != 0 {
            if let Err(e) = self.close_receiver() {
                warn!("[px4daemon] CLOSE failed: {e}");
            }
        }
        debug!("[px4daemon] receiver index {} released", self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_and_socket_overrides() {
        assert_eq!(
            parse_path("px4daemon:3").unwrap(),
            (
                3,
                ReceiverOptions::default(),
                DEFAULT_CTRL_SOCK.to_string(),
                DEFAULT_DATA_SOCK.to_string()
            )
        );
        assert_eq!(parse_path("px4daemon:any").unwrap().0, -1);
        assert_eq!(parse_path("px4daemon:").unwrap().0, -1);

        // LNB power is opt-in per receiver.
        let (idx, lnb, _, _) = parse_path("px4daemon:2+lnb").unwrap();
        assert_eq!((idx, lnb.lnb), (2, true));
        assert!(!parse_path("px4daemon:2").unwrap().1.lnb);

        let (idx, _, ctrl, data) = parse_path("px4daemon:1@/run/px4_ctrl.sock").unwrap();
        assert_eq!(idx, 1);
        assert_eq!(ctrl, "/run/px4_ctrl.sock");
        assert_eq!(
            data, "/run/px4_data.sock",
            "the data socket is derived from the control socket so one override covers both"
        );
    }

    #[test]
    fn rejects_paths_for_other_backends() {
        assert!(parse_path("/dev/px4video0").is_err());
        assert!(parse_path("px4daemon:x").is_err());
    }

    #[test]
    fn j83b_suffix_is_independent_and_rejects_incompatible_options() {
        assert!(parse_path("px4daemon:3+j83b").unwrap().1.j83b);
        assert!(parse_path("px4daemon:any+lnb+j83b").unwrap().1.j83b);
        for bad in [
            "+j83b+tsmf=0",
            "+tlv+j83b",
            "+j83b+qam=256",
            "+j83b+coffset=0",
            "+j83b+j83b",
        ] {
            assert!(parse_path(&format!("px4daemon:0{bad}")).is_err(), "{bad}");
        }
        assert_eq!(
            space_channel_plan("px4daemon:0+j83b"),
            Some(Space3Plan::J83b)
        );
        assert_eq!(
            space_channel_plan("px4daemon:0+lnb"),
            Some(Space3Plan::Catv)
        );
        assert_eq!(space_channel_plan("/dev/px4video0"), None);
    }

    #[test]
    fn j83b_tune_plan_and_names_cover_h01_through_h77() {
        let (_, options, _, _) = parse_path("px4daemon:0+j83b").unwrap();
        assert_eq!(
            tune_target(3, 0, &options).unwrap(),
            TuneTarget {
                system: system::ISDB_C,
                freq_khz: 93_250,
                modulation: 256,
                tlv: false,
                bonded: false,
            }
        );
        assert_eq!(channel_name(3, 0, &options).as_deref(), Some("H01"));
        assert_eq!(j83b_frequency(1).unwrap(), 99_750);
        assert_eq!(
            tune_target(3, 76, &options).unwrap(),
            TuneTarget {
                system: system::ISDB_C,
                freq_khz: 587_250,
                modulation: 256,
                tlv: false,
                bonded: false,
            }
        );
        assert_eq!(channel_name(3, 76, &options).as_deref(), Some("H77"));
        assert!(tune_target(3, 77, &options).is_err());
        assert_eq!(channel_name(3, 77, &options), None);
    }

    #[test]
    fn non_j83b_space_three_keeps_catv_channel_plan() {
        let options = ReceiverOptions::default();
        for channel in [0, 9, 10, 50] {
            assert_eq!(
                tune_target(3, channel, &options).unwrap().freq_khz,
                space_channel_to_freq(3, channel).unwrap().1
            );
        }
        assert_eq!(channel_name(3, 0, &options).as_deref(), Some("C13"));
        assert_eq!(channel_name(3, 50, &options).as_deref(), Some("C63"));
    }

    /// The frequency formulas are px4_drv's (`px4rec.cpp`); these pin the two
    /// ends of each band so a typo cannot silently mistune.
    #[test]
    fn maps_space_channel_to_the_same_frequencies_px4rec_uses() {
        // GR channel 0 → UHF 13 → 473143 kHz; channel 49 → UHF 62 → 767143.
        assert_eq!(
            space_channel_to_freq(0, 0).unwrap(),
            (system::ISDB_T, 473_143)
        );
        assert_eq!(
            space_channel_to_freq(0, 49).unwrap(),
            (system::ISDB_T, 767_143)
        );
        // BS transponder 0 and 11.
        assert_eq!(
            space_channel_to_freq(1, 0).unwrap(),
            (system::ISDB_S, 1_049_480)
        );
        assert_eq!(
            space_channel_to_freq(1, 11).unwrap(),
            (system::ISDB_S, 1_471_440)
        );
        // CS110 transponder 0 and 11.
        assert_eq!(
            space_channel_to_freq(2, 0).unwrap(),
            (system::ISDB_S, 1_613_000)
        );
        assert_eq!(
            space_channel_to_freq(2, 11).unwrap(),
            (system::ISDB_S, 2_053_000)
        );
    }

    #[test]
    fn catv_frequencies_include_chset_corrections_and_override() {
        assert_eq!(
            space_channel_to_freq(3, 0).unwrap(),
            (system::ISDB_C, 110_800)
        );
        assert_eq!(
            space_channel_to_freq(3, 9).unwrap(),
            (system::ISDB_C, 166_725)
        );
        assert_eq!(
            space_channel_to_freq(3, 10).unwrap(),
            (system::ISDB_C, 224_650)
        );
        assert_eq!(
            space_channel_to_freq(3, 50).unwrap(),
            (system::ISDB_C, 464_400)
        );
        for channel in 0..51 {
            let physical = if channel < 10 {
                channel + 3
            } else {
                channel + 12
            };
            assert_eq!(
                catv_frequency(channel, Some(0)).unwrap(),
                93_000 + physical * 6_000 + if physical == 12 { 2_000 } else { 0 }
            );
        }
        assert_eq!(catv_frequency(0, Some(-600)).unwrap(), 110_400);
        assert!(catv_frequency(0, Some(i32::MIN)).is_err());
        assert!(catv_frequency(51, None).is_err());
    }

    #[test]
    fn parses_catv_suffixes_in_any_order_and_rejects_invalid_values() {
        let (_, options, ctrl, _) =
            parse_path("px4daemon:any+tlv+coffset=-275+qam=256+lnb+tsmf=14@/run/px4_ctrl.sock")
                .unwrap();
        assert_eq!(
            options,
            ReceiverOptions {
                lnb: true,
                tlv: true,
                tsmf: Some(14),
                qam: Some(256),
                coffset: Some(-275),
                j83b: false,
            }
        );
        assert_eq!(ctrl, "/run/px4_ctrl.sock");
        assert_eq!(
            parse_path("px4daemon:0+tsmf=0+qam=64+coffset=0")
                .unwrap()
                .1
                .qam,
            Some(64)
        );
        let (_, options, _, _) = parse_path("px4daemon:any+tlv").unwrap();
        assert_eq!(
            tuning_params(system::ISDB_C, &options, 256, false, false)[4],
            (5, 0)
        );
        for suffix in [
            "tsmf=15",
            "tsmf=-1",
            "tsmf=",
            "qam=0",
            "qam=128",
            "qam=x",
            "coffset=2147483648",
            "coffset=",
            "unknown",
            "lnb=1",
            "tlv=1",
            "",
            "lnb+lnb",
        ] {
            assert!(
                parse_path(&format!("px4daemon:0+{suffix}")).is_err(),
                "{suffix}"
            );
        }
    }

    #[test]
    fn variable_length_params_match_cpp_wire_layout() {
        let plain = params_command(cmd::SET_PARAMS, system::ISDB_T, 473_143, &[]);
        assert_eq!(plain.len(), 28);
        assert_eq!(get_u32(&plain, 16), 0);
        let options = ReceiverOptions {
            tsmf: Some(14),
            tlv: true,
            ..Default::default()
        };
        let params = tuning_params(system::ISDB_C, &options, 256, false, false);
        let wire = params_command(cmd::SET_PARAMS, system::ISDB_C, 166_725, &params);
        let words: Vec<u32> = wire
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(
            words,
            vec![17, 0, 0x40, 166_725, 7, 1, 6, 2, 5274, 3, 256, 4, 1, 5, 14, 6, 0, 7, 1]
        );
        assert_eq!(wire.len(), 76);
        let normal = tuning_params(
            system::ISDB_C,
            &ReceiverOptions::default(),
            64,
            false,
            false,
        );
        assert_eq!(normal, vec![(1, 6), (2, 5274), (3, 64)]);
        assert_eq!(
            params_command(cmd::SET_PARAMS, system::ISDB_C, 110_800, &normal).len(),
            44
        );
        assert_eq!(
            tuning_params(system::ISDB_C, &options, 0, true, false),
            vec![(1, 6), (2, 5274), (3, 0)]
        );
        assert!(tuning_params(system::ISDB_S, &options, 256, false, false).is_empty());
        let tsmf = ReceiverOptions {
            tsmf: Some(0),
            ..Default::default()
        };
        assert_eq!(
            params_command(
                cmd::SET_PARAMS,
                system::ISDB_C,
                166_725,
                &tuning_params(system::ISDB_C, &tsmf, 256, false, false)
            )
            .len(),
            68
        );
        let get = params_command(cmd::GET_PARAMS, 0, 0, &[(3, 0)]);
        assert_eq!(
            (get.len(), get_u32(&get, 0), get_u32(&get, 20)),
            (28, 16, 3)
        );
    }

    /// 診断用。CATV 全 51 チャンネルを順に選局し、選局結果・ロック・C/N・
    /// 2 秒間の受信量と PID の内訳を出す。失敗させない (回線ごとに中身が違う)。
    /// `PX4_TEST_SWEEP_RECEIVER` (既定 `px4daemon:any`)、
    /// `PX4_TEST_SWEEP_CHANNELS` (例 `7,16,17,18`) で対象を変えられる。
    #[test]
    #[ignore = "requires a running DriverHost_PX4 and a CATV feed"]
    fn hardware_catv_sweep_reports_lock_cnr_and_bytes() {
        let path = std::env::var("PX4_TEST_SWEEP_RECEIVER")
            .unwrap_or_else(|_| "px4daemon:any".to_string());
        let channels: Vec<u32> = match std::env::var("PX4_TEST_SWEEP_CHANNELS") {
            Ok(list) => list
                .split(',')
                .filter_map(|c| c.trim().parse().ok())
                .collect(),
            Err(_) => (0..51).collect(),
        };
        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");
        let mut buf = vec![0; 65536];
        for channel in channels {
            let tune = tuner.set_channel(3, channel);
            let lock = tuner.check_lock();
            let cnr = tuner.get_signal_level();
            let unsupported = tuner.signal_measurement_unsupported();
            let mut bytes = Vec::new();
            if tune.is_ok() {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    if tuner.wait_ts_stream(200) {
                        if let Ok((n, _)) = tuner.get_ts_stream(&mut buf) {
                            bytes.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            }
            let mut pids = std::collections::BTreeMap::<u16, usize>::new();
            let mut offset = bytes.iter().position(|&b| b == 0x47).unwrap_or(bytes.len());
            while offset + 188 <= bytes.len() {
                if bytes[offset] != 0x47 {
                    offset += 1;
                    continue;
                }
                let pid = (u16::from(bytes[offset + 1] & 0x1F) << 8) | u16::from(bytes[offset + 2]);
                *pids.entry(pid).or_default() += 1;
                offset += 188;
            }
            let mut top: Vec<_> = pids.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = top
                .iter()
                .take(4)
                .map(|(pid, n)| format!("0x{pid:04X}x{n}"))
                .collect();
            println!(
                "C{:<2} ch={:<2} tune={} lock={} cnr={:.2}dB cnr_unsupported={} bytes={} ({} kbps) pat={} top=[{}]",
                channel + 13,
                channel,
                match &tune {
                    Ok(()) => "ok".to_string(),
                    Err(error) => format!("ERR({error})"),
                },
                lock,
                cnr,
                unsupported,
                bytes.len(),
                bytes.len() * 8 / 2000,
                pids.contains_key(&0x0000),
                top.join(" "),
            );
        }
    }

    #[test]
    #[ignore = "requires a running DriverHost_PX4 and ISDB-C CATV signal"]
    fn hardware_catv_c13_streams_ts() {
        let path =
            std::env::var("PX4_TEST_CATV_RECEIVER").unwrap_or_else(|_| "px4daemon:any".to_string());
        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");
        tuner.set_channel(3, 0).expect("tune C13");
        assert!(tuner.check_lock());
        let mut bytes = Vec::new();
        let mut buf = vec![0; 65536];
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while bytes.len() < 188 * 1000 && std::time::Instant::now() < deadline {
            if tuner.wait_ts_stream(200) {
                match tuner.get_ts_stream(&mut buf) {
                    Ok((n, _)) => bytes.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("C13 read failed: {e}"),
                }
            }
        }
        assert!(
            bytes.len() >= 188 * 1000,
            "insufficient TS: {} bytes",
            bytes.len()
        );
        for packet in bytes.chunks_exact(188) {
            assert_eq!(packet[0], 0x47);
            assert_eq!(packet[1] & 0x80, 0, "TS transport error");
        }
    }

    #[test]
    #[ignore = "requires a running DriverHost_PX4 and C22 TSMF/TLV CATV signal"]
    fn hardware_catv_c22_streams_contiguous_tlv() {
        let path = std::env::var("PX4_TEST_TLV_RECEIVER")
            .unwrap_or_else(|_| "px4daemon:any+tlv".to_string());
        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");
        tuner.set_channel(3, 9).expect("tune C22");
        assert!(tuner.check_lock());
        let mut pending = Vec::new();
        let mut buf = vec![0; 65536];
        let mut packets = 0;
        let mut total = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while packets < 1000 && std::time::Instant::now() < deadline {
            if !tuner.wait_ts_stream(200) {
                continue;
            }
            match tuner.get_ts_stream(&mut buf) {
                Ok((n, _)) => {
                    pending.extend_from_slice(&buf[..n]);
                    total += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("C22 read failed: {e}"),
            }
            // TLV: sync/type/big-endian payload length. Preserve incomplete
            // packets between reads; never resynchronize by skipping bytes.
            let mut consumed = 0;
            while pending.len() - consumed >= 4 {
                let header = &pending[consumed..consumed + 4];
                assert_eq!(header[0], 0x7f, "lost TLV boundary at packet {packets}");
                let size = 4 + usize::from(u16::from_be_bytes([header[2], header[3]]));
                if pending.len() - consumed < size {
                    break;
                }
                consumed += size;
                packets += 1;
            }
            pending.drain(..consumed);
        }
        assert!(
            packets >= 1000,
            "only {packets} TLV packets / {total} bytes"
        );
    }

    /// End-to-end smoke test against real hardware. Requires a running
    /// `DriverHost_PX4` and a connected, antenna-fed PX4-family tuner, so it
    /// is `#[ignore]`d by default:
    ///
    /// ```text
    /// cargo test -p recisdb-proxy --lib px4_daemon -- --ignored --nocapture
    /// ```
    ///
    /// `PX4_TEST_RECEIVER` (default `px4daemon:0`) and `PX4_TEST_CHANNEL`
    /// (default `0`, i.e. UHF 13) select what to tune.
    #[test]
    #[ignore = "requires a running DriverHost_PX4 and connected tuner hardware"]
    fn hardware_smoke_test_tunes_and_streams() {
        let path = std::env::var("PX4_TEST_RECEIVER").unwrap_or_else(|_| "px4daemon:0".to_string());
        let channel: u32 = std::env::var("PX4_TEST_CHANNEL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");
        tuner.set_channel(0, channel).expect("tune");
        assert!(tuner.check_lock(), "demodulator did not report lock");

        let cnr = tuner.get_signal_level();
        println!("CNR = {:.2} dB", cnr);
        assert!(
            cnr > 0.0,
            "CNR should be positive on a locked channel, got {}",
            cnr
        );

        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0usize;
        let mut sync_ok = 0usize;
        let mut sync_seen = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && total < 2 * 1024 * 1024 {
            if !tuner.wait_ts_stream(200) {
                continue;
            }
            match tuner.get_ts_stream(&mut buf) {
                Ok((n, _)) if n > 0 => {
                    // Only a coarse check: the daemon does not guarantee that
                    // a read starts on a packet boundary, so just confirm
                    // sync bytes appear at the expected stride somewhere.
                    let mut i = 0;
                    while i + 188 < n {
                        if buf[i] == 0x47 {
                            sync_seen += 1;
                            if buf[i + 188] == 0x47 {
                                sync_ok += 1;
                            }
                            i += 188;
                        } else {
                            i += 1;
                        }
                    }
                    total += n;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("read failed: {e}"),
            }
        }

        println!(
            "read {} bytes, {}/{} sync strides held",
            total, sync_ok, sync_seen
        );
        assert!(
            total > 188 * 1000,
            "expected a real TS flow, got {} bytes",
            total
        );
        assert!(
            sync_ok * 10 >= sync_seen * 9,
            "TS sync stride held for only {}/{} candidates — data looks corrupt",
            sync_ok,
            sync_seen
        );
    }

    /// Re-tuning must keep TS flowing. Requires hardware; see
    /// [`hardware_smoke_test_tunes_and_streams`] for how to run it.
    ///
    /// `PX4_TEST_CHANNELS` is a comma-separated list of GR channel indices
    /// (default `0,6,8` = UHF 13/19/21). Each is tuned in turn on the *same*
    /// tuner instance and must deliver data — the scan that first exercised
    /// this backend stalled after several channel changes, so this pins the
    /// switching path specifically rather than a single tune.
    #[test]
    #[ignore = "requires a running DriverHost_PX4 and connected tuner hardware"]
    fn hardware_retune_keeps_streaming() {
        let path = std::env::var("PX4_TEST_RECEIVER").unwrap_or_else(|_| "px4daemon:0".to_string());
        let channels: Vec<u32> = std::env::var("PX4_TEST_CHANNELS")
            .unwrap_or_else(|_| "0,6,8".to_string())
            .split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect();

        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");

        for (round, &ch) in channels.iter().enumerate() {
            tuner
                .set_channel(0, ch)
                .unwrap_or_else(|e| panic!("tune round {round} ch {ch}: {e}"));

            let mut buf = vec![0u8; 256 * 1024];
            let mut total = 0usize;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline && total < 512 * 1024 {
                if !tuner.wait_ts_stream(200) {
                    continue;
                }
                match tuner.get_ts_stream(&mut buf) {
                    Ok((n, _)) => total += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("round {round} ch {ch}: read failed: {e}"),
                }
            }
            println!("round {round}: UHF {} → {} bytes", ch + 13, total);
            assert!(
                total > 188 * 500,
                "round {round} (UHF {}): only {} bytes after re-tune — the stream did not survive the channel change",
                ch + 13,
                total
            );
        }
    }

    /// Crossing between terrestrial and satellite forces a fresh `OPEN`
    /// (a receiver is opened for one `SystemType` at a time), which is the
    /// branch that also has to rebuild the data socket. Requires hardware
    /// with both antennas; override with `PX4_TEST_PLAN` as
    /// `space:channel` pairs.
    #[test]
    #[ignore = "requires a running DriverHost_PX4, and both terrestrial and satellite antennas"]
    fn hardware_retune_across_systems_keeps_streaming() {
        let path = std::env::var("PX4_TEST_RECEIVER").unwrap_or_else(|_| "px4daemon:0".to_string());
        let plan: Vec<(u32, u32)> = std::env::var("PX4_TEST_PLAN")
            .unwrap_or_else(|_| "0:0,1:0,0:6,1:2".to_string())
            .split(',')
            .filter_map(|pair| {
                let (s, c) = pair.trim().split_once(':')?;
                Some((s.parse().ok()?, c.parse().ok()?))
            })
            .collect();

        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");

        for (round, &(space, ch)) in plan.iter().enumerate() {
            tuner
                .set_channel(space, ch)
                .unwrap_or_else(|e| panic!("round {round} space {space} ch {ch}: {e}"));

            let mut buf = vec![0u8; 256 * 1024];
            let mut total = 0usize;
            let deadline = std::time::Instant::now() + Duration::from_secs(6);
            while std::time::Instant::now() < deadline && total < 512 * 1024 {
                if !tuner.wait_ts_stream(200) {
                    continue;
                }
                match tuner.get_ts_stream(&mut buf) {
                    Ok((n, _)) => total += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("round {round}: read failed: {e}"),
                }
            }
            println!("round {round}: space={space} ch={ch} → {total} bytes");
            assert!(
                total > 188 * 500,
                "round {round} (space={space} ch={ch}): only {total} bytes — the stream did not survive the system change"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_channels() {
        assert!(space_channel_to_freq(0, 50).is_err());
        assert!(space_channel_to_freq(1, 12).is_err());
        assert!(space_channel_to_freq(2, 12).is_err());
        assert!(space_channel_to_freq(3, 51).is_err());
        assert!(space_channel_to_freq(4, 0).is_err());
    }

    #[test]
    fn same_daemon_compares_control_sockets() {
        assert!(same_daemon(
            "px4daemon:any+tlv+coffset=458000@/run/a.sock",
            "px4daemon:any+qam=256@/run/a.sock"
        ));
        assert!(same_daemon("px4daemon:0", "px4daemon:any+tlv"));
        assert!(!same_daemon(
            "px4daemon:any@/run/a.sock",
            "px4daemon:any@/run/b.sock"
        ));
        assert!(!same_daemon("px4daemon:any", "C:/BonDriver_PX4.dll"));
    }

    #[test]
    fn one_receiver_serves_catv_ts_and_tlv_spaces() {
        let path = "px4daemon:any+qam=256@/run/px4_ctrl_pipe.sock";
        let (_, options, _, _) = parse_path(path).unwrap();

        // Space 3: TS, same carriers as before, now extended into the UHF band.
        let ts = tune_target(3, 9, &options).unwrap();
        assert!(!ts.tlv);
        assert_eq!(
            tuning_params(ts.system, &options, ts.modulation, false, false).len(),
            3
        );
        assert_eq!(tune_target(3, 51, &options).unwrap().freq_khz, 473_000);
        assert_eq!(channel_name(3, 51, &options).as_deref(), Some("U13"));

        // Space 4: TLV on the same frequencies, always 256QAM.
        let tlv = tune_target(CATV_TLV_SPACE, 9, &options).unwrap();
        assert!(tlv.tlv);
        assert_eq!(tlv.freq_khz, ts.freq_khz);
        // The measured BS4K carriers: 225 MHz (C22) and 587-605 MHz (U32-U35).
        assert_eq!(
            tune_target(CATV_TLV_SPACE, 10, &options).unwrap().freq_khz,
            224_650
        );
        assert_eq!(
            tune_target(CATV_TLV_SPACE, 70, &options).unwrap().freq_khz,
            587_000
        );
        assert_eq!(
            tune_target(CATV_TLV_SPACE, 73, &options).unwrap().freq_khz,
            605_000
        );
        assert_eq!(
            channel_name(CATV_TLV_SPACE, 70, &options).as_deref(),
            Some("U32")
        );
        assert!(tune_target(CATV_TLV_SPACE, 101, &options).is_err());
        let mut tlv_options = options.clone();
        tlv_options.tlv = tlv.tlv;
        let params = tuning_params(tlv.system, &tlv_options, tlv.modulation, false, false);
        assert_eq!(&params[2..], &[(3, 256), (4, 1), (5, 0), (6, 0), (7, 1)]);

        // Auto QAM still means 256 for TLV.
        let (_, auto, _, _) = parse_path("px4daemon:any").unwrap();
        assert_eq!(
            tune_target(CATV_TLV_SPACE, 0, &auto).unwrap().modulation,
            256
        );
        assert_eq!(tune_target(3, 0, &auto).unwrap().modulation, 0);

        assert!(tune_delivers_tlv(path, CATV_TLV_SPACE));
        assert!(!tune_delivers_tlv(path, 3));
        assert!(!tune_delivers_tlv(path, 0));
        // Legacy +tlv receivers: TLV on space 3 and no separate TLV space.
        assert!(tune_delivers_tlv("px4daemon:any+tlv", 3));
        assert!(!tune_delivers_tlv("px4daemon:any+tlv", CATV_TLV_SPACE));
        // J.83B has no TSMF.
        assert!(!tune_delivers_tlv("px4daemon:0+j83b", CATV_TLV_SPACE));
    }

    #[test]
    fn tlv_receivers_expose_only_the_catv_space() {
        let (_, options, _, _) = parse_path("px4daemon:any+tlv+qam=256").unwrap();
        for space in 0..3 {
            assert!(tune_target(space, 0, &options).is_err(), "space {space}");
            assert_eq!(channel_name(space, 0, &options), None, "space {space}");
        }
        assert_eq!(channel_name(3, 9, &options).as_deref(), Some("C22"));
        assert!(tune_target(3, 9, &options).is_ok());

        // Without +tlv the terrestrial space stays available.
        let (_, options, _, _) = parse_path("px4daemon:any+qam=256").unwrap();
        assert_eq!(channel_name(0, 0, &options).as_deref(), Some("GR13"));
    }

    /// 複数搬送波 (BS8K, U29 = 569 MHz から3波) を合成して TLV が流れるか。
    /// ISDB-C を受信できる受信機が同じ DriverHost に3つ以上必要。
    /// 例: PX4_RUNTIME_DIR の制御ソケットを付けて
    /// `PX4_TEST_BOND_RECEIVER=px4daemon:any+qam=256@…/px4_ctrl_pipe.sock`。
    #[test]
    #[ignore = "requires DriverHost_PX4 with three ISDB-C receivers and BS8K on CATV"]
    fn hardware_bs8k_group_combines_into_tlv() {
        let path = std::env::var("PX4_TEST_BOND_RECEIVER")
            .unwrap_or_else(|_| "px4daemon:any+qam=256".to_string());
        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");
        let started = std::time::Instant::now();
        tuner
            .set_channel(CATV_BOND_SPACE, 67)
            .expect("tune BS8K group on U29");
        eprintln!("set_channel took {:?}", started.elapsed());
        let mut pending = Vec::new();
        let mut buf = vec![0; 1 << 20];
        let (mut packets, mut total) = (0usize, 0usize);
        let mut first_byte = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if !tuner.wait_ts_stream(100) {
                continue;
            }
            match tuner.get_ts_stream(&mut buf) {
                Ok((n, _)) => {
                    first_byte.get_or_insert_with(|| started.elapsed());
                    pending.extend_from_slice(&buf[..n]);
                    total += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("read failed: {e}"),
            }
            let mut consumed = 0;
            while pending.len() - consumed >= 4 {
                let header = &pending[consumed..consumed + 4];
                assert_eq!(header[0], 0x7f, "lost TLV boundary at packet {packets}");
                let size = 4 + usize::from(u16::from_be_bytes([header[2], header[3]]));
                if pending.len() - consumed < size {
                    break;
                }
                consumed += size;
                packets += 1;
            }
            pending.drain(..consumed);
        }
        let seconds = deadline.duration_since(started).as_secs_f64();
        eprintln!(
            "first byte after {first_byte:?}; {packets} TLV packets, {total} bytes ({:.1} Mbps avg)",
            total as f64 * 8.0 / seconds / 1e6
        );
        // 3波 x 256QAM で約 100 Mbps。合成できていれば 1 波分 (38 Mbps) を大きく超える。
        assert!(
            total as f64 * 8.0 / seconds > 50e6,
            "too little data: {total} bytes"
        );
    }

    /// 群の2番目以降の搬送波は、空間 5 では登録しない (第1搬送波で選局する)。
    #[test]
    #[ignore = "requires DriverHost_PX4 and BS8K on CATV"]
    fn hardware_bs8k_second_carrier_is_not_a_channel() {
        let path = std::env::var("PX4_TEST_BOND_RECEIVER")
            .unwrap_or_else(|_| "px4daemon:any+qam=256".to_string());
        let tuner = Px4DaemonTuner::new(&path).expect("open receiver");
        let err = tuner
            .set_channel(CATV_BOND_SPACE, 68)
            .expect_err("U30 is carrier 2");
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable, "{err}");
        assert!(err.to_string().contains("carrier 2/3"), "{err}");
    }

    #[test]
    fn multi_carrier_space_tunes_raw_slots_and_delivers_tlv() {
        let (_, options, _, _) = parse_path("px4daemon:any+qam=256").unwrap();
        // U29 = 569 MHz: first carrier of the BS8K group measured 2026-10-30.
        let target = tune_target(CATV_BOND_SPACE, 67, &options).unwrap();
        assert_eq!(
            target,
            TuneTarget {
                system: system::ISDB_C,
                freq_khz: 569_000,
                modulation: 256,
                tlv: true,
                bonded: true,
            }
        );
        assert_eq!(
            channel_name(CATV_BOND_SPACE, 67, &options).as_deref(),
            Some("U29")
        );
        assert!(tune_delivers_tlv("px4daemon:any+qam=256", CATV_BOND_SPACE));
        // TSMF_MODE=2 alone: no relative stream, no TLV extraction in the daemon.
        assert_eq!(
            tuning_params(system::ISDB_C, &options, 256, false, true),
            vec![(1, 6), (2, 5274), (3, 256), (4, 2)]
        );
        // +tlv and +j83b receivers have no multi-carrier space.
        for path in ["px4daemon:any+tlv", "px4daemon:any+j83b"] {
            let (_, options, _, _) = parse_path(path).unwrap();
            assert!(
                tune_target(CATV_BOND_SPACE, 67, &options).is_err(),
                "{path}"
            );
        }
    }
}
