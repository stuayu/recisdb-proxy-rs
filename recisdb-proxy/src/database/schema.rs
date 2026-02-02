//! Database schema definitions.

/// SQL schema for the channel database.
pub const SCHEMA_SQL: &str = r#"
-- BonDriver management table
CREATE TABLE IF NOT EXISTS bon_drivers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dll_path TEXT UNIQUE NOT NULL,
    driver_name TEXT,
    version TEXT,
    -- Group management for multi-tuner selection
    group_name TEXT,                       -- Unified group name (e.g., "PX-MLT", "PX-Q1UD")
    -- Scan configuration (per-tuner)
    auto_scan_enabled INTEGER DEFAULT 1,     -- Auto scan enabled/disabled
    scan_interval_hours INTEGER DEFAULT 24,  -- Scan interval in hours (0 = disabled)
    scan_priority INTEGER DEFAULT 0,         -- Scan priority (higher = scanned first)
    last_scan INTEGER,                       -- Last scan timestamp
    next_scan_at INTEGER,                    -- Next scheduled scan timestamp
    -- Passive scan configuration
    passive_scan_enabled INTEGER DEFAULT 1,  -- Real-time update during streaming
    -- Concurrent usage control
    max_instances INTEGER DEFAULT 1,         -- Maximum concurrent instances (1 for exclusive)
    -- Metadata
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER DEFAULT (strftime('%s', 'now'))
);

-- Channel information table
CREATE TABLE IF NOT EXISTS channels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bon_driver_id INTEGER NOT NULL,
    -- Unique identification key (NID-SID-TSID-manual_sheet)
    nid INTEGER NOT NULL,                -- Network ID (from SDT)
    sid INTEGER NOT NULL,                -- Service ID
    tsid INTEGER NOT NULL,               -- Transport Stream ID
    manual_sheet INTEGER,                -- User-defined sheet number (NULL = default)
    -- Channel information
    raw_name TEXT,                       -- Raw service name (ARIB encoded)
    channel_name TEXT,                   -- Normalized channel name
    physical_ch INTEGER,                 -- Physical channel number (from NIT)
    remote_control_key INTEGER,          -- Remote control key ID (from NIT)
    service_type INTEGER,                -- Service type (0x01=TV, 0x02=Radio, etc.)
    network_name TEXT,                   -- Network name (from NIT)
    -- BonDriver-specific information
    bon_space INTEGER,                   -- BonDriver Space number
    bon_channel INTEGER,                 -- BonDriver Channel number
    -- Band and region classification (for auto-generated tuning spaces)
    band_type INTEGER,                   -- BandType enum (0=Terrestrial, 1=BS, 2=CS, 3=4K, 4=Other, 5=CATV, 6=SKY)
    region_id INTEGER,                   -- ARIB region ID (1-62 for terrestrial, NULL for others)
    terrestrial_region TEXT,             -- Prefecture name for Terrestrial (e.g., "福島", "宮城")
    -- State management
    is_enabled INTEGER DEFAULT 1,        -- Enabled/disabled flag
    scan_time INTEGER,                   -- Last scan timestamp
    last_seen INTEGER,                   -- Last detected timestamp (for auto-update)
    failure_count INTEGER DEFAULT 0,     -- Consecutive tuning failure count
    -- Selection priority
    priority INTEGER DEFAULT 0,          -- Channel selection priority (for logical mode)
    -- Metadata
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER DEFAULT (strftime('%s', 'now')),
    UNIQUE(bon_driver_id, nid, sid, tsid, manual_sheet),
    FOREIGN KEY(bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
);

-- Scan history table
CREATE TABLE IF NOT EXISTS scan_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bon_driver_id INTEGER NOT NULL,
    scan_time INTEGER DEFAULT (strftime('%s', 'now')),
    channel_count INTEGER,
    success INTEGER,
    error_message TEXT,
    FOREIGN KEY(bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
);

-- Scan scheduler configuration table
CREATE TABLE IF NOT EXISTS scan_scheduler_config (
    id INTEGER PRIMARY KEY CHECK (id = 1),  -- Only one config row allowed
    check_interval_secs INTEGER DEFAULT 60,
    max_concurrent_scans INTEGER DEFAULT 1,
    scan_timeout_secs INTEGER DEFAULT 900,
    updated_at INTEGER DEFAULT (strftime('%s', 'now'))
);

-- Session history table
CREATE TABLE IF NOT EXISTS session_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL,
    client_address TEXT NOT NULL,
    tuner_path TEXT,
    channel_info TEXT,
    channel_name TEXT,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    duration_secs INTEGER,
    packets_sent INTEGER DEFAULT 0,
    packets_dropped INTEGER DEFAULT 0,
    packets_scrambled INTEGER DEFAULT 0,
    packets_error INTEGER DEFAULT 0,
    bytes_sent INTEGER DEFAULT 0,
    average_bitrate_mbps REAL,
    average_signal_level REAL,
    disconnect_reason TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

-- Alert rules table
CREATE TABLE IF NOT EXISTS alert_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    metric TEXT NOT NULL,       -- 'drop_rate', 'scramble_rate', 'error_rate', 'signal_level', 'bitrate'
    condition TEXT NOT NULL,    -- 'gt', 'lt', 'gte', 'lte'
    threshold REAL NOT NULL,
    severity TEXT DEFAULT 'warning',
    is_enabled INTEGER DEFAULT 1,
    webhook_url TEXT,
    webhook_format TEXT DEFAULT 'generic',
    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

-- Alert history table
CREATE TABLE IF NOT EXISTS alert_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id INTEGER NOT NULL,
    session_id INTEGER,
    triggered_at INTEGER NOT NULL,
    resolved_at INTEGER,
    metric_value REAL,
    message TEXT,
    acknowledged INTEGER DEFAULT 0,
    FOREIGN KEY(rule_id) REFERENCES alert_rules(id) ON DELETE CASCADE
);

-- Driver quality stats table
CREATE TABLE IF NOT EXISTS driver_quality_stats (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bon_driver_id INTEGER NOT NULL,
    -- Cumulative stats
    total_packets INTEGER DEFAULT 0,
    dropped_packets INTEGER DEFAULT 0,
    scrambled_packets INTEGER DEFAULT 0,
    error_packets INTEGER DEFAULT 0,
    total_sessions INTEGER DEFAULT 0,
    -- Calculated score (0.0 - 1.0, higher is better)
    quality_score REAL DEFAULT 1.0,
    -- Recent stats (last 24h)
    recent_drop_rate REAL DEFAULT 0.0,
    recent_error_rate REAL DEFAULT 0.0,
    -- Timestamp
    last_updated INTEGER DEFAULT (strftime('%s', 'now')),
    UNIQUE(bon_driver_id),
    FOREIGN KEY(bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
);

-- Indexes for efficient queries
CREATE INDEX IF NOT EXISTS idx_bon_drivers_group_name ON bon_drivers(group_name);
CREATE INDEX IF NOT EXISTS idx_channels_bon_driver ON channels(bon_driver_id);
CREATE INDEX IF NOT EXISTS idx_channels_nid_sid_tsid ON channels(nid, sid, tsid);
CREATE INDEX IF NOT EXISTS idx_channels_enabled ON channels(is_enabled);
CREATE INDEX IF NOT EXISTS idx_channels_nid_tsid_priority ON channels(nid, tsid, priority DESC, is_enabled);
CREATE INDEX IF NOT EXISTS idx_scan_history_bon_driver ON scan_history(bon_driver_id);
CREATE INDEX IF NOT EXISTS idx_channels_band_type ON channels(band_type, is_enabled);
CREATE INDEX IF NOT EXISTS idx_session_history_session_id ON session_history(session_id);
CREATE INDEX IF NOT EXISTS idx_session_history_created_at ON session_history(created_at);
CREATE INDEX IF NOT EXISTS idx_alert_rules_enabled ON alert_rules(is_enabled);
CREATE INDEX IF NOT EXISTS idx_alert_history_rule ON alert_history(rule_id);
CREATE INDEX IF NOT EXISTS idx_driver_quality_stats_driver ON driver_quality_stats(bon_driver_id);

-- Trigger to update updated_at on bon_drivers
CREATE TRIGGER IF NOT EXISTS bon_drivers_updated_at
AFTER UPDATE ON bon_drivers
BEGIN
    UPDATE bon_drivers SET updated_at = strftime('%s', 'now') WHERE id = NEW.id;
END;

-- Trigger to update updated_at on channels
CREATE TRIGGER IF NOT EXISTS channels_updated_at
AFTER UPDATE ON channels
BEGIN
    UPDATE channels SET updated_at = strftime('%s', 'now') WHERE id = NEW.id;
END;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_schema_valid() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        // Verify all tables were created
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"bon_drivers".to_string()));
        assert!(tables.contains(&"channels".to_string()));
        assert!(tables.contains(&"scan_history".to_string()));
        assert!(tables.contains(&"session_history".to_string()));
        assert!(tables.contains(&"alert_rules".to_string()));
        assert!(tables.contains(&"alert_history".to_string()));
        assert!(tables.contains(&"driver_quality_stats".to_string()));
    }
}
