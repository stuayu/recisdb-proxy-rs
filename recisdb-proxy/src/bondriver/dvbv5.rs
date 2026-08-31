//! Linux DVB API (DVBv5) tuner backend.
//!
//! Targets kernel-standard DVB drivers (`smsdvb`, `dvb-usb-*`, etc.) that expose
//! `/dev/dvb/adapterN/{frontend,demux,dvr}M`, as opposed to `unix.rs` which
//! speaks px4-drv/pt3-drv's private chardev ioctl protocol. The motivating
//! device is a PX-Q1UD (Siano chip, `smsdvb`), which only ever appears under
//! `/dev/dvb/adapter0` — never `/dev/px4videoN` — so `unix.rs`'s `CharDevTuner`
//! cannot drive it (its ioctls are magic `0x8d`, unrelated to DVB's `'o'`
//! ioctls, and fail with ENOTTY).
//!
//! `recisdb-rs/src/tuner/linux/dvbv5.rs` already talks to the same kernel API,
//! but through the `dvbv5-sys` crate, which binds `libdvbv5` (a C library from
//! v4l-utils). That's fine for `recisdb-rs`, which is normally built and run
//! on the same Linux box. `recisdb-proxy` additionally needs to cross-build
//! from macOS to Linux (see `docs/BUILD.md`), and pulling in a C library just
//! to reach the DVB API would mean carrying `libdvbv5` (and its own
//! dependencies) in the cross sysroot. The DVB ioctl surface used here is
//! small, kernel-stable ABI (`linux/dvb/frontend.h`, `linux/dvb/dmx.h`), so
//! this module instead defines the handful of ioctls and structs it needs
//! directly via `nix::ioctl_*!` and `#[repr(C)]`, with no additional
//! dependency beyond `nix` (already used by `unix.rs`) and `libc`.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use log::{debug, info, warn};

// ---------------------------------------------------------------------------
// linux/dvb/frontend.h / linux/dvb/dmx.h ioctl + struct definitions.
//
// These are kernel UAPI headers; their ioctl numbers and struct layouts are
// part of the stable Linux ABI and do not change across kernel versions.
// ---------------------------------------------------------------------------

/// `struct dtv_property` (linux/dvb/frontend.h). The kernel struct is
/// `__attribute__((packed))`; the `u` field is a union whose largest member
/// (`struct dtv_property_buffer { __u8 data[32]; __u32 len; ... }` /
/// `struct dtv_fe_stats`) fits in 56 bytes on 64-bit. We keep it as a raw
/// byte array and interpret it by hand per-command rather than modeling the
/// union in Rust.
#[repr(C, packed)]
struct DtvProperty {
    cmd: u32,
    reserved: [u32; 3],
    u: [u8; 56],
    result: i32,
}

const _: () = assert!(std::mem::size_of::<DtvProperty>() == 76);

/// `struct dtv_properties` (linux/dvb/frontend.h).
#[repr(C)]
struct DtvProperties {
    num: u32,
    props: *mut DtvProperty,
}

/// `struct dmx_pes_filter_params` (linux/dvb/dmx.h). Not packed in the
/// kernel header, so the `u16 pid` is followed by 2 bytes of padding before
/// the `u32` fields on any platform with natural alignment (matches x86/ARM).
#[repr(C)]
struct DmxPesFilterParams {
    pid: u16,
    input: u32,
    output: u32,
    pes_type: u32,
    flags: u32,
}

// DTV_* property command IDs (linux/dvb/frontend.h).
const DTV_TUNE: u32 = 1;
const DTV_CLEAR: u32 = 2;
const DTV_FREQUENCY: u32 = 3;
const DTV_BANDWIDTH_HZ: u32 = 5;
const DTV_DELIVERY_SYSTEM: u32 = 17;
const DTV_ISDBT_PARTIAL_RECEPTION: u32 = 18;
const DTV_ISDBT_SOUND_BROADCASTING: u32 = 19;
const DTV_ISDBT_LAYER_ENABLED: u32 = 41;
const DTV_ENUM_DELSYS: u32 = 44;
const DTV_STAT_CNR: u32 = 63;

/// All three ISDB-T hierarchical layers (A|B|C) enabled — the full-segment
/// broadcast every Japanese terrestrial station uses.
const ISDBT_ALL_LAYERS: u32 = 0x07;

// fe_delivery_system values (linux/dvb/frontend.h).
const SYS_ISDBT: u8 = 8;
const SYS_ISDBS: u8 = 9;

// fe_status bitmask (linux/dvb/frontend.h).
const FE_HAS_SIGNAL: u32 = 0x01;
const FE_HAS_CARRIER: u32 = 0x02;
#[allow(dead_code)]
const FE_HAS_VITERBI: u32 = 0x04;
#[allow(dead_code)]
const FE_HAS_SYNC: u32 = 0x08;
const FE_HAS_LOCK: u32 = 0x10;

// fecap_scale_params scale (linux/dvb/frontend.h, struct dtv_fe_stats).
const FE_SCALE_DECIBEL: u8 = 1;
const FE_SCALE_RELATIVE: u8 = 2;

// dmx_input / dmx_output / dmx_pes_type / DMX_IMMEDIATE_START (linux/dvb/dmx.h).
const DMX_IN_FRONTEND: u32 = 0;
const DMX_OUT_TS_TAP: u32 = 2;
const DMX_PES_OTHER: u32 = 20;
const DMX_IMMEDIATE_START: u32 = 4;
/// PID 0x2000 is the DVB API's "pass everything" pseudo-PID for a full TS tap.
const FULL_TS_PID: u16 = 0x2000;

nix::ioctl_write_ptr!(fe_set_property, b'o', 82, DtvProperties);
nix::ioctl_read!(fe_get_property, b'o', 83, DtvProperties);
nix::ioctl_read!(fe_read_status, b'o', 69, u32);
nix::ioctl_read!(fe_read_snr, b'o', 72, u16);

// DMX_START is intentionally not bound: the PES filter is installed with
// DMX_IMMEDIATE_START, so the demux starts as part of DMX_SET_PES_FILTER.
nix::ioctl_none!(dmx_stop, b'o', 42);
nix::ioctl_write_ptr!(dmx_set_pes_filter, b'o', 44, DmxPesFilterParams);
// DMX_SET_BUFFER_SIZE is `_IO('o', 45)` — no embedded type, argument passed
// by value (the buffer size in bytes), hence `ioctl_write_int_bad!` with a
// manually built bare `_IO` request code instead of `ioctl_write_int!`
// (which would encode a type size into the request).
nix::ioctl_write_int_bad!(dmx_set_buffer_size, nix::request_code_none!(b'o', 45));

/// Builds a single-property `FE_SET_PROPERTY`/`FE_GET_PROPERTY` call.
fn set_properties(fd: RawFd, props: &mut [DtvProperty]) -> nix::Result<()> {
    let dtv_props = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    unsafe { fe_set_property(fd, &dtv_props as *const DtvProperties) }?;
    Ok(())
}

fn get_properties(fd: RawFd, props: &mut [DtvProperty]) -> nix::Result<()> {
    let mut dtv_props = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    unsafe { fe_get_property(fd, &mut dtv_props as *mut DtvProperties) }?;
    Ok(())
}

fn dtv_prop(cmd: u32) -> DtvProperty {
    DtvProperty {
        cmd,
        reserved: [0; 3],
        u: [0; 56],
        result: 0,
    }
}

fn dtv_prop_u32(cmd: u32, value: u32) -> DtvProperty {
    let mut p = dtv_prop(cmd);
    p.u[0..4].copy_from_slice(&value.to_le_bytes());
    p
}

// ---------------------------------------------------------------------------
// Tuner
// ---------------------------------------------------------------------------

/// BonDriver-compatible wrapper for a Linux DVB API (DVBv5) frontend/demux/dvr
/// triple, e.g. `/dev/dvb/adapter0`.
///
/// # Scope
/// Only ISDB-T (`space=0`, "GR") is exposed. ISDB-S (BS/CS) support is not
/// implemented: even when the frontend reports `SYS_ISDBS` capability,
/// [`enum_tuning_space`](Self::enum_tuning_space) never returns a BS/CS
/// space, so the scanner will not attempt to tune satellite channels through
/// this backend. Add ISDB-S tuning (LNB voltage control via `DTV_VOLTAGE`,
/// stream ID / relative TS handling, frequency tables) as a follow-up if a
/// satellite-capable DVB device needs to go through this path.
pub struct DvbV5Tuner {
    frontend_fd: std::fs::File,
    demux_fd: std::fs::File,
    dvr_fd: std::fs::File,
    /// Whether `SYS_ISDBT` appeared in `DTV_ENUM_DELSYS` at open time.
    supports_isdbt: bool,
    /// Whether the frontend reached `FE_HAS_LOCK` during the most recent
    /// `set_channel`. Lets the scanner tell "this channel is empty, move on"
    /// apart from "this channel locked, so a momentary 0 dB reading is the
    /// demodulator settling rather than an empty channel".
    last_channel_locked: AtomicBool,
}

impl DvbV5Tuner {
    /// Accepts either `/dev/dvb/adapterN` (frontend/demux/dvr index 0
    /// implied) or `/dev/dvb/adapterN/frontendM` (explicit frontend index,
    /// matching demux/dvr index M).
    pub fn new(path: &str) -> Result<Self, io::Error> {
        let (adapter, frontend) = parse_adapter_path(path)?;

        let frontend_path = format!("/dev/dvb/adapter{}/frontend{}", adapter, frontend);
        let demux_path = format!("/dev/dvb/adapter{}/demux{}", adapter, frontend);
        let dvr_path = format!("/dev/dvb/adapter{}/dvr{}", adapter, frontend);

        // Deliberately open each device node explicitly rather than the bare
        // adapter directory: opening a directory with OpenOptions::read(true)
        // succeeds on Linux (you get a directory fd) but is useless for
        // ioctl/read, which is exactly the bug this backend fixes (the old
        // fallback to CharDevTuner would "successfully" open
        // /dev/dvb/adapter0 and then fail every ioctl with ENOTTY).
        let frontend_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&frontend_path)?;
        let demux_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&demux_path)?;
        let dvr_fd = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&dvr_path)?;

        // Best-effort: widen the dvr ring buffer so short scheduling delays
        // in the reader don't drop TS packets. Failure is not fatal (older
        // drivers may not support this ioctl).
        const DVR_BUFFER_SIZE: i32 = 8 * 1024 * 1024;
        if let Err(e) = unsafe { dmx_set_buffer_size(dvr_fd.as_raw_fd(), DVR_BUFFER_SIZE) } {
            debug!("DMX_SET_BUFFER_SIZE failed (non-fatal): {}", e);
        }

        let supports_isdbt = detect_isdbt_support(frontend_fd.as_raw_fd());
        // Logged at info: when a scan finds nothing, the first question is
        // always whether the frontend even claims to do ISDB-T. A device
        // whose firmware failed to load (the Siano parts need
        // /lib/firmware/isdbt_rio.inp, which most distros do not ship) still
        // creates its /dev/dvb nodes and still accepts every ioctl here — it
        // simply never locks. Seeing the delivery systems it reports is the
        // cheapest way to tell that apart from an antenna problem.
        info!(
            "DvbV5Tuner: opened {} (ISDB-T supported: {})",
            frontend_path, supports_isdbt
        );

        Ok(Self {
            frontend_fd,
            demux_fd,
            dvr_fd,
            supports_isdbt,
            last_channel_locked: AtomicBool::new(false),
        })
    }

    pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
        if space != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("DvbV5Tuner only supports space=0 (GR), got {}", space),
            ));
        }
        if channel > 49 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("GR channel {} out of range (0-49)", channel),
            ));
        }

        let fe_fd = self.frontend_fd.as_raw_fd();

        // Stop demux before re-tuning; ignore errors (nothing to stop yet on
        // first tune).
        let _ = unsafe { dmx_stop(self.demux_fd.as_raw_fd()) };

        // Clear any previous tuning parameters first (kernel recommends this
        // before a fresh DTV_TUNE sequence).
        let mut clear = [dtv_prop(DTV_CLEAR)];
        set_properties(fe_fd, &mut clear).map_err(io::Error::from)?;

        // channel=0 -> UHF13 = 473.142857 MHz, then 6 MHz steps (ISDB-T
        // channel raster). 473_142_857 Hz is the ARIB-standard UHF13 center
        // frequency; matches the formula used elsewhere in this workspace
        // (e.g. recisdb-rs's ISDB-T channel table).
        let freq_hz: u64 = 473_142_857 + (channel as u64) * 6_000_000;

        // The three ISDB-T layer properties are not optional in practice.
        // DTV_CLEAR leaves them at 0, and 0 in DTV_ISDBT_LAYER_ENABLED means
        // "no layer enabled" rather than "don't care", so a demodulator that
        // honours the field has nothing to decode and never reaches lock.
        // recisdb-rs's libdvbv5-based tuner sets exactly these three before
        // tuning (recisdb-rs/src/tuner/linux/dvbv5.rs) — matching it here
        // keeps both paths behaving the same on the same hardware.
        let mut tune = [
            dtv_prop_u32(DTV_DELIVERY_SYSTEM, SYS_ISDBT as u32),
            dtv_prop_u32(DTV_FREQUENCY, freq_hz as u32),
            dtv_prop_u32(DTV_BANDWIDTH_HZ, 6_000_000),
            dtv_prop_u32(DTV_ISDBT_PARTIAL_RECEPTION, 0),
            dtv_prop_u32(DTV_ISDBT_SOUND_BROADCASTING, 0),
            dtv_prop_u32(DTV_ISDBT_LAYER_ENABLED, ISDBT_ALL_LAYERS),
            dtv_prop(DTV_TUNE),
        ];
        set_properties(fe_fd, &mut tune).map_err(io::Error::from)?;
        self.last_channel_locked.store(false, Ordering::Release);

        // Poll for lock, but deliberately do not fail set_channel if it
        // never locks: of the 50 ISDB-T UHF channels the scanner walks, only
        // a handful are actually broadcasting in any given region, and an
        // empty channel is expected to not lock. Returning an error here
        // would count against scan_scheduler's
        // MAX_CONSECUTIVE_SET_CHANNEL_FAILURES (see
        // recisdb-proxy/src/scheduler/scan_scheduler.rs) and abort the whole
        // scan after 8 empty channels in a row, which happens routinely.
        // Whether this channel actually carries a signal is instead left to
        // get_signal_level() / the caller's MIN_SIGNAL_LEVEL check.
        //
        // Two timeouts rather than one, because both ends matter during a
        // full-band scan. Demodulators can take a couple of seconds to
        // declare FE_HAS_LOCK on a weak-but-real channel, so the overall
        // budget has to be generous. But paying that budget on all ~40 empty
        // channels would add minutes to every scan. An empty channel shows
        // neither FE_HAS_SIGNAL nor FE_HAS_CARRIER, and those bits appear
        // long before full lock, so a channel that is still completely blank
        // after NO_SIGNAL_TIMEOUT_MS is given up on early.
        // NO_SIGNAL_TIMEOUT_MS is generous on purpose. USB demodulators —
        // the Siano parts in particular — can leave every status bit clear
        // for well over a second after DTV_TUNE while their firmware works,
        // so cutting off at a few hundred ms would report "empty channel"
        // for every channel including the real ones.
        //
        // SETTLE_MS exists because FE_READ_STATUS does not become meaningless
        // the instant DTV_TUNE is issued — it keeps reporting the *previous*
        // channel's status until the driver has actually retuned. Reading it
        // right away therefore returns 0x1f (full lock) for a channel that is
        // in fact empty, and the whole scan goes wrong from there: the empty
        // channel is treated as found, get_signal_level reports the old
        // channel's CNR, the demux filter is installed, and the scanner then
        // waits out its entire TS read timeout for a stream that will never
        // arrive. Observed directly on a PX-Q1UD: every other channel logged
        // "locked after 0 ms" and then stalled. Waiting before the first read
        // costs nothing in practice, since a real lock takes ~750-900 ms on
        // this hardware anyway.
        const LOCK_POLL_INTERVAL_MS: u64 = 50;
        const LOCK_TIMEOUT_MS: u64 = 3000;
        const NO_SIGNAL_TIMEOUT_MS: u64 = 1500;
        const SETTLE_MS: u64 = 300;
        std::thread::sleep(std::time::Duration::from_millis(SETTLE_MS));
        let mut waited_ms = SETTLE_MS;
        let mut locked = false;
        let mut last_status = 0u32;
        let mut status_readable = false;
        let mut gave_up_early = false;
        loop {
            let mut status: u32 = 0;
            let status_ok = unsafe { fe_read_status(fe_fd, &mut status as *mut u32) }.is_ok();
            if status_ok {
                last_status = status;
                status_readable = true;
            }
            if status_ok && status & FE_HAS_LOCK != 0 {
                locked = true;
                break;
            }
            if waited_ms >= LOCK_TIMEOUT_MS {
                break;
            }
            if waited_ms >= NO_SIGNAL_TIMEOUT_MS
                && status_ok
                && status & (FE_HAS_SIGNAL | FE_HAS_CARRIER) == 0
            {
                gave_up_early = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(LOCK_POLL_INTERVAL_MS));
            waited_ms += LOCK_POLL_INTERVAL_MS;
        }

        // Start the demux only once the frontend has settled, and only if it
        // locked at all.
        //
        // Order matters here in a way it doesn't for a plain PCI demodulator.
        // Installing the filter is what makes the kernel call the driver's
        // start_feed, and for USB parts that is when the driver asks the
        // device to begin streaming — smsdvb, for instance, sends a PID
        // filter request to the Siano firmware at that moment. Issuing it
        // while the frontend is still hunting can leave the device never
        // actually streaming, which shows up as a channel that locks and
        // then delivers zero bytes forever. libdvbv5 (and therefore
        // recisdb-rs, and dvbv5-zap) tunes, waits for lock, and only then
        // sets the PES filter; this follows the same order.
        //
        // Skipping the filter entirely when there is no lock also avoids
        // leaving a feed running on an empty channel between scan steps.
        self.last_channel_locked.store(locked, Ordering::Release);

        if locked {
            // Full TS tap: pass every PID through to the dvr device.
            let filter = DmxPesFilterParams {
                pid: FULL_TS_PID,
                input: DMX_IN_FRONTEND,
                output: DMX_OUT_TS_TAP,
                pes_type: DMX_PES_OTHER,
                flags: DMX_IMMEDIATE_START,
            };
            unsafe {
                dmx_set_pes_filter(
                    self.demux_fd.as_raw_fd(),
                    &filter as *const DmxPesFilterParams,
                )
            }
            .map_err(io::Error::from)?;
        }

        // Info rather than debug: a scan walks 50 channels and the whole
        // question afterwards is "why did nothing lock". Without the status
        // bits there is no way to distinguish a genuinely empty channel
        // (status stays 0) from a frontend that is never going to tune at
        // all (FE_READ_STATUS unreadable) or one that sees carrier but can't
        // demodulate (signal/carrier set, lock never arrives).
        let reason = if locked {
            "locked"
        } else if !status_readable {
            "FE_READ_STATUS unreadable"
        } else if gave_up_early {
            "no signal/carrier, gave up early"
        } else {
            "timed out waiting for lock"
        };
        info!(
            "DvbV5Tuner: ch={} ({} Hz): {} after {} ms (fe_status=0x{:02x})",
            channel, freq_hz, reason, waited_ms, last_status
        );

        Ok(())
    }

    /// Whether the last `set_channel` saw `FE_HAS_LOCK` before returning.
    pub fn last_channel_locked(&self) -> bool {
        self.last_channel_locked.load(Ordering::Acquire)
    }

    pub fn get_signal_level(&self) -> f32 {
        let fe_fd = self.frontend_fd.as_raw_fd();

        let mut status: u32 = 0;
        if unsafe { fe_read_status(fe_fd, &mut status as *mut u32) }.is_err() {
            return 0.0;
        }
        if status & FE_HAS_LOCK == 0 {
            // Not locked: report "no signal" so callers skip this channel
            // during scan (MIN_SIGNAL_LEVEL gate).
            return 0.0;
        }

        // Locked. Try DTV_STAT_CNR for a real dB reading.
        let mut cnr = [dtv_prop(DTV_STAT_CNR)];
        if get_properties(fe_fd, &mut cnr).is_ok() {
            // struct dtv_fe_stats { __u8 len; struct { __u8 scale; __s64 value; } __packed stat[4]; } __packed
            let buf = &cnr[0].u;
            let len = buf[0];
            if len >= 1 {
                let scale = buf[1];
                // stat[0].value starts at byte offset 2, is a packed (unaligned) i64.
                let value = i64::from_le_bytes(buf[2..10].try_into().unwrap());
                match scale {
                    s if s == FE_SCALE_DECIBEL => {
                        return (value as f64 / 1000.0) as f32;
                    }
                    s if s == FE_SCALE_RELATIVE => {
                        // Not a dB value; fall through to FE_READ_SNR fallback below.
                    }
                    _ => {
                        // FE_SCALE_NOT_AVAILABLE or unknown; fall through.
                    }
                }
            }
        }

        // Fallback: FE_READ_SNR. On smsdvb (Siano, PX-Q1UD's chip) this
        // returns a 0..65535 relative value, not dB. We scale it into an
        // approximate 0..30 dB range purely so it clears the caller's
        // MIN_SIGNAL_LEVEL threshold when a real signal is present; this is
        // NOT a calibrated dB measurement.
        let mut snr: u16 = 0;
        if unsafe { fe_read_snr(fe_fd, &mut snr as *mut u16) }.is_ok() {
            return (snr as f32 / 65535.0) * 30.0;
        }

        // Locked but no readable stat at all: report a conservative
        // above-threshold value rather than 0.0, since the frontend has
        // already told us it's locked (i.e. genuinely receiving), and
        // reporting 0.0 here would cause the scanner to discard a channel
        // it can actually receive.
        warn!("DvbV5Tuner: locked but neither DTV_STAT_CNR nor FE_READ_SNR readable; reporting conservative signal level");
        10.0
    }

    /// Poll for available TS data with a timeout.
    pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        use nix::poll::{poll, PollFd, PollFlags};
        let fd = self.dvr_fd.as_raw_fd();
        // SAFETY: fd is valid for the lifetime of self.
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

    /// Read TS data from the dvr device. Returns (bytes_read, remaining=0).
    ///
    /// The dvr fd is opened O_NONBLOCK, so EAGAIN (no data currently
    /// buffered) is reported as a successful zero-byte read rather than an
    /// error, matching what the reader loop expects (an Err here would abort
    /// the read loop).
    pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
        match nix::unistd::read(self.dvr_fd.as_raw_fd(), buf) {
            Ok(n) => Ok((n, 0)),
            Err(nix::errno::Errno::EAGAIN) => Ok((0, 0)),
            Err(e) => Err(io::Error::from(e)),
        }
    }

    /// Discard buffered TS data (best-effort).
    pub fn purge_ts_stream(&self) {
        use nix::poll::{poll, PollFd, PollFlags};
        let fd = self.dvr_fd.as_raw_fd();
        let mut discard_buf = vec![0u8; 65536];
        for _ in 0..16 {
            let mut fds = [PollFd::new(
                unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
                PollFlags::POLLIN,
            )];
            match poll(&mut fds, 0u16) {
                Ok(n) if n > 0 => {
                    let has_data = fds[0]
                        .revents()
                        .map(|r| r.contains(PollFlags::POLLIN))
                        .unwrap_or(false);
                    if !has_data {
                        break;
                    }
                    match nix::unistd::read(fd, &mut discard_buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
                _ => break,
            }
        }
    }

    /// Enumerate tuning space names. Only GR (ISDB-T) is exposed; see the
    /// struct-level doc comment for why ISDB-S is out of scope.
    pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
        if !self.supports_isdbt {
            return None;
        }
        match space {
            0 => Some("GR".to_string()),
            _ => None,
        }
    }

    /// Enumerate channel names within a tuning space (GR only; UHF ch
    /// 13-62, same naming as `unix.rs::CharDevTuner`).
    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
        if !self.supports_isdbt || space != 0 {
            return None;
        }
        let uhf_ch = channel + 13;
        if uhf_ch <= 62 {
            Some(format!("GR{}", uhf_ch))
        } else {
            None
        }
    }

    /// BonDriver interface version (IBonDriver2: supports
    /// EnumTuningSpace/EnumChannelName).
    pub fn version(&self) -> u8 {
        2
    }
}

impl Drop for DvbV5Tuner {
    fn drop(&mut self) {
        let _ = unsafe { dmx_stop(self.demux_fd.as_raw_fd()) };
        debug!("DvbV5Tuner: DMX_STOP called on drop");
    }
}

/// Parses `/dev/dvb/adapterN` or `/dev/dvb/adapterN/frontendM` into
/// (adapter, frontend) indices.
fn parse_adapter_path(path: &str) -> Result<(u32, u32), io::Error> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "expected /dev/dvb/adapterN or /dev/dvb/adapterN/frontendM, got {:?}",
                path
            ),
        )
    };

    let rest = path.strip_prefix("/dev/dvb/adapter").ok_or_else(invalid)?;
    let (adapter_str, frontend_str) = match rest.split_once('/') {
        Some((a, tail)) => {
            let frontend_str = tail.strip_prefix("frontend").ok_or_else(invalid)?;
            (a, frontend_str)
        }
        None => (rest, "0"),
    };

    let adapter: u32 = adapter_str.parse().map_err(|_| invalid())?;
    let frontend: u32 = frontend_str.parse().map_err(|_| invalid())?;
    Ok((adapter, frontend))
}

/// Reads DTV_ENUM_DELSYS off the frontend and checks whether SYS_ISDBT is
/// among the reported delivery systems. Returns `true` (conservative
/// default: assume ISDB-T support) if the ioctl itself fails.
fn detect_isdbt_support(fd: RawFd) -> bool {
    let mut delsys = [dtv_prop(DTV_ENUM_DELSYS)];
    if get_properties(fd, &mut delsys).is_err() {
        warn!("DTV_ENUM_DELSYS failed; assuming ISDB-T support (conservative default)");
        return true;
    }
    // struct dtv_property_buffer { __u8 data[32]; __u32 len; ... } — data is
    // the delsys array, len (u32 LE) at offset 32 is the element count.
    let buf = &delsys[0].u;
    let len = u32::from_le_bytes(buf[32..36].try_into().unwrap()).min(32) as usize;
    buf[..len].contains(&SYS_ISDBT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtv_property_is_76_bytes_packed() {
        assert_eq!(std::mem::size_of::<DtvProperty>(), 76);
    }

    #[test]
    fn parses_bare_adapter_path() {
        assert_eq!(parse_adapter_path("/dev/dvb/adapter0").unwrap(), (0, 0));
        assert_eq!(parse_adapter_path("/dev/dvb/adapter3").unwrap(), (3, 0));
    }

    #[test]
    fn parses_adapter_with_explicit_frontend() {
        assert_eq!(
            parse_adapter_path("/dev/dvb/adapter0/frontend1").unwrap(),
            (0, 1)
        );
    }

    #[test]
    fn rejects_non_dvb_paths() {
        assert!(parse_adapter_path("/dev/px4video0").is_err());
        assert!(parse_adapter_path("/dev/dvb/adapter0/demux0").is_err());
        assert!(parse_adapter_path("/dev/dvb/adapterX").is_err());
    }

    #[test]
    fn dtv_prop_u32_writes_little_endian_at_front_of_union() {
        let p = dtv_prop_u32(DTV_FREQUENCY, 0x1234_5678);
        let cmd = p.cmd; // copy out: packed struct fields can't be borrowed directly
        assert_eq!(cmd, DTV_FREQUENCY);
        assert_eq!(&p.u[0..4], &0x1234_5678u32.to_le_bytes());
    }

    #[test]
    fn delsys_detection_reads_len_and_data_correctly() {
        let mut prop = dtv_prop(DTV_ENUM_DELSYS);
        // Simulate a kernel response reporting {SYS_ISDBT} as the only
        // supported delivery system (len=1, data[0]=SYS_ISDBT).
        prop.u[0] = SYS_ISDBT;
        prop.u[32..36].copy_from_slice(&1u32.to_le_bytes());
        let buf = &prop.u;
        let len = u32::from_le_bytes(buf[32..36].try_into().unwrap()).min(32) as usize;
        assert_eq!(len, 1);
        assert!(buf[..len].contains(&SYS_ISDBT));
        assert!(!buf[..len].contains(&SYS_ISDBS));
    }
}
