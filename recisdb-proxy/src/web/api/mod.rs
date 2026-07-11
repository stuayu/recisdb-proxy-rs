//! Web API endpoints for monitoring and configuration.
//!
//! docs/SYSTEM_REVIEW_2026-07.md Phase 8 (M3): this used to be a single
//! ~2950-line `api.rs`. It is now split into domain submodules purely for
//! readability — every handler and public type is re-exported here so
//! `web/mod.rs`'s routes (`api::get_tuners`, etc.) and any other caller
//! keep working unchanged.

mod error;
mod statics;
mod tuners;
mod bondrivers;
mod channels;
mod clients;
mod alerts;
mod configs;
mod client_view;

pub use error::*;
pub use statics::*;
pub use tuners::*;
pub use bondrivers::*;
pub use channels::*;
pub use clients::*;
pub use alerts::*;
pub use configs::*;
pub use client_view::*;
