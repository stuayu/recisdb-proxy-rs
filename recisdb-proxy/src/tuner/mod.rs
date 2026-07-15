//! Tuner management for the proxy server.
//!
//! This module provides:
//! - [`TunerPool`]: Pool of shared tuner instances with channel sharing
//! - [`SharedTuner`]: Wrapper for tuner with broadcast capability

pub mod channel_key;
pub mod encoder_pool;
pub mod lock;
pub mod pool;
pub mod shared;
pub mod ts_parser;
pub mod ts_analyzer;
pub mod b25_pipe;
pub mod quality_scorer;
pub mod warm;
pub mod logo_collector;
pub mod epg_collector;

pub use channel_key::ChannelKey;
pub use encoder_pool::{EncodeKey, EncoderPool, EncoderPoolError, EncoderRuntimeConfig, SharedEncoder};
pub use pool::{TunerPool, TunerPoolConfig};
pub use shared::SharedTuner;
pub use warm::WarmTunerHandle;
pub use quality_scorer::{BonDriverWithScore, QualityScorer};
