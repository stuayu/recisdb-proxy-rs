//! Database model definitions.

use recisdb_protocol::ChannelInfo;

/// BonDriver record from database.
#[derive(Debug, Clone)]
pub struct BonDriverRecord {
    pub id: i64,
    pub dll_path: String,
    pub driver_name: Option<String>,
    pub version: Option<String>,
    // Scan configuration
    pub auto_scan_enabled: bool,
    pub scan_interval_hours: i32,
    pub scan_priority: i32,
    pub last_scan: Option<i64>,
    pub next_scan_at: Option<i64>,
    pub passive_scan_enabled: bool,
    // Metadata
    pub created_at: i64,
    pub updated_at: i64,
}

/// Channel record from database.
#[derive(Debug, Clone)]
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

/// Scan history record.
#[derive(Debug, Clone)]
pub struct ScanHistoryRecord {
    pub id: i64,
    pub bon_driver_id: i64,
    pub scan_time: i64,
    pub channel_count: Option<i32>,
    pub success: bool,
    pub error_message: Option<String>,
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
}

impl NewBonDriver {
    pub fn new(dll_path: impl Into<String>) -> Self {
        Self {
            dll_path: dll_path.into(),
            driver_name: None,
            version: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.driver_name = Some(name.into());
        self
    }
}
