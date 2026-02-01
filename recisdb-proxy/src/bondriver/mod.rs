//! BonDriver wrapper module for direct tuner access on Windows.
//!
//! This module provides direct access to BonDriver DLLs for channel scanning.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(target_os = "windows"))]
mod stub {
    //! Stub implementation for non-Windows platforms.

    use std::io;

    pub struct BonDriverTuner;

    impl BonDriverTuner {
        pub fn new(_path: &str) -> Result<Self, io::Error> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "BonDriver is only supported on Windows",
            ))
        }

        pub fn set_channel(&self, _space: u32, _channel: u32) -> Result<(), io::Error> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "BonDriver is only supported on Windows",
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
                "BonDriver is only supported on Windows",
            ))
        }

        pub fn enum_tuning_space(&self, _space: u32) -> Option<String> {
            None
        }

        pub fn enum_channel_name(&self, _space: u32, _channel: u32) -> Option<String> {
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::*;
