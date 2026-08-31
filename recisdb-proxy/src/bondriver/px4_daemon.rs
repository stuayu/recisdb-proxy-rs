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
//! - **data** (`/tmp/px4_data_pipe.sock`) — raw TS, after the client claims a
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
//!   → SET_PARAMS{system, freq_kHz} → TUNE{timeout} → SET_CAPTURE{true}
//!   → connect(data) → SET_DATA_ID{data_id} → read TS …
//! teardown: SET_CAPTURE{false} → CLOSE
//! ```
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
use std::time::Duration;

use log::{debug, info, warn};

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
/// margin for a weak signal while keeping a 74-channel scan minutes rather
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

/// Convert this crate's `(space, channel)` indices to the daemon's
/// `(system, freq_kHz)`.
///
/// The space/channel convention is the same one
/// [`super::unix::CharDevTuner`] uses, so a `channels` row scanned on Linux
/// addresses the same physical channel here:
/// space 0 = GR (channel 0..49 → UHF 13..62), 1 = BS, 2 = CS110.
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
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown tuning space {}", space),
        )),
    }
}

/// Parse `px4daemon:<index>` (or `px4daemon:any`), optionally followed by
/// `@<ctrl_sock>` for a non-default socket path.
fn parse_path(path: &str) -> Result<(i32, bool, String, String), io::Error> {
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

    // `+lnb` opts this receiver into supplying LNB power on satellite
    // channels. Opt-in for the same reason px4_drv's own BonDriver keeps it
    // behind an `LNBPower` setting: feeding voltage onto a line that another
    // device already powers is not something to do by default. Without it,
    // BS/CS simply never lock on a setup that has no other power injector.
    let (index_part, lnb_power) = match index_part.strip_suffix("+lnb") {
        Some(stripped) => (stripped, true),
        None => (index_part, false),
    };

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

    Ok((index, lnb_power, ctrl_sock, data_sock))
}

// ---------------------------------------------------------------------
// Tuner
// ---------------------------------------------------------------------

/// One receiver claimed from the daemon.
pub struct Px4DaemonTuner {
    ctrl: Mutex<UnixStream>,
    /// Connected and given its `data_id` once capture starts.
    data: Mutex<Option<UnixStream>>,
    data_sock_path: String,
    /// Receiver index requested (-1 = any).
    index: i32,
    /// Whether this receiver may supply LNB power for satellite channels
    /// (`+lnb` in the path).
    lnb_power: bool,
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
    /// terrestrial and satellite needs a fresh `OPEN`.
    opened_system: AtomicI32,
    capturing: AtomicBool,
    current_space: AtomicI32,
}

impl Px4DaemonTuner {
    pub fn new(path: &str) -> Result<Self, io::Error> {
        let (index, lnb_power, ctrl_sock, data_sock) = parse_path(path)?;

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

        let tuner = Self {
            ctrl: Mutex::new(ctrl),
            data: Mutex::new(None),
            data_sock_path: data_sock,
            index,
            lnb_power,
            lnb_on: AtomicBool::new(false),
            data_id: AtomicU64::new(NO_DATA_ID),
            opened_system: AtomicI32::new(0),
            capturing: AtomicBool::new(false),
            current_space: AtomicI32::new(0),
        };

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

        Ok(tuner)
    }

    /// Send a command and read the same-sized response in place.
    ///
    /// Every control command is a fixed-size struct that the daemon mutates
    /// and echoes back, so request and response share one buffer.
    fn transact(&self, buf: &mut [u8]) -> Result<(), io::Error> {
        let mut ctrl = self
            .ctrl
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "px4daemon ctrl lock poisoned"))?;
        ctrl.write_all(buf)?;
        ctrl.flush()?;
        ctrl.read_exact(buf)?;
        Ok(())
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

    fn close_receiver(&self) {
        let mut buf = vec![0u8; CLOSE_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::CLOSE));
        if let Err(e) = self.transact(&mut buf) {
            warn!("[px4daemon] CLOSE failed: {}", e);
        }
        self.opened_system.store(0, Ordering::Release);
        self.data_id.store(NO_DATA_ID, Ordering::Release);
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
        if !self.lnb_power {
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

    fn set_params(&self, sys: u32, freq_khz: u32) -> Result<(), io::Error> {
        let mut buf = vec![0u8; PARAMS_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::SET_PARAMS));
        put_u32(&mut buf, 8, sys); // param_set.system
        put_u32(&mut buf, 12, freq_khz); // param_set.freq
        put_u32(&mut buf, 16, 0); // param_set.num (no extra Parameters)
        self.transact_checked(&mut buf, "SET_PARAMS")
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

    // -----------------------------------------------------------------
    // BonDriverTuner-shaped API
    // -----------------------------------------------------------------

    pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
        let (sys, freq_khz) = space_channel_to_freq(space, channel)?;

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
        // terrestrial and satellite means re-opening it, and that *does*
        // invalidate the data socket (a new `OPEN` yields a new `data_id`).
        let opened = self.opened_system.load(Ordering::Acquire) as u32;
        if opened != sys {
            if opened != 0 {
                if self.capturing.load(Ordering::Acquire) {
                    let _ = self.set_capture(false);
                }
                self.drop_data_socket();
                self.close_receiver();
            }
            self.open_receiver(sys)?;
        }

        self.apply_lnb_power(sys)?;
        self.set_params(sys, freq_khz)?;
        self.tune()?;
        if !self.capturing.load(Ordering::Acquire) {
            self.set_capture(true)?;
        }

        // Always reconnect when we do not already hold a socket — which,
        // after the capture stop above, is every re-tune. See that comment
        // for why an existing socket cannot be reused across a capture stop.
        let needs_data = self.data.lock().map(|g| g.is_none()).unwrap_or(true);
        if needs_data {
            self.attach_data_socket()?;
        }

        // Drop whatever the previous channel left in the daemon's ring so the
        // caller does not start the new channel by reading the old one.
        self.purge_ts_stream();

        self.current_space.store(space as i32, Ordering::Release);
        info!(
            "[px4daemon] tuned: space={} channel={} → system=0x{:02x} {} kHz",
            space, channel, sys, freq_khz
        );
        Ok(())
    }

    /// CNR in dB, as reported by the demodulator.
    pub fn get_signal_level(&self) -> f32 {
        let mut buf = vec![0u8; STATS_CMD_SIZE];
        buf[..HEADER_SIZE].copy_from_slice(&header(cmd::READ_STATS));
        put_u32(&mut buf, 8, 1); // stat_set.num
        put_u32(&mut buf, 12, STAT_CNR); // stat_set.data[0].type
        if self.transact(&mut buf).is_err() || get_u32(&buf, 4) != status::SUCCEEDED {
            return 0.0;
        }
        // px4_drv reports CNR in millibels (×1000 dB), same as the Linux
        // driver's PTX_GET_CNR.
        get_i32(&buf, 16) as f32 / 1000.0
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

    pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
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

    /// Read TS. Returns `(bytes_read, remaining)`; the daemon does not report
    /// a backlog, so `remaining` is always 0.
    pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
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
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "px4daemon: data socket closed by the daemon",
            )),
            Ok(n) => Ok((n, 0)),
            Err(e) => Err(e),
        }
    }

    pub fn purge_ts_stream(&self) {
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
        let mut discard = vec![0u8; 65536];
        for _ in 0..16 {
            if !self.wait_ts_stream(0) {
                break;
            }
            match self.get_ts_stream(&mut discard) {
                Ok((n, _)) if n > 0 => continue,
                _ => break,
            }
        }
    }

    pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
        match space {
            0 => Some("GR".to_string()),
            1 => Some("BS".to_string()),
            2 => Some("CS".to_string()),
            _ => None,
        }
    }

    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
        match space {
            0 if channel <= 49 => Some(format!("GR{}", channel + 13)),
            1 if channel <= 11 => Some(format!("BS{}", channel * 2 + 1)),
            2 if channel <= 11 => Some(format!("CS{}", channel * 2 + 2)),
            _ => None,
        }
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
        self.drop_data_socket();
        if self.lnb_on.load(Ordering::Acquire) {
            // The daemon reference-counts LNB power and asserts the calls
            // balance, so this must not be skipped.
            let _ = self.apply_lnb_power(system::ISDB_T);
        }
        if self.opened_system.load(Ordering::Acquire) != 0 {
            self.close_receiver();
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
                false,
                DEFAULT_CTRL_SOCK.to_string(),
                DEFAULT_DATA_SOCK.to_string()
            )
        );
        assert_eq!(parse_path("px4daemon:any").unwrap().0, -1);
        assert_eq!(parse_path("px4daemon:").unwrap().0, -1);

        // LNB power is opt-in per receiver.
        let (idx, lnb, _, _) = parse_path("px4daemon:2+lnb").unwrap();
        assert_eq!((idx, lnb), (2, true));
        assert!(!parse_path("px4daemon:2").unwrap().1);

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
        assert!(space_channel_to_freq(3, 0).is_err());
    }
}
