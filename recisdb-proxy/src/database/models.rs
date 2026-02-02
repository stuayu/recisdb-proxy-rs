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
    pub remote_control_key: Option<u8>,
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
