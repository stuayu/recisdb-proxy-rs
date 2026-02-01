//! Channel CRUD operations.

use super::{ChannelRecord, ChannelWithDriver, Database, MergeResult, Result, ScanHistoryRecord};
use recisdb_protocol::ChannelInfo;
use rusqlite::params;
use std::collections::HashSet;

impl Database {
    /// Insert a new channel.
    pub fn insert_channel(&self, bon_driver_id: i64, info: &ChannelInfo) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO channels (
                bon_driver_id, nid, sid, tsid, manual_sheet,
                raw_name, channel_name, physical_ch, remote_control_key,
                service_type, network_name, bon_space, bon_channel,
                scan_time, last_seen
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                      strftime('%s', 'now'), strftime('%s', 'now'))",
            params![
                bon_driver_id,
                info.nid as i32,
                info.sid as i32,
                info.tsid as i32,
                info.manual_sheet.map(|v| v as i32),
                info.raw_name,
                info.channel_name,
                info.physical_ch.map(|v| v as i32),
                info.remote_control_key.map(|v| v as i32),
                info.service_type.map(|v| v as i32),
                info.network_name,
                info.bon_space.map(|v| v as i32),
                info.bon_channel.map(|v| v as i32),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get channel by unique key (bon_driver_id, nid, sid, tsid, manual_sheet).
    pub fn get_channel_by_key(
        &self,
        bon_driver_id: i64,
        nid: u16,
        sid: u16,
        tsid: u16,
        manual_sheet: Option<u16>,
    ) -> Result<Option<ChannelRecord>> {
        let sql = if manual_sheet.is_some() {
            "SELECT * FROM channels WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet = ?5"
        } else {
            "SELECT * FROM channels WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet IS NULL"
        };

        let mut stmt = self.conn.prepare(sql)?;

        let result = if let Some(ms) = manual_sheet {
            stmt.query_row(
                params![bon_driver_id, nid as i32, sid as i32, tsid as i32, ms as i32],
                Self::row_to_channel_record,
            )
        } else {
            stmt.query_row(
                params![bon_driver_id, nid as i32, sid as i32, tsid as i32],
                Self::row_to_channel_record,
            )
        };

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get channels by BonDriver ID.
    pub fn get_channels_by_bon_driver(&self, bon_driver_id: i64) -> Result<Vec<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels WHERE bon_driver_id = ?1 ORDER BY priority DESC, nid, tsid, sid",
        )?;

        let records = stmt
            .query_map([bon_driver_id], Self::row_to_channel_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get all channels from the database.
    pub fn get_all_channels(&self) -> Result<Vec<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels ORDER BY nid, tsid, sid",
        )?;

        let records = stmt
            .query_map([], Self::row_to_channel_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get enabled channels by NID/TSID with priority ordering.
    pub fn get_channels_by_nid_tsid_ordered(
        &self,
        nid: u16,
        tsid: u16,
        sid: Option<u16>,
    ) -> Result<Vec<ChannelWithDriver>> {
        let sql = if sid.is_some() {
            "SELECT c.*, bd.dll_path, bd.scan_priority
             FROM channels c
             JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             WHERE c.nid = ?1 AND c.tsid = ?2 AND c.sid = ?3 AND c.is_enabled = 1
             ORDER BY c.priority DESC, bd.scan_priority DESC"
        } else {
            "SELECT c.*, bd.dll_path, bd.scan_priority
             FROM channels c
             JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             WHERE c.nid = ?1 AND c.tsid = ?2 AND c.is_enabled = 1
             ORDER BY c.priority DESC, bd.scan_priority DESC"
        };

        let mut stmt = self.conn.prepare(sql)?;

        let results = if let Some(s) = sid {
            stmt.query_map(params![nid as i32, tsid as i32, s as i32], |row| {
                Ok(ChannelWithDriver {
                    channel: Self::row_to_channel_record(row)?,
                    bon_driver_path: row.get("dll_path")?,
                    bon_driver_scan_priority: row.get("scan_priority")?,
                })
            })?
        } else {
            stmt.query_map(params![nid as i32, tsid as i32], |row| {
                Ok(ChannelWithDriver {
                    channel: Self::row_to_channel_record(row)?,
                    bon_driver_path: row.get("dll_path")?,
                    bon_driver_scan_priority: row.get("scan_priority")?,
                })
            })?
        };

        let records = results.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Get channel by physical specification (tuner + space + channel).
    pub fn get_channel_by_physical(
        &self,
        bon_driver_path: &str,
        space: u32,
        channel: u32,
    ) -> Result<Option<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.* FROM channels c
             JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             WHERE bd.dll_path = ?1 AND c.bon_space = ?2 AND c.bon_channel = ?3",
        )?;

        let result = stmt.query_row(
            params![bon_driver_path, space as i32, channel as i32],
            Self::row_to_channel_record,
        );

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Update channel information.
    pub fn update_channel(&self, bon_driver_id: i64, info: &ChannelInfo) -> Result<()> {
        let sql = if info.manual_sheet.is_some() {
            "UPDATE channels SET
                raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                scan_time = strftime('%s', 'now'), last_seen = strftime('%s', 'now'),
                is_enabled = 1
             WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet = ?13"
        } else {
            "UPDATE channels SET
                raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                scan_time = strftime('%s', 'now'), last_seen = strftime('%s', 'now'),
                is_enabled = 1
             WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet IS NULL"
        };

        if info.manual_sheet.is_some() {
            self.conn.execute(
                sql,
                params![
                    bon_driver_id,
                    info.nid as i32,
                    info.sid as i32,
                    info.tsid as i32,
                    info.raw_name,
                    info.channel_name,
                    info.physical_ch.map(|v| v as i32),
                    info.remote_control_key.map(|v| v as i32),
                    info.service_type.map(|v| v as i32),
                    info.network_name,
                    info.bon_space.map(|v| v as i32),
                    info.bon_channel.map(|v| v as i32),
                    info.manual_sheet.map(|v| v as i32),
                ],
            )?;
        } else {
            self.conn.execute(
                sql,
                params![
                    bon_driver_id,
                    info.nid as i32,
                    info.sid as i32,
                    info.tsid as i32,
                    info.raw_name,
                    info.channel_name,
                    info.physical_ch.map(|v| v as i32),
                    info.remote_control_key.map(|v| v as i32),
                    info.service_type.map(|v| v as i32),
                    info.network_name,
                    info.bon_space.map(|v| v as i32),
                    info.bon_channel.map(|v| v as i32),
                ],
            )?;
        }

        Ok(())
    }

    /// Disable a channel (soft delete).
    pub fn disable_channel(&self, channel_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE channels SET is_enabled = 0 WHERE id = ?1",
            [channel_id],
        )?;
        Ok(())
    }

    /// Increment failure count for a channel.
    pub fn increment_failure_count(&self, channel_id: i64) -> Result<i32> {
        self.conn.execute(
            "UPDATE channels SET failure_count = failure_count + 1 WHERE id = ?1",
            [channel_id],
        )?;

        let count: i32 = self.conn.query_row(
            "SELECT failure_count FROM channels WHERE id = ?1",
            [channel_id],
            |row| row.get(0),
        )?;

        Ok(count)
    }

    /// Reset failure count for a channel.
    pub fn reset_failure_count(&self, channel_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE channels SET failure_count = 0, last_seen = strftime('%s', 'now') WHERE id = ?1",
            [channel_id],
        )?;
        Ok(())
    }

    /// Merge scan results into database.
    pub fn merge_scan_results(
        &mut self,
        bon_driver_id: i64,
        scanned_channels: &[ChannelInfo],
    ) -> Result<MergeResult> {
        let tx = self.conn.transaction()?;
        let mut result = MergeResult::default();

        // Get existing channels for this BonDriver
        let existing: Vec<ChannelRecord> = {
            let mut stmt = tx.prepare(
                "SELECT * FROM channels WHERE bon_driver_id = ?1",
            )?;
            stmt.query_map([bon_driver_id], Self::row_to_channel_record)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let existing_keys: HashSet<_> = existing
            .iter()
            .map(|c| (c.nid, c.sid, c.tsid, c.manual_sheet))
            .collect();

        let scanned_keys: HashSet<_> = scanned_channels
            .iter()
            .map(|c| (c.nid, c.sid, c.tsid, c.manual_sheet))
            .collect();

        // Process scanned channels
        for info in scanned_channels {
            let key = (info.nid, info.sid, info.tsid, info.manual_sheet);

            if existing_keys.contains(&key) {
                // Update existing
                let sql = if info.manual_sheet.is_some() {
                    "UPDATE channels SET
                        raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                        service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                        scan_time = strftime('%s', 'now'), last_seen = strftime('%s', 'now'),
                        is_enabled = 1
                     WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet = ?13"
                } else {
                    "UPDATE channels SET
                        raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                        service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                        scan_time = strftime('%s', 'now'), last_seen = strftime('%s', 'now'),
                        is_enabled = 1
                     WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet IS NULL"
                };

                if info.manual_sheet.is_some() {
                    tx.execute(
                        sql,
                        params![
                            bon_driver_id,
                            info.nid as i32,
                            info.sid as i32,
                            info.tsid as i32,
                            info.raw_name,
                            info.channel_name,
                            info.physical_ch.map(|v| v as i32),
                            info.remote_control_key.map(|v| v as i32),
                            info.service_type.map(|v| v as i32),
                            info.network_name,
                            info.bon_space.map(|v| v as i32),
                            info.bon_channel.map(|v| v as i32),
                            info.manual_sheet.map(|v| v as i32),
                        ],
                    )?;
                } else {
                    tx.execute(
                        sql,
                        params![
                            bon_driver_id,
                            info.nid as i32,
                            info.sid as i32,
                            info.tsid as i32,
                            info.raw_name,
                            info.channel_name,
                            info.physical_ch.map(|v| v as i32),
                            info.remote_control_key.map(|v| v as i32),
                            info.service_type.map(|v| v as i32),
                            info.network_name,
                            info.bon_space.map(|v| v as i32),
                            info.bon_channel.map(|v| v as i32),
                        ],
                    )?;
                }
                result.updated += 1;
            } else {
                // Insert new
                tx.execute(
                    "INSERT INTO channels (
                        bon_driver_id, nid, sid, tsid, manual_sheet,
                        raw_name, channel_name, physical_ch, remote_control_key,
                        service_type, network_name, bon_space, bon_channel,
                        scan_time, last_seen
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                              strftime('%s', 'now'), strftime('%s', 'now'))",
                    params![
                        bon_driver_id,
                        info.nid as i32,
                        info.sid as i32,
                        info.tsid as i32,
                        info.manual_sheet.map(|v| v as i32),
                        info.raw_name,
                        info.channel_name,
                        info.physical_ch.map(|v| v as i32),
                        info.remote_control_key.map(|v| v as i32),
                        info.service_type.map(|v| v as i32),
                        info.network_name,
                        info.bon_space.map(|v| v as i32),
                        info.bon_channel.map(|v| v as i32),
                    ],
                )?;
                result.inserted += 1;
            }
        }

        // Disable channels that were not found in this scan
        for existing_ch in &existing {
            let key = (
                existing_ch.nid,
                existing_ch.sid,
                existing_ch.tsid,
                existing_ch.manual_sheet,
            );
            if !scanned_keys.contains(&key) && existing_ch.is_enabled {
                tx.execute(
                    "UPDATE channels SET is_enabled = 0 WHERE id = ?1",
                    [existing_ch.id],
                )?;
                result.disabled += 1;
            }
        }

        tx.commit()?;
        Ok(result)
    }

    /// Passive scan update (lightweight: only update last_seen or full update if changed).
    pub fn passive_update_channels(
        &self,
        bon_driver_id: i64,
        channels: &[ChannelInfo],
    ) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let mut updated = 0;

        for info in channels {
            let existing =
                self.get_channel_by_key(bon_driver_id, info.nid, info.sid, info.tsid, info.manual_sheet)?;

            match existing {
                Some(existing) => {
                    // Update last_seen and reset failure count
                    self.conn.execute(
                        "UPDATE channels SET last_seen = ?1, failure_count = 0 WHERE id = ?2",
                        params![now, existing.id],
                    )?;

                    // Full update if channel name or service type changed
                    if existing.channel_name != info.channel_name
                        || existing.service_type != info.service_type
                    {
                        self.update_channel(bon_driver_id, info)?;
                        updated += 1;
                    }
                }
                None => {
                    // New channel discovered during passive scan
                    self.insert_channel(bon_driver_id, info)?;
                    updated += 1;
                    log::info!(
                        "Passive scan: new channel discovered: NID={}, SID={}, TSID={}",
                        info.nid,
                        info.sid,
                        info.tsid
                    );
                }
            }
        }

        Ok(updated)
    }

    /// Record scan history.
    pub fn insert_scan_history(
        &self,
        bon_driver_id: i64,
        channel_count: i32,
        success: bool,
        error_message: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO scan_history (bon_driver_id, channel_count, success, error_message)
             VALUES (?1, ?2, ?3, ?4)",
            params![bon_driver_id, channel_count, success as i32, error_message],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get scan history for a BonDriver.
    pub fn get_scan_history(&self, bon_driver_id: i64, limit: i32) -> Result<Vec<ScanHistoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, bon_driver_id, scan_time, channel_count, success, error_message
             FROM scan_history WHERE bon_driver_id = ?1 ORDER BY scan_time DESC LIMIT ?2",
        )?;

        let records = stmt
            .query_map(params![bon_driver_id, limit], |row| {
                Ok(ScanHistoryRecord {
                    id: row.get(0)?,
                    bon_driver_id: row.get(1)?,
                    scan_time: row.get(2)?,
                    channel_count: row.get(3)?,
                    success: row.get::<_, i32>(4)? != 0,
                    error_message: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Helper: Convert a row to ChannelRecord.
    fn row_to_channel_record(row: &rusqlite::Row) -> rusqlite::Result<ChannelRecord> {
        Ok(ChannelRecord {
            id: row.get("id")?,
            bon_driver_id: row.get("bon_driver_id")?,
            nid: row.get::<_, i32>("nid")? as u16,
            sid: row.get::<_, i32>("sid")? as u16,
            tsid: row.get::<_, i32>("tsid")? as u16,
            manual_sheet: row.get::<_, Option<i32>>("manual_sheet")?.map(|v| v as u16),
            raw_name: row.get("raw_name")?,
            channel_name: row.get("channel_name")?,
            physical_ch: row.get::<_, Option<i32>>("physical_ch")?.map(|v| v as u8),
            remote_control_key: row.get::<_, Option<i32>>("remote_control_key")?.map(|v| v as u8),
            service_type: row.get::<_, Option<i32>>("service_type")?.map(|v| v as u8),
            network_name: row.get("network_name")?,
            bon_space: row.get::<_, Option<i32>>("bon_space")?.map(|v| v as u32),
            bon_channel: row.get::<_, Option<i32>>("bon_channel")?.map(|v| v as u32),
            is_enabled: row.get::<_, i32>("is_enabled")? != 0,
            scan_time: row.get("scan_time")?,
            last_seen: row.get("last_seen")?,
            failure_count: row.get("failure_count")?,
            priority: row.get("priority")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_channel(nid: u16, sid: u16, tsid: u16) -> ChannelInfo {
        let mut info = ChannelInfo::new(nid, sid, tsid);
        info.channel_name = Some(format!("Test Channel {}", sid));
        info.bon_space = Some(0);
        info.bon_channel = Some(sid as u32);
        info
    }

    #[test]
    fn test_channel_crud() {
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();

        // Insert
        let info = create_test_channel(0x7FE8, 1024, 32736);
        let id = db.insert_channel(bon_driver_id, &info).unwrap();
        assert!(id > 0);

        // Get by key
        let record = db
            .get_channel_by_key(bon_driver_id, 0x7FE8, 1024, 32736, None)
            .unwrap()
            .unwrap();
        assert_eq!(record.nid, 0x7FE8);
        assert_eq!(record.sid, 1024);
        assert!(record.is_enabled);

        // Update
        let mut updated_info = info.clone();
        updated_info.channel_name = Some("Updated Channel".to_string());
        db.update_channel(bon_driver_id, &updated_info).unwrap();

        let updated = db
            .get_channel_by_key(bon_driver_id, 0x7FE8, 1024, 32736, None)
            .unwrap()
            .unwrap();
        assert_eq!(updated.channel_name, Some("Updated Channel".to_string()));

        // Disable
        db.disable_channel(id).unwrap();
        let disabled = db
            .get_channel_by_key(bon_driver_id, 0x7FE8, 1024, 32736, None)
            .unwrap()
            .unwrap();
        assert!(!disabled.is_enabled);
    }

    #[test]
    fn test_merge_scan_results() {
        let mut db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();

        // Initial scan: 3 channels
        let channels1 = vec![
            create_test_channel(0x7FE8, 1024, 32736),
            create_test_channel(0x7FE8, 1032, 32736),
            create_test_channel(0x7FE8, 1040, 32736),
        ];

        let result1 = db.merge_scan_results(bon_driver_id, &channels1).unwrap();
        assert_eq!(result1.inserted, 3);
        assert_eq!(result1.updated, 0);
        assert_eq!(result1.disabled, 0);

        // Second scan: 1 updated, 1 new, 1 removed
        let channels2 = vec![
            create_test_channel(0x7FE8, 1024, 32736), // existing
            create_test_channel(0x7FE8, 1032, 32736), // existing
            create_test_channel(0x7FE8, 1048, 32736), // new
            // 1040 is missing -> should be disabled
        ];

        let result2 = db.merge_scan_results(bon_driver_id, &channels2).unwrap();
        assert_eq!(result2.inserted, 1);
        assert_eq!(result2.updated, 2);
        assert_eq!(result2.disabled, 1);

        // Verify disabled channel
        let disabled = db
            .get_channel_by_key(bon_driver_id, 0x7FE8, 1040, 32736, None)
            .unwrap()
            .unwrap();
        assert!(!disabled.is_enabled);
    }

    #[test]
    fn test_failure_count() {
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();

        let info = create_test_channel(0x7FE8, 1024, 32736);
        let id = db.insert_channel(bon_driver_id, &info).unwrap();

        // Increment
        assert_eq!(db.increment_failure_count(id).unwrap(), 1);
        assert_eq!(db.increment_failure_count(id).unwrap(), 2);

        // Reset
        db.reset_failure_count(id).unwrap();
        let record = db
            .get_channel_by_key(bon_driver_id, 0x7FE8, 1024, 32736, None)
            .unwrap()
            .unwrap();
        assert_eq!(record.failure_count, 0);
    }

    #[test]
    fn test_scan_history() {
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();

        db.insert_scan_history(bon_driver_id, 10, true, None)
            .unwrap();
        db.insert_scan_history(bon_driver_id, 0, false, Some("Timeout"))
            .unwrap();

        let history = db.get_scan_history(bon_driver_id, 10).unwrap();
        assert_eq!(history.len(), 2);
        assert!(!history[0].success); // Most recent first
        assert!(history[1].success);
    }
}
