//! Tuner backend wrapper module.
//!
//! One `BonDriverTuner` type, three ways of reaching hardware:
//!
//! - **Windows**: BonDriver DLLs via FFI (`windows.rs`).
//! - **Unix, character device**: `/dev/px4videoN`, `/dev/pt3videoN`, … via
//!   `ioctl` (`unix.rs`). This is how px4_drv/pt3-drv present themselves on
//!   Linux.
//! - **Unix, px4_drv daemon**: `px4daemon:<index>` (`px4_daemon.rs`). macOS
//!   cannot create `/dev/*` nodes without a kernel extension, so px4_drv's
//!   macOS port runs as a user-space daemon reachable over UNIX domain
//!   sockets instead. See that module's doc comment.
//!
//! On Unix the backend is chosen by the tuner path itself, so a
//! `bon_drivers.dll_path` of `px4daemon:0` selects the daemon and anything
//! else is treated as a device node. Nothing above this module needs to know
//! which one it got.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(unix)]
mod px4_daemon;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use px4_daemon::PATH_PREFIX as PX4_DAEMON_PATH_PREFIX;

#[cfg(unix)]
mod dispatch {
    use std::io;

    use super::px4_daemon::{Px4DaemonTuner, PATH_PREFIX};
    use super::unix::CharDevTuner;

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
    }

    impl BonDriverTuner {
        pub fn new(path: &str) -> Result<Self, io::Error> {
            if path.starts_with(PATH_PREFIX) {
                Ok(BonDriverTuner::Px4Daemon(Px4DaemonTuner::new(path)?))
            } else {
                Ok(BonDriverTuner::CharDev(CharDevTuner::new(path)?))
            }
        }

        pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
            match self {
                BonDriverTuner::CharDev(t) => t.set_channel(space, channel),
                BonDriverTuner::Px4Daemon(t) => t.set_channel(space, channel),
            }
        }

        pub fn get_signal_level(&self) -> f32 {
            match self {
                BonDriverTuner::CharDev(t) => t.get_signal_level(),
                BonDriverTuner::Px4Daemon(t) => t.get_signal_level(),
            }
        }

        pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
            match self {
                BonDriverTuner::CharDev(t) => t.wait_ts_stream(timeout_ms),
                BonDriverTuner::Px4Daemon(t) => t.wait_ts_stream(timeout_ms),
            }
        }

        pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
            match self {
                BonDriverTuner::CharDev(t) => t.get_ts_stream(buf),
                BonDriverTuner::Px4Daemon(t) => t.get_ts_stream(buf),
            }
        }

        pub fn purge_ts_stream(&self) {
            match self {
                BonDriverTuner::CharDev(t) => t.purge_ts_stream(),
                BonDriverTuner::Px4Daemon(t) => t.purge_ts_stream(),
            }
        }

        pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
            match self {
                BonDriverTuner::CharDev(t) => t.enum_tuning_space(space),
                BonDriverTuner::Px4Daemon(t) => t.enum_tuning_space(space),
            }
        }

        pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
            match self {
                BonDriverTuner::CharDev(t) => t.enum_channel_name(space, channel),
                BonDriverTuner::Px4Daemon(t) => t.enum_channel_name(space, channel),
            }
        }

        pub fn version(&self) -> u8 {
            match self {
                BonDriverTuner::CharDev(t) => t.version(),
                BonDriverTuner::Px4Daemon(t) => t.version(),
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

        pub fn wait_ts_stream(&self, _timeout_ms: u32) -> bool {
            false
        }

        pub fn get_ts_stream(&self, _buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "BonDriver/chardev tuner is only supported on Windows and Linux",
            ))
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
