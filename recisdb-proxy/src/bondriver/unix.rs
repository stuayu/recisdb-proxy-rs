//! Unix character device implementation of BonDriverTuner.
//!
//! Supports physical tuners at /dev/px4video*, /dev/pt3video*, etc.
//! Uses ioctl interface compatible with px4-drv and pt3-drv kernel drivers.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use log::{debug, warn};

/// ioctl payload for channel selection (matches struct ptx_freq in px4-drv).
#[repr(C)]
struct IoctlFreq {
    ch: i32,
    slot: i32,
}

nix::ioctl_write_ptr!(set_ch, 0x8d, 0x01, IoctlFreq);
nix::ioctl_none!(start_rec, 0x8d, 0x02);
nix::ioctl_none!(stop_rec, 0x8d, 0x03);
nix::ioctl_read!(ptx_get_cnr, 0x8d, 0x04, i64);
nix::ioctl_write_int!(ptx_enable_lnb, 0x8d, 0x05);
nix::ioctl_none!(ptx_disable_lnb, 0x8d, 0x06);
nix::ioctl_write_int!(ptx_set_sys_mode, 0x8d, 0x0b);

/// Converts BonDriver (space, channel) indices to IoctlFreq for px4-drv/pt3-drv.
///
/// BonDriver space/channel → actual channel number → IoctlFreq:
/// - space=0 (GR/Terrestrial): channel 0..49 → UHF ch 13..62 → ioctl ch = uhf_ch + 50
/// - space=1 (BS): channel 0..11 → BS ch 1,3,...,23 → ioctl ch = bs_ch / 2, slot = -1 (AsIs)
/// - space=2 (CS): channel 0..11 → CS ch 2,4,...,24 → ioctl ch = cs_ch / 2 + 11, slot = 0
///
/// These formulas match `channels.rs` in recisdb-rs (IoctlFreq::from(ChannelType)).
/// Reference test: T18 → ch=68 (= 18 + 50), confirmed in recisdb-rs/src/channels.rs.
fn space_channel_to_ioctl_freq(space: u32, channel: u32) -> Result<IoctlFreq, io::Error> {
    match space {
        0 => {
            // Terrestrial (GR): BonDriver channel index 0..49 → UHF ch 13..62
            // px4-drv PTX_SET_CHANNEL expects: ch = uhf_channel_number + 50
            //   channel=0 → UHF 13 → ch=63, channel=49 → UHF 62 → ch=112
            // (confirmed by recisdb-rs channels.rs test: T18 → ch=68 = 18+50)
            if channel > 49 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("GR channel {} out of range (0-49)", channel),
                ));
            }
            let uhf_ch = (channel + 13) as i32; // UHF channel number 13..62
            Ok(IoctlFreq {
                ch: uhf_ch + 50, // px4-drv formula: uhf_ch + 50
                slot: 0,
            })
        }
        1 => {
            // BS: BonDriver channel index → BS1, BS3, ..., BS23 (odd numbers)
            if channel > 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("BS channel {} out of range (0-11)", channel),
                ));
            }
            let bs_ch = (channel * 2 + 1) as i32; // 1, 3, 5, ..., 23
            Ok(IoctlFreq {
                ch: bs_ch / 2,  // px4-drv: ch_num / 2
                slot: -1,       // -1 = AsIs (pass all TS streams)
            })
        }
        2 => {
            // CS: BonDriver channel index → CS2, CS4, ..., CS24 (even numbers)
            if channel > 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("CS channel {} out of range (0-11)", channel),
                ));
            }
            let cs_ch = (channel * 2 + 2) as i32; // 2, 4, 6, ..., 24
            Ok(IoctlFreq {
                ch: cs_ch / 2 + 11, // px4-drv: ch_num / 2 + 11
                slot: 0,
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unknown tuning space: {}", space),
        )),
    }
}

/// BonDriver-compatible wrapper for Unix character device tuners.
///
/// Provides the same interface as the Windows BonDriverTuner to allow
/// transparent usage in recisdb-proxy on Unix systems.
pub struct BonDriverTuner {
    /// File handle for TS data reading.
    file: File,
    /// Duplicated fd for ioctl operations (avoids borrowing conflicts with reader).
    /// Using try_clone() (dup) is safe for Linux device files and allows
    /// concurrent read + ioctl from different threads.
    ioctl_file: File,
    /// Whether recording (streaming) has been started.
    recording: AtomicBool,
    /// Current tuning space (0=GR, 1=BS, 2=CS) for signal level conversion.
    current_space: AtomicI32,
}

impl BonDriverTuner {
    pub fn new(path: &str) -> Result<Self, io::Error> {
        // Canonicalize to resolve symlinks (e.g. /dev/px4video0 → real device node)
        let path = std::fs::canonicalize(path)?;
        let file = OpenOptions::new().read(true).open(&path)?;
        let ioctl_file = file.try_clone()?;
        Ok(Self {
            file,
            ioctl_file,
            recording: AtomicBool::new(false),
            current_space: AtomicI32::new(0),
        })
    }

    pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
        // Stop recording before re-tuning if already active
        if self.recording.load(Ordering::Acquire) {
            unsafe {
                let _ = stop_rec(self.ioctl_file.as_raw_fd());
            }
            self.recording.store(false, Ordering::Release);
        }

        let freq = space_channel_to_ioctl_freq(space, channel)?;

        // Select system mode before set_ch.
        // Some px4-drv devices require explicit ISDB-T(0)/ISDB-S(1) selection.
        // Ignore errors — older drivers that don't support this ioctl return EINVAL.
        match space {
            0 => {
                let _ = unsafe { ptx_set_sys_mode(self.ioctl_file.as_raw_fd(), 0) }; // ISDB-T
            }
            _ => {
                let _ = unsafe { ptx_set_sys_mode(self.ioctl_file.as_raw_fd(), 1) }; // ISDB-S
            }
        }

        // LNB must be powered BEFORE set_ch for satellite bands.
        // px4-drv's PTX_SET_CHANNEL blocks internally waiting for PLL lock (~5s).
        // If LNB is not yet powered when set_ch starts that wait, the dish has no
        // power and the lock fails with EAGAIN.  Powering LNB first gives the
        // satellite LNB time to stabilize before the driver starts scanning.
        match space {
            1 | 2 => {
                // BS/CS: enable LNB voltage (11V) before set_ch
                let _ = unsafe { ptx_enable_lnb(self.ioctl_file.as_raw_fd(), 1) };
            }
            _ => {
                // Terrestrial: disable LNB
                let _ = unsafe { ptx_disable_lnb(self.ioctl_file.as_raw_fd()) };
            }
        }

        unsafe {
            set_ch(self.ioctl_file.as_raw_fd(), &freq).map_err(io::Error::from)?;
        }

        unsafe {
            start_rec(self.ioctl_file.as_raw_fd()).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("start_rec ioctl failed (space={}, ch={}): {}", space, channel, e),
                )
            })?;
        }

        self.recording.store(true, Ordering::Release);
        self.current_space.store(space as i32, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_signal_level(&self) -> f32 {
        let mut raw: i64 = 0;
        let result = unsafe { ptx_get_cnr(self.ioctl_file.as_raw_fd(), &mut raw) };
        if result.is_err() {
            warn!("ptx_get_cnr ioctl failed: {:?}", result);
            return 0.0;
        }

        let space = self.current_space.load(Ordering::Relaxed);
        if space == 0 {
            // Terrestrial (GR): CNR → C/N dB (px4-drv formula, matches recisdb-rs)
            let p = (5505024.0_f64 / (raw as f64)).log10() * 10.0;
            let cn = (0.000024 * p * p * p * p)
                - (0.0016 * p * p * p)
                + (0.0398 * p * p)
                + (0.5491 * p)
                + 3.0965;
            cn as f32
        } else {
            // BS/CS: AF level table linear interpolation (matches recisdb-rs)
            const AF_LEVEL_TABLE: [f64; 14] = [
                24.07, // 0x00 → 24.07 dB
                24.07, // 0x10 → 24.07 dB
                18.61, // 0x20 → 18.61 dB
                15.21, // 0x30 → 15.21 dB
                12.50, // 0x40 → 12.50 dB
                10.19, // 0x50 → 10.19 dB
                8.140, // 0x60 →  8.14 dB
                6.270, // 0x70 →  6.27 dB
                4.550, // 0x80 →  4.55 dB
                3.730, // 0x88 →  3.73 dB
                3.630, // 0x88FF → 3.63 dB
                2.940, // 0x90 →  2.94 dB
                1.420, // 0xA0 →  1.42 dB
                0.000, // 0xB0 →  0.00 dB
            ];
            let sig = ((raw & 0xFF00) >> 8) as u8;
            if sig <= 0x10 {
                24.07_f32
            } else if sig >= 0xB0 {
                0.0_f32
            } else {
                let f_mix_rate =
                    (((sig as u16 & 0x0F) << 8) | sig as u16) as f64 / 4096.0;
                let idx = (sig >> 4) as usize;
                (AF_LEVEL_TABLE[idx] * (1.0 - f_mix_rate)
                    + AF_LEVEL_TABLE[idx + 1] * f_mix_rate) as f32
            }
        }
    }

    /// Poll for available TS data with a timeout.
    pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        use nix::poll::{poll, PollFd, PollFlags};
        let fd = self.file.as_raw_fd();
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

    /// Read TS data from the device. Returns (bytes_read, remaining=0).
    pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
        let n = nix::unistd::read(self.file.as_raw_fd(), buf)
            .map_err(io::Error::from)?;
        Ok((n, 0))
    }

    /// Discard buffered TS data (best-effort).
    pub fn purge_ts_stream(&self) {
        use nix::poll::{poll, PollFd, PollFlags};
        let fd = self.file.as_raw_fd();
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

    /// Enumerate tuning space names.
    pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
        match space {
            0 => Some("GR".to_string()),
            1 => Some("BS".to_string()),
            2 => Some("CS".to_string()),
            _ => None,
        }
    }

    /// Enumerate channel names within a tuning space.
    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
        match space {
            0 => {
                // GR: UHF ch 13-62 (50 channels)
                let uhf_ch = channel + 13;
                if uhf_ch <= 62 {
                    Some(format!("GR{}", uhf_ch))
                } else {
                    None
                }
            }
            1 => {
                // BS: BS1, BS3, ..., BS23 (12 channels)
                if channel <= 11 {
                    Some(format!("BS{}", channel * 2 + 1))
                } else {
                    None
                }
            }
            2 => {
                // CS: CS2, CS4, ..., CS24 (12 channels)
                if channel <= 11 {
                    Some(format!("CS{}", channel * 2 + 2))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// BonDriver interface version (IBonDriver2: supports EnumTuningSpace/EnumChannelName).
    pub fn version(&self) -> u8 {
        2
    }
}

impl Drop for BonDriverTuner {
    fn drop(&mut self) {
        if self.recording.load(Ordering::Acquire) {
            // Disable LNB first (matches recisdb-rs PowerOffHandle drop order),
            // then stop recording.
            let space = self.current_space.load(Ordering::Relaxed);
            if space == 1 || space == 2 {
                let _ = unsafe { ptx_disable_lnb(self.ioctl_file.as_raw_fd()) };
            }
            let _ = unsafe { stop_rec(self.ioctl_file.as_raw_fd()) };
            debug!("UnixChardevTuner: stop_rec called on drop");
        }
    }
}
