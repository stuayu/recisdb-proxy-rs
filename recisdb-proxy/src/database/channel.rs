//! Channel CRUD operations.

use super::{
    BonDriverRecord, ChannelRecord, ChannelWithDriver, ClientChannelRecord, Database, MergeResult,
    Result, ScanHistoryRecord,
};
use recisdb_protocol::{
    broadcast_region::{get_prefecture_name, get_region_id_from_nid},
    ChannelInfo,
};
use rusqlite::params;
use std::collections::HashSet;

impl Database {
    /// Insert a new channel.
    pub fn insert_channel(&self, bon_driver_id: i64, info: &ChannelInfo) -> Result<i64> {
        // Auto-detect band_type, region_id, and terrestrial_region if not provided
        let bt = info
            .band_type
            .unwrap_or_else(|| recisdb_protocol::BandType::from_nid(info.nid) as u8);
        let region_id = get_region_id_from_nid(info.nid);
        let terrestrial_region = info.terrestrial_region.clone().or_else(|| {
            get_prefecture_name(info.nid).map(|s| s.to_string())
        });

        self.conn.execute(
            "INSERT INTO channels (
                bon_driver_id, nid, sid, tsid, manual_sheet,
                raw_name, channel_name, physical_ch, remote_control_key,
                service_type, network_name, bon_space, bon_channel,
                band_type, region_id, terrestrial_region,
                scan_time, last_seen
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                      ?14, ?15, ?16, strftime('%s', 'now'), strftime('%s', 'now'))",
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
                bt as i32,
                region_id.map(|v| v as i32),
                terrestrial_region,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fill in terrestrial metadata columns that are still NULL, from a NIT
    /// seen on a live stream (`tuner/nit_collector.rs`).
    ///
    /// Rows created by the manual routes (CSV import / `POST /api/channels`)
    /// insert `remote_control_key` / `physical_ch` / `network_name` as NULL,
    /// and nothing else fills them in afterwards — a scan is the only other
    /// source. This is the repair path for them.
    ///
    /// Matching is by `nid` alone rather than `(nid, tsid)`: those manual rows
    /// routinely carry a placeholder tsid, and they are exactly the rows this
    /// exists to fix. A terrestrial network id maps to a single transport
    /// stream, so the wider match cannot reach an unrelated multiplex — the
    /// caller drops satellite entries, where one nid does span many tsids.
    ///
    /// Existing values are never overwritten (`COALESCE`), so a scan result
    /// always outranks a live observation. Returns the number of rows changed.
    pub fn fill_missing_terrestrial_metadata(
        &self,
        nid: u16,
        remote_control_key: Option<u8>,
        physical_ch: Option<u8>,
        network_name: Option<&str>,
    ) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE channels SET
                 remote_control_key = COALESCE(remote_control_key, ?1),
                 physical_ch        = COALESCE(physical_ch, ?2),
                 network_name       = COALESCE(network_name, ?3)
             WHERE nid = ?4
               AND ((remote_control_key IS NULL AND ?1 IS NOT NULL)
                 OR (physical_ch IS NULL AND ?2 IS NOT NULL)
                 OR (network_name IS NULL AND ?3 IS NOT NULL))",
            params![
                remote_control_key.map(|v| v as i32),
                physical_ch.map(|v| v as i32),
                network_name,
                nid as i32,
            ],
        )?;
        Ok(changed)
    }

    /// Get channel by primary key (id).
    pub fn get_channel_by_id(&self, id: i64) -> Result<Option<ChannelRecord>> {
        let mut stmt = self.conn.prepare("SELECT * FROM channels WHERE id = ?1")?;
        match stmt.query_row([id], Self::row_to_channel_record) {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get total channel count.
    pub fn get_total_channel_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM channels", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Get all channels with BonDriver path (for export).
    pub fn get_all_channels_for_export(&self) -> Result<Vec<(ChannelRecord, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.*, bd.dll_path
             FROM channels c
             LEFT JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             ORDER BY c.bon_driver_id, c.nid, c.tsid, c.sid",
        )?;
        let records = stmt
            .query_map([], |row| {
                let ch = Self::row_to_channel_record(row)?;
                let dll: Option<String> = row.get("dll_path").ok();
                Ok((ch, dll))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
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

    /// Get enabled channels by BonDriver ID.
    pub fn get_enabled_channels_by_bon_driver(&self, bon_driver_id: i64) -> Result<Vec<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels WHERE bon_driver_id = ?1 AND is_enabled = 1 ORDER BY priority DESC, nid, tsid, sid",
        )?;

        let records = stmt
            .query_map([bon_driver_id], Self::row_to_channel_record)?
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
        let records = if let Some(s) = sid {
            let mut stmt = self.conn.prepare(
                "SELECT c.*, bd.dll_path, bd.scan_priority
                 FROM channels c
                 JOIN bon_drivers bd ON c.bon_driver_id = bd.id
                 WHERE c.nid = ?1 AND c.tsid = ?2 AND c.sid = ?3 AND c.is_enabled = 1
                 ORDER BY c.priority DESC, bd.scan_priority DESC",
            )?;
            let rows = stmt.query_map(params![nid as i32, tsid as i32, s as i32], |row| {
                Ok(ChannelWithDriver {
                    channel: Self::row_to_channel_record(row)?,
                    bon_driver_path: row.get("dll_path")?,
                    bon_driver_scan_priority: row.get("scan_priority")?,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT c.*, bd.dll_path, bd.scan_priority
                 FROM channels c
                 JOIN bon_drivers bd ON c.bon_driver_id = bd.id
                 WHERE c.nid = ?1 AND c.tsid = ?2 AND c.is_enabled = 1
                 ORDER BY c.priority DESC, bd.scan_priority DESC",
            )?;
            let rows = stmt.query_map(params![nid as i32, tsid as i32], |row| {
                Ok(ChannelWithDriver {
                    channel: Self::row_to_channel_record(row)?,
                    bon_driver_path: row.get("dll_path")?,
                    bon_driver_scan_priority: row.get("scan_priority")?,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        Ok(records)
    }

    /// Look up a channel by `(nid, sid)` alone, ignoring `tsid`.
    ///
    /// Used by the Mirakurun-compatible API (STREAMING_DESIGN.md §7.1),
    /// whose service-id convention (`networkId * 100000 + serviceId`) does
    /// not carry `tsid`. If more than one row shares the same `(nid, sid)`
    /// (e.g. the same service scanned via more than one BonDriver, or the
    /// rare case of a network id spanning multiple transport streams), the
    /// enabled row with the highest `priority` wins; ties are broken by the
    /// lowest `id` for determinism. A disabled row is still returned when no
    /// enabled row matches, so callers get a `Disabled` error instead of a
    /// misleading `NotFound`.
    pub fn get_channel_by_nid_sid(&self, nid: u16, sid: u16) -> Result<Option<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels WHERE nid = ?1 AND sid = ?2
             ORDER BY is_enabled DESC, priority DESC, id ASC LIMIT 1",
        )?;
        match stmt.query_row(params![nid as i32, sid as i32], Self::row_to_channel_record) {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get channel by broadcast service_id alone (any network).
    /// Japanese SID allocations don't collide across terrestrial/BS/CS in
    /// practice; prefer enabled and higher-priority rows when they do.
    pub fn get_channel_by_sid(&self, sid: u16) -> Result<Option<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels WHERE sid = ?1
             ORDER BY is_enabled DESC, priority DESC, id ASC LIMIT 1",
        )?;
        match stmt.query_row(params![sid as i32], Self::row_to_channel_record) {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get every row for a given `(nid, sid)`, not just the top-ranked one.
    ///
    /// The same broadcast service is routinely scanned into more than one
    /// BonDriver's `channels` rows (one row per (BonDriver, service) pair —
    /// see `server/channel_resolve.rs`'s module doc comment), so a caller
    /// that needs every physical tuning target for a service (to hand them
    /// all to `tuner::acquire::acquire` as candidates) cannot use
    /// [`Self::get_channel_by_nid_sid`]'s `LIMIT 1`. Same ordering as that
    /// single-row lookup (`is_enabled DESC, priority DESC, id ASC`), so
    /// `rows.first()` here is exactly what the single-row version would have
    /// returned.
    pub fn get_channels_by_nid_sid(&self, nid: u16, sid: u16) -> Result<Vec<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels WHERE nid = ?1 AND sid = ?2
             ORDER BY is_enabled DESC, priority DESC, id ASC",
        )?;
        let records = stmt
            .query_map(params![nid as i32, sid as i32], Self::row_to_channel_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Get every row for a given broadcast service_id alone (any network),
    /// not just the top-ranked one. See [`Self::get_channels_by_nid_sid`]
    /// (same reasoning, keyed on `sid` alone like [`Self::get_channel_by_sid`]).
    pub fn get_channels_by_sid(&self, sid: u16) -> Result<Vec<ChannelRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM channels WHERE sid = ?1
             ORDER BY is_enabled DESC, priority DESC, id ASC",
        )?;
        let records = stmt
            .query_map(params![sid as i32], Self::row_to_channel_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Get all distinct SIDs for a given NID+TSID combination.
    pub fn get_sids_for_nid_tsid(&self, nid: u16, tsid: u16) -> Result<Vec<u16>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT sid FROM channels
             WHERE nid = ?1 AND tsid = ?2 AND is_enabled = 1
             ORDER BY sid ASC",
        )?;
        let rows = stmt.query_map(params![nid as i32, tsid as i32], |row| {
            let sid: i32 = row.get(0)?;
            Ok(sid as u16)
        })?;
        let sids = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(sids)
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

    /// Get all channels with their BonDriver information (for channel list queries).
    pub fn get_all_channels_with_drivers(
        &self,
    ) -> Result<Vec<(ClientChannelRecord, Option<BonDriverRecord>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.bon_driver_id, c.nid, c.sid, c.tsid,
                    c.channel_name, c.network_name, c.service_type,
                    c.remote_control_key, c.bon_space, c.bon_channel,
                    c.is_enabled, c.priority,
                    bd.id as bd_id, bd.dll_path, bd.driver_name, bd.version,
                    bd.group_name, bd.auto_scan_enabled, bd.scan_interval_hours, bd.scan_priority,
                    bd.last_scan, bd.next_scan_at, bd.passive_scan_enabled, bd.max_instances,
                    bd.created_at as bd_created_at, bd.updated_at as bd_updated_at
             FROM channels c
             LEFT JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             ORDER BY c.priority DESC, c.nid, c.tsid, c.sid",
        )?;

        let rows = stmt.query_map([], |row| {
            let channel = ClientChannelRecord {
                id: row.get("id")?,
                bon_driver_id: row.get("bon_driver_id")?,
                nid: row.get("nid")?,
                sid: row.get("sid")?,
                tsid: row.get("tsid")?,
                service_name: row.get("channel_name")?,
                ts_name: row.get("network_name")?,
                service_type: row.get("service_type")?,
                remote_control_key: row.get("remote_control_key")?,
                space: row.get::<_, Option<i32>>("bon_space")?.unwrap_or(0) as u32,
                channel: row.get::<_, Option<i32>>("bon_channel")?.unwrap_or(0) as u32,
                is_enabled: row.get::<_, i32>("is_enabled")? != 0,
                priority: row.get("priority")?,
            };

            let bon_driver: Option<BonDriverRecord> = match row.get::<_, Option<i64>>("bd_id")? {
                Some(id) => Some(BonDriverRecord {
                    id,
                    dll_path: row.get("dll_path").unwrap_or_default(),
                    driver_name: row.get("driver_name").ok().flatten(),
                    version: row.get("version").ok().flatten(),
                    group_name: row.get("group_name")?,
                    auto_scan_enabled: row.get::<_, Option<i32>>("auto_scan_enabled").ok().flatten().unwrap_or(0) != 0,
                    scan_interval_hours: row.get("scan_interval_hours").unwrap_or(24),
                    scan_priority: row.get("scan_priority").unwrap_or(0),
                    last_scan: row.get("last_scan").ok().flatten(),
                    next_scan_at: row.get("next_scan_at").ok().flatten(),
                    passive_scan_enabled: row.get::<_, Option<i32>>("passive_scan_enabled").ok().flatten().unwrap_or(1) != 0,
                    max_instances: row.get::<_, Option<i32>>("max_instances")?.unwrap_or(1),
                    created_at: row.get("bd_created_at").unwrap_or(0),
                    updated_at: row.get("bd_updated_at").unwrap_or(0),
                }),
                None => None,
            };

            Ok((channel, bon_driver))
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(|e| e.into())
    }

    /// Same as [`Self::get_all_channels_with_drivers`] but filtered to a single
    /// NID+TSID up front via `WHERE c.nid=?1 AND c.tsid=?2`, so callers that only
    /// need the candidates for one transport stream (e.g. group-mode driver
    /// selection in `session.rs`) don't have to pull and scan the full table.
    /// Row shape/mapping/ORDER BY are identical to `get_all_channels_with_drivers`.
    pub fn get_channels_by_nid_tsid(
        &self,
        nid: u16,
        tsid: u16,
    ) -> Result<Vec<(ClientChannelRecord, Option<BonDriverRecord>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.bon_driver_id, c.nid, c.sid, c.tsid,
                    c.channel_name, c.network_name, c.service_type,
                    c.remote_control_key, c.bon_space, c.bon_channel,
                    c.is_enabled, c.priority,
                    bd.id as bd_id, bd.dll_path, bd.driver_name, bd.version,
                    bd.group_name, bd.auto_scan_enabled, bd.scan_interval_hours, bd.scan_priority,
                    bd.last_scan, bd.next_scan_at, bd.passive_scan_enabled, bd.max_instances,
                    bd.created_at as bd_created_at, bd.updated_at as bd_updated_at
             FROM channels c
             LEFT JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             WHERE c.nid = ?1 AND c.tsid = ?2
             ORDER BY c.priority DESC, c.nid, c.tsid, c.sid",
        )?;

        let rows = stmt.query_map(params![nid as i32, tsid as i32], |row| {
            let channel = ClientChannelRecord {
                id: row.get("id")?,
                bon_driver_id: row.get("bon_driver_id")?,
                nid: row.get("nid")?,
                sid: row.get("sid")?,
                tsid: row.get("tsid")?,
                service_name: row.get("channel_name")?,
                ts_name: row.get("network_name")?,
                service_type: row.get("service_type")?,
                remote_control_key: row.get("remote_control_key")?,
                space: row.get::<_, Option<i32>>("bon_space")?.unwrap_or(0) as u32,
                channel: row.get::<_, Option<i32>>("bon_channel")?.unwrap_or(0) as u32,
                is_enabled: row.get::<_, i32>("is_enabled")? != 0,
                priority: row.get("priority")?,
            };

            let bon_driver: Option<BonDriverRecord> = match row.get::<_, Option<i64>>("bd_id")? {
                Some(id) => Some(BonDriverRecord {
                    id,
                    dll_path: row.get("dll_path").unwrap_or_default(),
                    driver_name: row.get("driver_name").ok().flatten(),
                    version: row.get("version").ok().flatten(),
                    group_name: row.get("group_name")?,
                    auto_scan_enabled: row.get::<_, Option<i32>>("auto_scan_enabled").ok().flatten().unwrap_or(0) != 0,
                    scan_interval_hours: row.get("scan_interval_hours").unwrap_or(24),
                    scan_priority: row.get("scan_priority").unwrap_or(0),
                    last_scan: row.get("last_scan").ok().flatten(),
                    next_scan_at: row.get("next_scan_at").ok().flatten(),
                    passive_scan_enabled: row.get::<_, Option<i32>>("passive_scan_enabled").ok().flatten().unwrap_or(1) != 0,
                    max_instances: row.get::<_, Option<i32>>("max_instances")?.unwrap_or(1),
                    created_at: row.get("bd_created_at").unwrap_or(0),
                    updated_at: row.get("bd_updated_at").unwrap_or(0),
                }),
                None => None,
            };

            Ok((channel, bon_driver))
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(|e| e.into())
    }

    /// For each driver in `group_paths`, count how many logical channels
    /// (distinct NID+TSID pairs, enabled only) can be received ONLY by that
    /// driver within the group ("exclusive" channels).
    ///
    /// Used by group-mode driver selection to keep drivers that are the sole
    /// receiver of some channel (e.g. a Tokyo-pointed tuner that alone gets
    /// Tokyo MX) free for those channels: drivers with fewer exclusive
    /// channels are preferred when a channel is receivable on several drivers.
    /// Every path in `group_paths` is present in the returned map (0 if the
    /// driver has no exclusive channels or no channels at all).
    pub fn get_exclusive_channel_counts(
        &self,
        group_paths: &[String],
    ) -> Result<std::collections::HashMap<String, i64>> {
        use std::collections::HashMap;

        let mut counts: HashMap<String, i64> =
            group_paths.iter().map(|p| (p.clone(), 0)).collect();
        if group_paths.is_empty() {
            return Ok(counts);
        }

        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT bd.dll_path, c.nid, c.tsid
             FROM channels c
             JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             WHERE c.is_enabled = 1",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        // (nid, tsid) -> set of group driver paths that carry it.
        let mut carriers: HashMap<(i64, i64), HashSet<&str>> = HashMap::new();
        let all: Vec<(String, i64, i64)> =
            rows.collect::<std::result::Result<Vec<_>, _>>()?;
        for (path, nid, tsid) in &all {
            if let Some(known) = group_paths.iter().find(|p| *p == path) {
                carriers.entry((*nid, *tsid)).or_default().insert(known.as_str());
            }
        }

        for paths in carriers.values() {
            if paths.len() == 1 {
                let sole = *paths.iter().next().unwrap();
                if let Some(c) = counts.get_mut(sole) {
                    *c += 1;
                }
            }
        }

        Ok(counts)
    }

    /// Update channel information.
    pub fn update_channel(&self, bon_driver_id: i64, info: &ChannelInfo) -> Result<()> {
        // Auto-detect band_type, region_id, and terrestrial_region if not provided
        let band_type = info
            .band_type
            .unwrap_or_else(|| recisdb_protocol::BandType::from_nid(info.nid) as u8);
        let region_id = get_region_id_from_nid(info.nid);
        let terrestrial_region = info.terrestrial_region.clone().or_else(|| {
            get_prefecture_name(info.nid).map(|s| s.to_string())
        });

        let sql = if info.manual_sheet.is_some() {
            "UPDATE channels SET
                raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                band_type = ?14, region_id = ?15, terrestrial_region = ?16,
                scan_time = strftime('%s', 'now'), last_seen = strftime('%s', 'now'),
                is_enabled = 1
             WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet = ?13"
        } else {
            "UPDATE channels SET
                raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                band_type = ?13, region_id = ?14, terrestrial_region = ?15,
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
                    band_type as i32,
                    region_id.map(|v| v as i32),
                    terrestrial_region,
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
                    band_type as i32,
                    region_id.map(|v| v as i32),
                    terrestrial_region,
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

    /// Enable a channel.
    pub fn enable_channel(&self, channel_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE channels SET is_enabled = 1 WHERE id = ?1",
            [channel_id],
        )?;
        Ok(())
    }

    /// Update channel fields (name, priority, enabled).
    pub fn update_channel_fields(
        &self,
        channel_id: i64,
        channel_name: Option<&str>,
        priority: Option<i32>,
        is_enabled: Option<bool>,
    ) -> Result<()> {
        self.update_channel_full(channel_id, channel_name, priority, is_enabled, None, None, None, None, None, None)
    }

    /// Update all editable channel fields (full update used by GUI).
    #[allow(clippy::too_many_arguments)]
    pub fn update_channel_full(
        &self,
        channel_id: i64,
        channel_name: Option<&str>,
        priority: Option<i32>,
        is_enabled: Option<bool>,
        bon_driver_id: Option<i64>,
        nid: Option<u16>,
        sid: Option<u16>,
        tsid: Option<u16>,
        bon_space: Option<Option<u32>>,
        bon_channel: Option<Option<u32>>,
    ) -> Result<()> {
        let mut updates = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(name) = channel_name {
            updates.push("channel_name = ?");
            values.push(Box::new(name.to_string()));
        }
        if let Some(p) = priority {
            updates.push("priority = ?");
            values.push(Box::new(p));
        }
        if let Some(e) = is_enabled {
            updates.push("is_enabled = ?");
            values.push(Box::new(if e { 1 } else { 0 }));
        }
        if let Some(bd) = bon_driver_id {
            updates.push("bon_driver_id = ?");
            values.push(Box::new(bd));
        }
        if let Some(v) = nid {
            updates.push("nid = ?");
            values.push(Box::new(v as i32));
        }
        if let Some(v) = sid {
            updates.push("sid = ?");
            values.push(Box::new(v as i32));
        }
        if let Some(v) = tsid {
            updates.push("tsid = ?");
            values.push(Box::new(v as i32));
        }
        if let Some(v) = bon_space {
            updates.push("bon_space = ?");
            values.push(Box::new(v.map(|x| x as i32)));
        }
        if let Some(v) = bon_channel {
            updates.push("bon_channel = ?");
            values.push(Box::new(v.map(|x| x as i32)));
        }

        if updates.is_empty() {
            return Ok(());
        }

        values.push(Box::new(channel_id));
        let sql = format!(
            "UPDATE channels SET {} WHERE id = ?",
            updates.join(", ")
        );

        let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&sql, params.as_slice())?;
        Ok(())
    }

    /// Delete a channel.
    pub fn delete_channel(&self, channel_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM channels WHERE id = ?1",
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

        // Guard: a scan can take minutes, and the driver row may have been
        // deleted (or bulk-replaced with a new id) by the time results come
        // back. Without this check every INSERT below fails with an opaque
        // "FOREIGN KEY constraint failed". Checking inside the transaction
        // makes the merge atomic with respect to a concurrent delete.
        let driver_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM bon_drivers WHERE id = ?1)",
            [bon_driver_id],
            |row| row.get(0),
        )?;
        if !driver_exists {
            return Err(super::DatabaseError::BonDriverNotFound(format!(
                "id={bon_driver_id} (削除/置換済み。スキャン結果を破棄します)"
            )));
        }

        // Get existing channels for this BonDriver
        let existing: Vec<ChannelRecord> = {
            let mut stmt = tx.prepare(
                "SELECT * FROM channels WHERE bon_driver_id = ?1",
            )?;
            let rows = stmt.query_map([bon_driver_id], Self::row_to_channel_record)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
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

            // Auto-detect band_type, region_id, and terrestrial_region
            let band_type = info
                .band_type
                .unwrap_or_else(|| recisdb_protocol::BandType::from_nid(info.nid) as u8);
            let region_id = get_region_id_from_nid(info.nid);
            let terrestrial_region = info.terrestrial_region.clone().or_else(|| {
                get_prefecture_name(info.nid).map(|s| s.to_string())
            });

            if existing_keys.contains(&key) {
                // Update existing
                let sql = if info.manual_sheet.is_some() {
                    "UPDATE channels SET
                        raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                        service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                        band_type = ?14, region_id = ?15, terrestrial_region = ?16,
                        scan_time = strftime('%s', 'now'), last_seen = strftime('%s', 'now'),
                        is_enabled = 1
                     WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 AND manual_sheet = ?13"
                } else {
                    "UPDATE channels SET
                        raw_name = ?5, channel_name = ?6, physical_ch = ?7, remote_control_key = ?8,
                        service_type = ?9, network_name = ?10, bon_space = ?11, bon_channel = ?12,
                        band_type = ?13, region_id = ?14, terrestrial_region = ?15,
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
                            band_type as i32,
                            region_id.map(|v| v as i32),
                            terrestrial_region,
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
                            band_type as i32,
                            region_id.map(|v| v as i32),
                            terrestrial_region,
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
                        band_type, region_id, terrestrial_region,
                        scan_time, last_seen
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                              ?14, ?15, ?16, strftime('%s', 'now'), strftime('%s', 'now'))",
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
                        band_type as i32,
                        region_id.map(|v| v as i32),
                        terrestrial_region,
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
                        "Passive scan: new channel discovered: NID=0x{:04X}, SID=0x{:04X}, TSID=0x{:04X}",
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
             FROM scan_history WHERE bon_driver_id = ?1 ORDER BY id DESC LIMIT ?2",
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

    /// Get distinct tuning spaces for a BonDriver.
    /// Returns space numbers and their names (derived from band_type and terrestrial_region).
    pub fn get_tuning_spaces(&self, bon_driver_id: i64) -> Result<Vec<(u32, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT bon_space, band_type, terrestrial_region
             FROM channels
             WHERE bon_driver_id = ?1 AND bon_space IS NOT NULL AND is_enabled = 1
             ORDER BY bon_space",
        )?;

        let rows = stmt.query_map([bon_driver_id], |row| {
            let space: i32 = row.get(0)?;
            let band_type: Option<i32> = row.get(1)?;
            let terrestrial_region: Option<String> = row.get(2)?;
            let space_name = Self::generate_space_name(band_type, terrestrial_region, space);
            Ok((space as u32, space_name))
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.into())
    }

    /// Get tuning space name by space number.
    pub fn get_tuning_space_name(&self, bon_driver_id: i64, space: u32) -> Result<Option<String>> {
        let result: std::result::Result<(Option<i32>, Option<String>), _> = self.conn.query_row(
            "SELECT band_type, terrestrial_region
             FROM channels
             WHERE bon_driver_id = ?1 AND bon_space = ?2 AND is_enabled = 1
             LIMIT 1",
            params![bon_driver_id, space as i32],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match result {
            Ok((band_type, terrestrial_region)) => {
                Ok(Some(Self::generate_space_name(band_type, terrestrial_region, space as i32)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Generate a space name from band_type and terrestrial_region.
    fn generate_space_name(band_type: Option<i32>, terrestrial_region: Option<String>, space: i32) -> String {
        match band_type {
            Some(0) => {
                // Terrestrial - use region name
                terrestrial_region.unwrap_or_else(|| "地上波".to_string())
            }
            Some(1) => "BS".to_string(),
            Some(2) => "CS".to_string(),
            Some(3) => "4K".to_string(),
            Some(4) => "その他".to_string(),
            Some(5) => "CATV".to_string(),
            Some(6) => "SKY".to_string(),
            _ => format!("Space {}", space),
        }
    }

    /// Get channel names for a specific space.
    /// Returns (channel_number, channel_name) pairs.
    pub fn get_channel_names(&self, bon_driver_id: i64, space: u32) -> Result<Vec<(u32, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT bon_channel, COALESCE(channel_name, raw_name, 'Ch ' || bon_channel)
             FROM channels
             WHERE bon_driver_id = ?1 AND bon_space = ?2 AND bon_channel IS NOT NULL AND is_enabled = 1
             ORDER BY bon_channel",
        )?;

        let rows = stmt.query_map(params![bon_driver_id, space as i32], |row| {
            let channel: i32 = row.get(0)?;
            let name: String = row.get(1)?;
            Ok((channel as u32, name))
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.into())
    }

    /// Get channel name by space and channel number.
    pub fn get_channel_name(
        &self,
        bon_driver_id: i64,
        space: u32,
        channel: u32,
    ) -> Result<Option<String>> {
        let result: std::result::Result<String, _> = self.conn.query_row(
            "SELECT COALESCE(channel_name, raw_name, 'Ch ' || bon_channel)
             FROM channels
             WHERE bon_driver_id = ?1 AND bon_space = ?2 AND bon_channel = ?3 AND is_enabled = 1
             LIMIT 1",
            params![bon_driver_id, space as i32, channel as i32],
            |row| row.get(0),
        );

        match result {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get channel priority by tuner path, space, and channel.
    pub fn get_channel_priority(
        &self,
        bon_driver_path: &str,
        space: u32,
        channel: u32,
    ) -> Result<Option<i32>> {
        let result: std::result::Result<i32, _> = self.conn.query_row(
            "SELECT c.priority
             FROM channels c
             JOIN bon_drivers bd ON c.bon_driver_id = bd.id
             WHERE bd.dll_path = ?1 AND c.bon_space = ?2 AND c.bon_channel = ?3 AND c.is_enabled = 1
             LIMIT 1",
            params![bon_driver_path, space as i32, channel as i32],
            |row| row.get(0),
        );

        match result {
            Ok(priority) => Ok(Some(priority)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
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
            remote_control_key: row.get::<_, Option<i32>>("remote_control_key")?.map(|v| v as u16),
            service_type: row.get::<_, Option<i32>>("service_type")?.map(|v| v as u8),
            network_name: row.get("network_name")?,
            bon_space: row.get::<_, Option<i32>>("bon_space")?.map(|v| v as u32),
            bon_channel: row.get::<_, Option<i32>>("bon_channel")?.map(|v| v as u32),
            band_type: row.get::<_, Option<i32>>("band_type")?.map(|v| v as u8),
            region_id: row.get::<_, Option<i32>>("region_id")?.map(|v| v as u8),
            terrestrial_region: row.get("terrestrial_region")?,
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
    fn fill_missing_terrestrial_metadata_only_fills_nulls() {
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();

        // 手動登録相当: リモコンキー・物理チャンネル・ネットワーク名が NULL。
        // tsid は nid と同値のプレースホルダ (CSV インポートで実際に起きる形)。
        let manual = create_test_channel(0x7E87, 23608, 0x7E87);
        db.insert_channel(bon_driver_id, &manual).unwrap();

        // スキャン済み相当: 既に値が入っている別サービス。
        let mut scanned = create_test_channel(0x7E87, 23610, 0x7E87);
        scanned.remote_control_key = Some(5);
        scanned.physical_ch = Some(20);
        scanned.network_name = Some("スキャン済み".to_string());
        db.insert_channel(bon_driver_id, &scanned).unwrap();

        let changed = db
            .fill_missing_terrestrial_metadata(0x7E87, Some(9), Some(16), Some("ＴＯＫＹＯ　ＭＸ"))
            .unwrap();
        assert_eq!(changed, 1, "欠損している行だけが更新される");

        let filled = db
            .get_channel_by_key(bon_driver_id, 0x7E87, 23608, 0x7E87, None)
            .unwrap()
            .unwrap();
        assert_eq!(filled.remote_control_key, Some(9));
        assert_eq!(filled.physical_ch, Some(16));
        assert_eq!(filled.network_name, Some("ＴＯＫＹＯ　ＭＸ".to_string()));

        // 既存値はスキャン由来が優先。上書きしない
        let untouched = db
            .get_channel_by_key(bon_driver_id, 0x7E87, 23610, 0x7E87, None)
            .unwrap()
            .unwrap();
        assert_eq!(untouched.remote_control_key, Some(5));
        assert_eq!(untouched.physical_ch, Some(20));
        assert_eq!(untouched.network_name, Some("スキャン済み".to_string()));

        // 2 回目は変更なし (NitWriter がここで打ち切れる)
        assert_eq!(
            db.fill_missing_terrestrial_metadata(0x7E87, Some(9), Some(16), Some("ＴＯＫＹＯ　ＭＸ"))
                .unwrap(),
            0
        );
    }

    #[test]
    fn fill_missing_terrestrial_metadata_ignores_other_networks() {
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();
        db.insert_channel(bon_driver_id, &create_test_channel(0x7E87, 23608, 0x7E87))
            .unwrap();

        assert_eq!(
            db.fill_missing_terrestrial_metadata(0x7E88, Some(1), None, None).unwrap(),
            0
        );
        let record = db
            .get_channel_by_key(bon_driver_id, 0x7E87, 23608, 0x7E87, None)
            .unwrap()
            .unwrap();
        assert_eq!(record.remote_control_key, None);
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
    fn test_get_channels_by_nid_tsid() {
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();
        // group_name and max_instances must round-trip through
        // get_channels_by_nid_tsid: the SELECT previously omitted
        // bd.group_name/bd.max_instances while the row mapper still read
        // them, so every row silently came back with group_name=None and
        // max_instances=1 regardless of the stored value.
        db.set_group_name(bon_driver_id, Some("GroupX")).unwrap();
        db.update_bon_driver_max_instances(bon_driver_id, 4).unwrap();

        // Two channels with different NID (and different TSID) so we can
        // verify the WHERE nid=?/tsid=? filter narrows to a single group.
        let info_a = create_test_channel(0x7FE8, 1024, 32736);
        let info_b = create_test_channel(0x7FE1, 2048, 16400);
        db.insert_channel(bon_driver_id, &info_a).unwrap();
        db.insert_channel(bon_driver_id, &info_b).unwrap();

        let rows = db.get_channels_by_nid_tsid(0x7FE8, 32736).unwrap();
        assert_eq!(rows.len(), 1);
        let (channel, driver) = &rows[0];
        assert_eq!(channel.nid, 0x7FE8);
        assert_eq!(channel.tsid, 32736);
        assert_eq!(channel.sid, 1024);
        let driver = driver.as_ref().unwrap();
        assert_eq!(driver.dll_path, "Test.dll");
        assert_eq!(driver.group_name.as_deref(), Some("GroupX"));
        assert_eq!(driver.max_instances, 4);

        // A NID+TSID pair with no matching rows returns an empty result.
        let none = db.get_channels_by_nid_tsid(0xFFFF, 0xFFFF).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn test_get_all_channels_with_drivers_includes_group_and_max_instances() {
        // Same regression as test_get_channels_by_nid_tsid, but for the
        // sibling query used by the full (unfiltered) channel listing.
        let db = Database::open_in_memory().unwrap();
        let bon_driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();
        db.set_group_name(bon_driver_id, Some("GroupY")).unwrap();
        db.update_bon_driver_max_instances(bon_driver_id, 2).unwrap();
        db.insert_channel(bon_driver_id, &create_test_channel(0x7FE8, 1024, 32736))
            .unwrap();

        let rows = db.get_all_channels_with_drivers().unwrap();
        assert_eq!(rows.len(), 1);
        let driver = rows[0].1.as_ref().unwrap();
        assert_eq!(driver.group_name.as_deref(), Some("GroupY"));
        assert_eq!(driver.max_instances, 2);
    }

    #[test]
    fn test_get_channels_by_nid_sid_returns_all_rows_ordered() {
        let db = Database::open_in_memory().unwrap();
        let driver_a = db.get_or_create_bon_driver("A.dll").unwrap();
        let driver_b = db.get_or_create_bon_driver("B.dll").unwrap();

        // Same (nid, sid) scanned on two drivers, differing priority.
        let id_a = db.insert_channel(driver_a, &create_test_channel(1, 100, 200)).unwrap();
        let id_b = db.insert_channel(driver_b, &create_test_channel(1, 100, 201)).unwrap();
        db.update_channel_fields(id_a, None, Some(0), None).unwrap();
        db.update_channel_fields(id_b, None, Some(10), None).unwrap();

        let rows = db.get_channels_by_nid_sid(1, 100).unwrap();
        assert_eq!(rows.len(), 2);
        // priority DESC first.
        assert_eq!(rows[0].id, id_b);
        assert_eq!(rows[1].id, id_a);

        // Single-row lookup must agree with rows[0] (same ORDER BY).
        let single = db.get_channel_by_nid_sid(1, 100).unwrap().unwrap();
        assert_eq!(single.id, rows[0].id);

        assert!(db.get_channels_by_nid_sid(1, 999).unwrap().is_empty());
    }

    #[test]
    fn test_get_channels_by_sid_returns_all_rows_ordered() {
        let db = Database::open_in_memory().unwrap();
        let driver_a = db.get_or_create_bon_driver("A.dll").unwrap();
        let driver_b = db.get_or_create_bon_driver("B.dll").unwrap();

        let id_a = db.insert_channel(driver_a, &create_test_channel(1, 100, 200)).unwrap();
        let id_b = db.insert_channel(driver_b, &create_test_channel(2, 100, 300)).unwrap();
        db.disable_channel(id_a).unwrap();

        let rows = db.get_channels_by_sid(100).unwrap();
        assert_eq!(rows.len(), 2);
        // is_enabled DESC first: the still-enabled row (id_b) leads.
        assert_eq!(rows[0].id, id_b);
        assert_eq!(rows[1].id, id_a);

        let single = db.get_channel_by_sid(100).unwrap().unwrap();
        assert_eq!(single.id, rows[0].id);

        assert!(db.get_channels_by_sid(9999).unwrap().is_empty());
    }

    #[test]
    fn test_get_exclusive_channel_counts() {
        let db = Database::open_in_memory().unwrap();
        let tokyo = db.get_or_create_bon_driver("Tokyo.dll").unwrap();
        let gunma = db.get_or_create_bon_driver("Gunma.dll").unwrap();
        let idle = db.get_or_create_bon_driver("Idle.dll").unwrap();
        let _ = idle;

        // Tokyo MX: only the Tokyo tuner carries it.
        db.insert_channel(tokyo, &create_test_channel(0x7FE6, 23608, 23608))
            .unwrap();
        // テレ東相当: carried by both tuners (same NID+TSID, different SIDs
        // must still count as ONE logical channel per driver).
        db.insert_channel(tokyo, &create_test_channel(0x7FE8, 1024, 32736))
            .unwrap();
        db.insert_channel(tokyo, &create_test_channel(0x7FE8, 1025, 32736))
            .unwrap();
        db.insert_channel(gunma, &create_test_channel(0x7FE8, 1024, 32736))
            .unwrap();
        // 群馬テレビ相当: only the Gunma tuner — but disabled, so it must
        // not count.
        let gtv = db
            .insert_channel(gunma, &create_test_channel(0x7FD1, 3088, 30256))
            .unwrap();
        db.disable_channel(gtv).unwrap();

        let group = vec![
            "Tokyo.dll".to_string(),
            "Gunma.dll".to_string(),
            "Idle.dll".to_string(),
        ];
        let counts = db.get_exclusive_channel_counts(&group).unwrap();
        assert_eq!(counts.get("Tokyo.dll"), Some(&1)); // MX のみ
        assert_eq!(counts.get("Gunma.dll"), Some(&0)); // 共通chと無効chのみ
        assert_eq!(counts.get("Idle.dll"), Some(&0)); // チャンネルなしでも0で存在

        // Empty group: empty map, no error.
        assert!(db.get_exclusive_channel_counts(&[]).unwrap().is_empty());
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

    /// A scan can outlive its driver row (deleted/bulk-replaced mid-scan).
    /// The merge must fail with a clear BonDriverNotFound instead of an
    /// opaque FOREIGN KEY error from the first INSERT.
    #[test]
    fn test_merge_scan_results_rejects_missing_driver() {
        let mut db = Database::open_in_memory().unwrap();
        let channels = vec![create_test_channel(0x7FE8, 1024, 32736)];
        let err = db.merge_scan_results(9999, &channels).unwrap_err();
        assert!(
            matches!(err, super::super::DatabaseError::BonDriverNotFound(_)),
            "expected BonDriverNotFound, got: {err:?}"
        );
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
