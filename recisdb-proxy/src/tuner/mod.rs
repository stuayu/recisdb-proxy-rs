//! Tuner management for the proxy server.
//!
//! This module provides:
//! - [`TunerPool`]: Pool of shared tuner instances with channel sharing
//! - [`SharedTuner`]: Wrapper for tuner with broadcast capability
//! - [`TunerLock`]: Exclusive/shared lock mechanism
//! - [`TunerSelector`]: Intelligent tuner selection with fallback
//! - [`passive_scanner`]: Real-time channel info updates during streaming
//! - [`space_generator`]: Automatic virtual space generation from channels
//! - [`group_space`]: Group-based aggregation and driver selection

pub mod channel_key;
pub mod lock;
pub mod passive_scanner;
pub mod pool;
pub mod selector;
pub mod shared;
pub mod ts_parser;
pub mod ts_analyzer;
pub mod b25_pipe;
pub mod space_generator;
pub mod group_space;
pub mod quality_scorer;
pub mod warm;

pub use channel_key::ChannelKey;
#[allow(unused_imports)]
pub use lock::{ExclusiveLockGuard, LockError, SharedLockGuard, TunerLock};
pub use pool::{TunerPool, TunerPoolConfig};
#[allow(unused_imports)]
pub use selector::{ChannelCandidate, FallbackResult, SelectError, TuneError, TunerSelector};
pub use shared::SharedTuner;
pub use warm::WarmTunerHandle;
pub use space_generator::{SpaceGenerator, SpaceMapping, ChannelInfo as SpaceGenChannelInfo};
pub use group_space::{GroupSpaceInfo, DriverInfo, DriverSelector, DriverSelectionStrategy};
pub use quality_scorer::{BonDriverWithScore, QualityScorer};
