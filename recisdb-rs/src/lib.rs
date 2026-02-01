//! recisdb library - TV tuner access for ISDB-T/ISDB-S
//!
//! This library provides access to TV tuners via BonDriver (Windows) or
//! character devices (Linux).

pub mod channels;
pub mod tuner;

// Re-export commonly used types
pub use channels::Channel;
pub use channels::representation::{ChannelSpace, ChannelType};
pub use tuner::{Tunable, Tuner, UnTunedTuner, Voltage};
