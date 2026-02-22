use crate::channels::Channel;

#[cfg(target_os = "linux")]
pub use self::linux::{Tuner, UnTunedTuner};
#[cfg(target_os = "windows")]
pub use self::windows::{Tuner, UnTunedTuner};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub use self::unsupported::{Tuner, UnTunedTuner};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;

mod error;

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Voltage {
    _11v,
    _15v,
    Low,
}

pub trait Tunable {
    fn tune(self, ch: Channel, lnb: Option<Voltage>) -> Result<Tuner, std::io::Error>;
}
