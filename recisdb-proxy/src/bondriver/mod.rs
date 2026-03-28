//! BonDriver wrapper module.
//!
//! On Windows: wraps BonDriver DLLs via FFI.
//! On Linux: wraps character devices (/dev/px4video*, etc.) via ioctl.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
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
            0
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub use stub::*;
