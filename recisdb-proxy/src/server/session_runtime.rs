//! Runtime settings and encoder-side channel metadata for BNDP sessions.
//!
//! Extracted from `session.rs` so DB-backed tsreplace/prefill policy and SID
//! resolution can evolve independently of the core session state machine.

use crate::server::listener::DatabaseHandle;
use log::{debug, warn};

#[derive(Debug, Clone)]
pub(super) struct TsreplaceRuntimeConfig {
    pub enabled: bool,
    pub command_path: String,
    pub arguments: String,
    pub read_timeout_ms: u64,
    pub passthrough_on_error: bool,
    pub max_concurrent_encoders: i64,
    /// Optional stage-1 command (e.g. tsreadex) piped before `command_path`.
    pub preprocessor_path: String,
    pub preprocessor_arguments: String,
}

/// Fixed-duration prefill/jitter buffer settings loaded from `tuner_config`.
#[derive(Debug, Clone, Copy)]
pub(super) struct PrefillRuntimeConfig {
    pub view_ms: u64,
    pub preview_ms: u64,
    pub record_ms: u64,
    pub safety_factor: f64,
}

impl Default for PrefillRuntimeConfig {
    fn default() -> Self {
        Self {
            view_ms: 1000,
            preview_ms: 2000,
            record_ms: 6000,
            safety_factor: 1.5,
        }
    }
}

/// Per-class TS write queue durations loaded from `tuner_config`
/// (STREAMING_DESIGN.md §3.2).
///
/// A duration, not a byte count: the byte budget is computed at runtime from
/// the session's measured bitrate, so one setting means the same thing whether
/// the stream is a full 18 Mbps multiplex or a 2 Mbps transcode relayed
/// between sites.
#[derive(Debug, Clone, Copy)]
pub(super) struct TsQueueRuntimeConfig {
    pub view_ms: u64,
    pub preview_ms: u64,
    pub record_ms: u64,
}

impl Default for TsQueueRuntimeConfig {
    fn default() -> Self {
        Self {
            view_ms: 8_000,
            preview_ms: 12_000,
            record_ms: 15_000,
        }
    }
}

pub(super) async fn load_ts_queue_runtime_config(
    database: &DatabaseHandle,
    session_id: u64,
) -> TsQueueRuntimeConfig {
    let db = database.lock().await;
    match db.get_ts_queue_config() {
        Ok((view_ms, preview_ms, record_ms)) => TsQueueRuntimeConfig {
            view_ms,
            preview_ms,
            record_ms,
        },
        Err(e) => {
            warn!(
                "[Session {}] Failed to load TS queue config: {}",
                session_id, e
            );
            TsQueueRuntimeConfig::default()
        }
    }
}

pub(super) async fn load_tsreplace_runtime_config(
    database: &DatabaseHandle,
    session_id: u64,
) -> TsreplaceRuntimeConfig {
    let db = database.lock().await;
    match db.get_tsreplace_config() {
        Ok((
            enabled,
            command_path,
            arguments,
            read_timeout_ms,
            passthrough_on_error,
            max_concurrent_encoders,
            preprocessor_path,
            preprocessor_arguments,
        )) => TsreplaceRuntimeConfig {
            enabled,
            command_path,
            arguments,
            read_timeout_ms,
            passthrough_on_error,
            max_concurrent_encoders,
            preprocessor_path,
            preprocessor_arguments,
        },
        Err(e) => {
            warn!(
                "[Session {}] Failed to load tsreplace config: {}",
                session_id, e
            );
            TsreplaceRuntimeConfig {
                enabled: false,
                command_path: "tsreplace".to_string(),
                arguments: String::new(),
                read_timeout_ms: 10_000,
                passthrough_on_error: true,
                max_concurrent_encoders: 2,
                preprocessor_path: String::new(),
                preprocessor_arguments: String::new(),
            }
        }
    }
}

pub(super) async fn load_prefill_runtime_config(
    database: &DatabaseHandle,
    session_id: u64,
) -> PrefillRuntimeConfig {
    let db = database.lock().await;
    match db.get_tuner_config() {
        Ok((
            _keep_alive_secs,
            _prewarm_enabled,
            _prewarm_timeout_secs,
            _set_channel_retry_interval_ms,
            _set_channel_retry_timeout_ms,
            _signal_poll_interval_ms,
            _signal_wait_timeout_ms,
            prefill_view_ms,
            prefill_preview_ms,
            prefill_record_ms,
            jitter_safety_factor,
        )) => PrefillRuntimeConfig {
            view_ms: prefill_view_ms,
            preview_ms: prefill_preview_ms,
            record_ms: prefill_record_ms,
            safety_factor: jitter_safety_factor,
        },
        Err(e) => {
            warn!(
                "[Session {}] Failed to load prefill config: {}",
                session_id, e
            );
            PrefillRuntimeConfig::default()
        }
    }
}

pub(super) async fn resolve_encode_sids(
    database: &DatabaseHandle,
    session_id: u64,
    single_service_filter_enabled: bool,
    current_sid: Option<u16>,
    current_nid: Option<u16>,
    current_tsid: Option<u16>,
) -> Vec<u16> {
    if single_service_filter_enabled {
        return current_sid.into_iter().collect();
    }

    if let (Some(nid), Some(tsid)) = (current_nid, current_tsid) {
        let db = database.lock().await;
        match db.get_sids_for_nid_tsid(nid, tsid) {
            Ok(sids) if !sids.is_empty() => return sids,
            Ok(_) => {
                debug!(
                    "[Session {}] No SIDs found for NID=0x{:04X} TSID=0x{:04X}",
                    session_id, nid, tsid
                );
            }
            Err(e) => {
                warn!(
                    "[Session {}] Failed to query SIDs for NID=0x{:04X} TSID=0x{:04X}: {}",
                    session_id, nid, tsid, e
                );
            }
        }
    }

    Vec::new()
}
