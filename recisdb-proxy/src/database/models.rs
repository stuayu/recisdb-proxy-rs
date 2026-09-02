//! Database model definitions.

use recisdb_protocol::ChannelInfo;
use serde::Serialize;

/// BonDriver record from database.
#[derive(Debug, Clone, Serialize)]
pub struct BonDriverRecord {
    pub id: i64,
    pub dll_path: String,
    pub driver_name: Option<String>,
    pub version: Option<String>,
    // Group management
    pub group_name: Option<String>,
    // Scan configuration
    pub auto_scan_enabled: bool,
    pub scan_interval_hours: i32,
    pub scan_priority: i32,
    pub last_scan: Option<i64>,
    pub next_scan_at: Option<i64>,
    pub passive_scan_enabled: bool,
    // Concurrent usage control
    pub max_instances: i32,
    // Metadata
    pub created_at: i64,
    pub updated_at: i64,
}

/// Channel record from database.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelRecord {
    pub id: i64,
    pub bon_driver_id: i64,
    // Unique key
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub manual_sheet: Option<u16>,
    // Channel info
    pub raw_name: Option<String>,
    pub channel_name: Option<String>,
    pub physical_ch: Option<u8>,
    // u16: CS110 stores the 3-digit channel number (= SID) here
    pub remote_control_key: Option<u16>,
    pub service_type: Option<u8>,
    pub network_name: Option<String>,
    // BonDriver specific
    pub bon_space: Option<u32>,
    pub bon_channel: Option<u32>,
    // Band and region classification
    pub band_type: Option<u8>,
    pub region_id: Option<u8>,
    pub terrestrial_region: Option<String>,
    // State
    pub is_enabled: bool,
    pub scan_time: Option<i64>,
    pub last_seen: Option<i64>,
    pub failure_count: i32,
    pub priority: i32,
    // Metadata
    pub created_at: i64,
    pub updated_at: i64,
}

impl ChannelRecord {
    /// Convert to ChannelInfo (protocol type).
    pub fn to_channel_info(&self) -> ChannelInfo {
        ChannelInfo {
            nid: self.nid,
            sid: self.sid,
            tsid: self.tsid,
            manual_sheet: self.manual_sheet,
            raw_name: self.raw_name.clone(),
            channel_name: self.channel_name.clone(),
            physical_ch: self.physical_ch,
            remote_control_key: self.remote_control_key,
            service_type: self.service_type,
            network_name: self.network_name.clone(),
            bon_space: self.bon_space,
            bon_channel: self.bon_channel,
            band_type: self.band_type,
            terrestrial_region: self.terrestrial_region.clone(),
        }
    }
}

/// Channel record with BonDriver path (for joined queries).
#[derive(Debug, Clone)]
pub struct ChannelWithDriver {
    pub channel: ChannelRecord,
    pub bon_driver_path: String,
    pub bon_driver_scan_priority: i32,
}

/// Simplified channel record with BonDriver info for client queries.
#[derive(Debug, Clone)]
pub struct ClientChannelRecord {
    pub id: i64,
    pub bon_driver_id: i64,
    pub nid: i32,
    pub sid: i32,
    pub tsid: i32,
    pub service_name: Option<String>,
    pub ts_name: Option<String>,
    pub service_type: Option<i32>,
    pub remote_control_key: Option<i32>,
    pub space: u32,
    pub channel: u32,
    pub is_enabled: bool,
    pub priority: i32,
}

/// Scan history record.
#[derive(Debug, Clone, Serialize)]
pub struct ScanHistoryRecord {
    pub id: i64,
    pub bon_driver_id: i64,
    pub scan_time: i64,
    pub channel_count: Option<i32>,
    pub success: bool,
    pub error_message: Option<String>,
}

/// Session history record.
#[derive(Debug, Clone, Serialize)]
pub struct SessionHistoryRecord {
    pub id: i64,
    pub session_id: i64,
    pub client_address: String,
    pub tuner_path: Option<String>,
    pub channel_info: Option<String>,
    pub channel_name: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_secs: Option<i64>,
    pub packets_sent: i64,
    pub packets_dropped: i64,
    pub packets_scrambled: i64,
    pub packets_error: i64,
    pub bytes_sent: i64,
    pub average_bitrate_mbps: Option<f64>,
    pub average_signal_level: Option<f64>,
    pub disconnect_reason: Option<String>,
    pub created_at: i64,
    /// JSON: loss-source breakdown + top-loss PIDs (P1, see STREAMING_DESIGN.md §3.1).
    pub loss_summary: Option<String>,
    /// Stream reliability class at session end: "view"/"record"/"preview"
    /// (P2, see STREAMING_DESIGN.md §2). `None` for rows predating P2.
    pub stream_class: Option<String>,
}

/// Alert rule record.
#[derive(Debug, Clone, Serialize)]
pub struct AlertRuleRecord {
    pub id: i64,
    pub name: String,
    pub metric: String,
    pub condition: String,
    pub threshold: f64,
    pub severity: String,
    pub is_enabled: bool,
    pub webhook_url: Option<String>,
    pub webhook_format: Option<String>,
    pub created_at: i64,
}

/// Alert history record.
#[derive(Debug, Clone, Serialize)]
pub struct AlertHistoryRecord {
    pub id: i64,
    pub rule_id: i64,
    pub session_id: Option<i64>,
    pub triggered_at: i64,
    pub resolved_at: Option<i64>,
    pub metric_value: Option<f64>,
    pub message: Option<String>,
    pub acknowledged: bool,
}

/// Encode profile record (STREAMING_DESIGN.md §5.3/§9 P5).
///
/// `purpose` is a free-form string in practice (`'record'` / `'preview'` /
/// `'view'`) but kept as `String` rather than an enum since it is stored
/// verbatim in SQLite and new purposes may be added without a migration.
#[derive(Debug, Clone, Serialize)]
pub struct EncodeProfileRecord {
    pub id: i64,
    pub name: String,
    pub purpose: String,
    pub codec: String,
    pub container: String,
    pub target_bitrate: Option<i64>,
    pub extra_args: Option<String>,
    pub is_enabled: bool,
    pub created_at: i64,
}

/// Driver quality stats record.
#[derive(Debug, Clone, Serialize)]
pub struct DriverQualityStats {
    pub id: i64,
    pub bon_driver_id: i64,
    pub total_packets: i64,
    pub dropped_packets: i64,
    pub scrambled_packets: i64,
    pub error_packets: i64,
    pub total_sessions: i64,
    pub quality_score: f64,
    pub recent_drop_rate: f64,
    pub recent_error_rate: f64,
    pub last_updated: i64,
}

/// Runtime-health sample for a BonDriver. Packet integrity is intentionally
/// separate: this captures "technically succeeds but is too slow/stall-prone"
/// behaviour so the route selector can demote it before users repeatedly pay
/// the startup delay.
#[derive(Debug, Clone, Copy, Default)]
pub struct DriverRuntimeSample {
    pub open_ms: Option<u64>,
    pub tune_ms: Option<u64>,
    pub first_ts_ms: Option<u64>,
    pub stalled: bool,
    pub open_failed: bool,
    pub tune_failed: bool,
    pub first_ts_timeout: bool,
    pub worker_restart: bool,
}

/// Result of merging scan results into database.
#[derive(Debug, Default, Clone)]
pub struct MergeResult {
    pub inserted: usize,
    pub updated: usize,
    pub disabled: usize,
}

impl MergeResult {
    pub fn total_changes(&self) -> usize {
        self.inserted + self.updated + self.disabled
    }
}

/// Logical EIT database, matching EDCB's independent p/f and schedule stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpgSource {
    PresentFollowing = 0,
    Schedule = 1,
}

/// One EIT event ready to be UPSERTed into `programs` (Migration 015).
/// Produced by `tuner::epg_collector::EpgCollector` from a parsed EIT
/// section; consumed in batches by `crate::epg_writer::EpgWriter`.
#[derive(Debug, Clone)]
pub struct ProgramUpsert {
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub event_id: u16,
    /// Event start time, epoch seconds (UTC).
    pub start_at: i64,
    pub duration_secs: i64,
    pub free_ca_mode: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub extended: Option<String>,
    /// `(content_nibble_level_1 << 4) | content_nibble_level_2` of the
    /// first content_descriptor genre entry, if present.
    pub genre: Option<i64>,
    pub updated_at: i64,
    pub source: EpgSource,
    pub basic_updated_at: Option<i64>,
    pub extended_updated_at: Option<i64>,
}

/// A stored `programs` row (dashboard `GET /api/programs`, Mirakurun-
/// compatible `GET /programs`).
#[derive(Debug, Clone, Serialize)]
pub struct ProgramRecord {
    pub id: i64,
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub event_id: u16,
    pub start_at: i64,
    pub duration_secs: i64,
    pub free_ca_mode: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub extended: Option<String>,
    pub genre: Option<i64>,
    pub updated_at: i64,
}

/// New BonDriver to insert.
#[derive(Debug, Clone, Default)]
pub struct NewBonDriver {
    pub dll_path: String,
    pub driver_name: Option<String>,
    pub version: Option<String>,
    pub max_instances: Option<i32>,
}

impl NewBonDriver {
    pub fn new(dll_path: impl Into<String>) -> Self {
        Self {
            dll_path: dll_path.into(),
            driver_name: None,
            version: None,
            max_instances: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.driver_name = Some(name.into());
        self
    }

    pub fn with_max_instances(mut self, max_instances: i32) -> Self {
        self.max_instances = Some(max_instances);
        self
    }
}

/// What a BonDriver actually hands back from `GetTsStream`.
///
/// Everything downstream of the reader — the TS analyzer, the EPG and logo
/// collectors, `send_ts_data`'s 188-byte alignment, every session — assumes
/// MPEG-2 TS. A 4K tuner delivers MMT/TLV instead, which has to be converted
/// before any of that runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamFormat {
    /// MPEG-2 TS. Every terrestrial/BS/CS tuner.
    #[default]
    Ts,
    /// Raw MMT/TLV from an advanced-BS (4K) tuner. Needs the external
    /// converter (`tuner/mmt_pipe.rs`) in front of the broadcast.
    MmtTlv,
}

impl StreamFormat {
    pub fn as_db_value(self) -> &'static str {
        match self {
            StreamFormat::Ts => "ts",
            StreamFormat::MmtTlv => "mmttlv",
        }
    }

    /// Parse a stored value. Anything unrecognised is treated as TS: that
    /// keeps a typo from silently inserting a converter (or, worse, from
    /// leaving a 4K driver looking like a broken TS driver).
    pub fn from_db_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "mmttlv" | "mmt/tlv" | "mmt_tlv" => StreamFormat::MmtTlv,
            _ => StreamFormat::Ts,
        }
    }

    pub fn is_mmt_tlv(self) -> bool {
        matches!(self, StreamFormat::MmtTlv)
    }
}
