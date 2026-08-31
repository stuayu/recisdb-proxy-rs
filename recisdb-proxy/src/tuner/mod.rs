//! Tuner management for the proxy server.
//!
//! This module provides:
//! - [`TunerPool`]: Pool of shared tuner instances with channel sharing
//! - [`SharedTuner`]: Wrapper for tuner with broadcast capability

pub(crate) mod acquire;
pub mod b25_pipe;
pub mod channel_key;
pub mod claim;
pub mod encoder_pool;
pub mod epg_collector;
pub mod lock;
pub mod logo_collector;
pub mod mmt_pipe;
pub mod nit_collector;
pub(crate) mod open_backoff;
pub mod policy;
pub mod pool;
pub mod quality_scorer;
pub mod shared;
pub(crate) mod timing;
pub mod ts_analyzer;
pub mod ts_parser;
pub(crate) mod ts_source;
pub mod warm;

pub use channel_key::ChannelKey;
pub use claim::EffectiveClaim;
pub use encoder_pool::{
    EncodeKey, EncoderPool, EncoderPoolError, EncoderRuntimeConfig, SharedEncoder,
};
pub use pool::{CarriedSlotPermit, ScanReservation, SlotPermit, TunerPool, TunerPoolConfig};
pub use quality_scorer::{BonDriverWithScore, QualityScorer};
pub use shared::{ReaderState, SharedTuner, TunerSubscription};
pub use warm::WarmTunerHandle;
