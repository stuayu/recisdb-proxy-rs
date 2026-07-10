//! Shared TS constants.
//!
//! The minimal passive-scan TS parser that once lived here has been removed
//! along with the (dead) passive scanner. These constants are still used by
//! [`crate::tuner::ts_analyzer`].

/// TS packet size.
pub const TS_PACKET_SIZE: usize = 188;
/// TS sync byte.
pub const SYNC_BYTE: u8 = 0x47;
