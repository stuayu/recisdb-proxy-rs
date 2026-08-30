//! Tuner backend wrapper module.
//!
//! One `BonDriverTuner` type, four ways of reaching hardware:
//!
//! - **Windows**: BonDriver DLLs via FFI (`windows.rs`).
//! - **Unix, character device**: `/dev/px4videoN`, `/dev/pt3videoN`, … via
//!   `ioctl` (`unix.rs`). This is how px4_drv/pt3-drv present themselves on
//!   Linux.
//! - **Unix, px4_drv daemon**: `px4daemon:<index>` (`px4_daemon.rs`). macOS
//!   cannot create `/dev/*` nodes without a kernel extension, so px4_drv's
//!   macOS port runs as a user-space daemon reachable over UNIX domain
//!   sockets instead. See that module's doc comment.
//! - **Linux, DVB API**: `/dev/dvb/adapterN[/frontendM]` via raw DVBv5
//!   ioctls (`dvbv5.rs`, Linux-only). This is the path for kernel-standard
//!   DVB drivers (e.g. `smsdvb` for Siano-based tuners like the PX-Q1UD)
//!   that never register a `/dev/px4videoN`-style node at all. See that
//!   module's doc comment for why it doesn't reuse `libdvbv5`.
//!
//! On Unix the backend is chosen by the tuner path itself: `px4daemon:0`
//! selects the daemon, `/dev/dvb/...` selects the DVB API backend (Linux
//! only), and anything else is treated as a px4-drv/pt3-drv device node.
//! Nothing above this module needs to know which one it got.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(target_os = "linux")]
mod dvbv5;
#[cfg(unix)]
mod px4_daemon;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use px4_daemon::PATH_PREFIX as PX4_DAEMON_PATH_PREFIX;

#[cfg(unix)]
mod dispatch {
    use std::io;

    #[cfg(target_os = "linux")]
    use super::dvbv5::DvbV5Tuner;
    use super::px4_daemon::{Px4DaemonTuner, PATH_PREFIX};
    use super::unix::CharDevTuner;

    /// Path prefix that selects the Linux DVB API backend
    /// (`/dev/dvb/adapterN[/frontendM]`) instead of the px4-drv/pt3-drv
    /// chardev backend.
    const DVB_PATH_PREFIX: &str = "/dev/dvb/";

    /// A tuner opened through whichever Unix backend its path selects.
    ///
    /// Deliberately an enum rather than a `Box<dyn Tuner>`: the reader loop
    /// calls `get_ts_stream` once per chunk on the hot path, and neither
    /// backend is `Send` in a way that would let the loop move off its
    /// `spawn_blocking` thread anyway, so there is nothing to gain from a
    /// vtable here.
    pub enum BonDriverTuner {
        /// `/dev/px4videoN` and friends (Linux kernel driver).
        CharDev(CharDevTuner),
        /// px4_drv's user-space daemon (macOS).
        Px4Daemon(Px4DaemonTuner),
        /// `/dev/dvb/adapterN[/frontendM]` (Linux kernel-standard DVB API).
        #[cfg(target_os = "linux")]
        DvbV5(DvbV5Tuner),
    }

    impl BonDriverTuner {
        pub fn new(path: &str) -> Result<Self, io::Error> {
            if path.starts_with(PATH_PREFIX) {
                Ok(BonDriverTuner::Px4Daemon(Px4DaemonTuner::new(path)?))
            } else {
                #[cfg(target_os = "linux")]
                if path.starts_with(DVB_PATH_PREFIX) {
                    return Ok(BonDriverTuner::DvbV5(DvbV5Tuner::new(path)?));
                }
                Ok(BonDriverTuner::CharDev(CharDevTuner::new(path)?))
            }
        }

        pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
            match self {
                BonDriverTuner::CharDev(t) => t.set_channel(space, channel),
                BonDriverTuner::Px4Daemon(t) => t.set_channel(space, channel),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.set_channel(space, channel),
            }
        }

        pub fn get_signal_level(&self) -> f32 {
            match self {
                BonDriverTuner::CharDev(t) => t.get_signal_level(),
                BonDriverTuner::Px4Daemon(t) => t.get_signal_level(),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.get_signal_level(),
            }
        }

        /// Whether the last `set_channel` is known to have reached signal
        /// lock. `None` means the backend cannot report it — callers must
        /// then assume a channel may be receivable.
        pub fn last_channel_locked(&self) -> Option<bool> {
            match self {
                BonDriverTuner::CharDev(_) => None,
                BonDriverTuner::Px4Daemon(_) => None,
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => Some(t.last_channel_locked()),
            }
        }

        pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
            match self {
                BonDriverTuner::CharDev(t) => t.wait_ts_stream(timeout_ms),
                BonDriverTuner::Px4Daemon(t) => t.wait_ts_stream(timeout_ms),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.wait_ts_stream(timeout_ms),
            }
        }

        pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
            match self {
                BonDriverTuner::CharDev(t) => t.get_ts_stream(buf),
                BonDriverTuner::Px4Daemon(t) => t.get_ts_stream(buf),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.get_ts_stream(buf),
            }
        }

        /// Windows-only native read diagnostics. Other backends have no
        /// caller-buffer carry-over and return `None`.
        pub fn get_ts_stream_stats(&self) -> Option<(u64, u64, u64, u64)> {
            match self {
                BonDriverTuner::CharDev(_) => None,
                BonDriverTuner::Px4Daemon(_) => None,
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(_) => None,
            }
        }

        pub fn purge_ts_stream(&self) {
            match self {
                BonDriverTuner::CharDev(t) => t.purge_ts_stream(),
                BonDriverTuner::Px4Daemon(t) => t.purge_ts_stream(),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.purge_ts_stream(),
            }
        }

        pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
            match self {
                BonDriverTuner::CharDev(t) => t.enum_tuning_space(space),
                BonDriverTuner::Px4Daemon(t) => t.enum_tuning_space(space),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.enum_tuning_space(space),
            }
        }

        pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
            match self {
                BonDriverTuner::CharDev(t) => t.enum_channel_name(space, channel),
                BonDriverTuner::Px4Daemon(t) => t.enum_channel_name(space, channel),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.enum_channel_name(space, channel),
            }
        }

        pub fn version(&self) -> u8 {
            match self {
                BonDriverTuner::CharDev(t) => t.version(),
                BonDriverTuner::Px4Daemon(t) => t.version(),
                #[cfg(target_os = "linux")]
                BonDriverTuner::DvbV5(t) => t.version(),
            }
        }
    }
}

#[cfg(unix)]
pub use dispatch::BonDriverTuner;

#[cfg(not(any(target_os = "windows", unix)))]
mod stub {
    //! Stub implementation for unsupported platforms.

    use std::io;

    pub struct BonDriverTuner;

    impl BonDriverTuner {
        pub fn new(_path: &str) -> Result<Self, io::Error> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "BonDriver/chardev tuner is only supported on Windows and Linux",
            ))
        }

        pub fn set_channel(&self, _space: u32, _channel: u32) -> Result<(), io::Error> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "BonDriver/chardev tuner is only supported on Windows and Linux",
            ))
        }

        pub fn get_signal_level(&self) -> f32 {
            0.0
        }

        pub fn last_channel_locked(&self) -> Option<bool> {
            None
        }

        pub fn wait_ts_stream(&self, _timeout_ms: u32) -> bool {
            false
        }

        pub fn get_ts_stream(&self, _buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "BonDriver/chardev tuner is only supported on Windows and Linux",
            ))
        }

        pub fn get_ts_stream_stats(&self) -> Option<(u64, u64, u64, u64)> {
            None
        }

        pub fn purge_ts_stream(&self) {}

        pub fn enum_tuning_space(&self, _space: u32) -> Option<String> {
            None
        }

        pub fn enum_channel_name(&self, _space: u32, _channel: u32) -> Option<String> {
            None
        }

        pub fn version(&self) -> u8 {
            2
        }
    }
}

#[cfg(not(any(target_os = "windows", unix)))]
pub use stub::*;
