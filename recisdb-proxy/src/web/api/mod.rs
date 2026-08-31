//! Web API endpoints for monitoring and configuration.
//!
//! docs/SYSTEM_REVIEW_2026-07.md Phase 8 (M3): this used to be a single
//! ~2950-line `api.rs`. It is now split into domain submodules purely for
//! readability — every handler and public type is re-exported here so
//! `web/mod.rs`'s routes (`api::get_tuners`, etc.) and any other caller
//! keep working unchanged.

mod alerts;
mod bondrivers;
mod channels;
mod client_view;
mod clients;
mod configs;
mod epg;
mod error;
mod logs;
mod nodes;
mod programs;
mod service;
mod statics;
mod tuners;
mod update;

pub use alerts::*;
pub use bondrivers::*;
pub use channels::*;
pub use client_view::*;
pub use clients::*;
pub use configs::*;
pub use epg::*;
pub use error::*;
pub use logs::*;
pub use nodes::*;
pub use programs::*;
pub use service::*;
pub use statics::*;
pub use tuners::*;
pub use update::*;
