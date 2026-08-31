//! Scheduled task management for recisdb-proxy.
//!
//! This module provides:
//! - [`ScanScheduler`]: Periodic channel scanning scheduler

pub mod epg_scheduler;
pub mod scan_scheduler;

pub use epg_scheduler::EpgScanScheduler;
pub use scan_scheduler::ScanScheduler;
