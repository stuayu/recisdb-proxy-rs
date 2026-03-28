//! Linux character device implementation of BonDriverTuner.
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

/// Converts BonDriver (space, channel) indices to IoctlFreq for px4-drv/pt3-drv.
///
/// Mapping:
/// - space=0 (GR/Terrestrial): channel 0..49 → UHF ch 13..62
/// - space=1 (BS): channel 0..11 → BS ch 1,3,5,...,23
/// - space=2 (CS): channel 0..11 → CS ch 2,4,6,...,24
fn space_channel_to_ioctl_freq(space: u32, channel: u32) -> Result<IoctlFreq, io::Error> {
    match space {
        0 => {
            // Terrestrial (GR): UHF channel 13-62
            let uhf_ch = (channel as i32) + 13;
            if !(13..=62).contains(&uhf_ch) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("GR channel {} out of range (0-49)", channel),
                ));
            }
            Ok(IoctlFreq {
                ch: uhf_ch + 50, // px4-drv formula: ch_num + 50
                slot: 0,
            })
        }
        1 => {
            // BS: BS1, BS3, BS5, ..., BS23 (odd numbers 1-23)
            if channel > 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("BS channel {} out of range (0-11)", channel),
                ));
            }
            let bs_ch = (channel * 2 + 1) as i32; // 1, 3, 5, ..., 23
            Ok(IoctlFreq {
                ch: bs_ch / 2,  // px4-drv formula: ch_num / 2
                slot: -1,       // -1 = AsIs (all TS streams)
            })
        }
        2 => {
            // CS: CS2, CS4, CS6, ..., CS24 (even numbers 2-24)
            if channel > 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("CS channel {} out of range (0-11)", channel),
                ));
            }
            let cs_ch = (channel * 2 + 2) as i32; // 2, 4, 6, ..., 24
            Ok(IoctlFreq {
                ch: cs_ch / 2 + 11, // px4-drv formula: ch_num / 2 + 11
                slot: 0,
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unknown tuning space: {}", space),
        )),
    }
}

/// BonDriver-compatible wrapper for Linux character device tuners.
///
/// Provides the same interface as the Windows BonDriverTuner to allow
/// transparent usage in recisdb-proxy on Linux.
pub struct BonDriverTuner {
    /// File handle for TS data reading.
    file: File,
    /// Duplicated fd for ioctl operations (avoids borrowing conflicts).
    ioctl_file: File,
    /// Whether recording (streaming) has been started.
    recording: AtomicBool,
    /// Current tuning space (0=GR, 1=BS, 2=CS) for signal level conversion.
    current_space: AtomicI32,
}

impl BonDriverTuner {
    pub fn new(path: &str) -> Result<Self, io::Error> {
        let file = OpenOptions::new().read(true).open(path)?;
        let ioctl_file = file.try_clone()?;
        Ok(Self {
            file,
            ioctl_file,
            recording: AtomicBool::new(false),
            current_space: AtomicI32::new(0),
        })
    }

    pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
        // Stop recording if already active before re-tuning
        if self.recording.load(Ordering::Acquire) {
            unsafe {
                let _ = stop_rec(self.ioctl_file.as_raw_fd());
            }
            self.recording.store(false, Ordering::Release);
        }

        let freq = space_channel_to_ioctl_freq(space, channel)?;

        // Set LNB for BS/CS
        match space {
            1 | 2 => {
                // Enable LNB voltage for satellite (11V)
                let _ = unsafe { ptx_enable_lnb(self.ioctl_file.as_raw_fd(), 1) };
            }
            _ => {
                let _ = unsafe { ptx_disable_lnb(self.ioctl_file.as_raw_fd()) };
            }
        }

        unsafe {
            set_ch(self.ioctl_file.as_raw_fd(), &freq).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("set_ch ioctl failed: {}", e))
            })?;
        }

        unsafe {
            start_rec(self.ioctl_file.as_raw_fd()).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("start_rec ioctl failed: {}", e))
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
            // Terrestrial (GR) CNR to C/N dB conversion (px4-drv formula)
            let p = (5505024.0_f64 / (raw as f64)).log10() * 10.0;
            let cn = (0.000024 * p * p * p * p)
                - (0.0016 * p * p * p)
                + (0.0398 * p * p)
                + (0.5491 * p)
                + 3.0965;
            cn as f32
        } else {
            // BS/CS: AF level table interpolation
            const AF_LEVEL_TABLE: [f64; 14] = [
                24.07, 24.07, 18.61, 15.21, 12.50, 10.19, 8.140,
                6.270, 4.550, 3.730, 3.630, 2.940, 1.420, 0.000,
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
        let mut fds = [PollFd::new(unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) }, PollFlags::POLLIN)];
        match poll(&mut fds, timeout_ms as i32) {
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
        // Poll with 0 timeout and drain any available data
        for _ in 0..16 {
            let mut fds = [PollFd::new(
                unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
                PollFlags::POLLIN,
            )];
            match poll(&mut fds, 0) {
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
    /// Returns predefined space names for ISDB-T/S channel mapping.
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

    /// BonDriver interface version (1 for IBonDriver compatibility).
    pub fn version(&self) -> u8 {
        2 // Reports as IBonDriver2 (supports EnumTuningSpace/EnumChannelName)
    }
}

impl Drop for BonDriverTuner {
    fn drop(&mut self) {
        if self.recording.load(Ordering::Acquire) {
            let space = self.current_space.load(Ordering::Relaxed);
            if space == 1 || space == 2 {
                let _ = unsafe { ptx_disable_lnb(self.ioctl_file.as_raw_fd()) };
            }
            let _ = unsafe { stop_rec(self.ioctl_file.as_raw_fd()) };
            debug!("LinuxChardevTuner: stop_rec called on drop");
        }
    }
}
