//! Program (EPG) storage: EIT-collected event data (Migration 015).
//!
//! Rows are written by `crate::epg_writer::EpgWriter`, in batches produced
//! from `tuner::epg_collector::EpgCollector`'s parsed EIT sections. Read by
//! the dashboard API (`web/api/programs.rs`) and the Mirakurun-compatible
//! API (`web/mirakurun.rs::get_programs`).

use super::{Database, EpgSource, ProgramRecord, ProgramUpsert, ProgramUpsertWithClocks, Result};
use crate::guide_services::{self, ProgramNameAt, ServiceMeta, SubchannelInfo};
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;

/// Upper bound for a stored event duration, in seconds.
///
/// EIT carries `duration` as 3 BCD bytes, and the all-ones value 0xFFFFFF
/// means "undefined" (ARIB STD-B10 / EN 300 468). A decoder that BCD-decodes
/// that sentinel byte-wise yields 165:165:165 = 604065s, which renders as a
/// week-long block that swallows every other event in the same service's
/// column of the guide. `ts_analyzer::eit::parse_duration` maps the sentinel
/// to 0; this bound is the second line of defence for rows arriving from any
/// other path (node transport, an older peer). Real broadcast events never
/// approach 24h.
pub const MAX_EVENT_DURATION_SECS: i64 = 86_400;

/// Normalize a duration to the stored range, mapping anything out of bounds
/// to 0 ("length undefined") rather than dropping the event: the event's
/// existence and title are still worth keeping.
fn sanitize_duration(duration_secs: i64) -> i64 {
    if !(0..=MAX_EVENT_DURATION_SECS).contains(&duration_secs) {
        return 0;
    }
    duration_secs
}

// 共有イベントは読み出し時だけ解決する。旧データは参照列がないため、
// 同一 mux・開始時刻の主サービス名を移行措置として併用する。
const PROGRAM_READ_JOINS: &str = r#"
 LEFT JOIN programs shared
   ON shared.nid = p.nid AND shared.tsid = p.tsid
  AND shared.sid = p.shared_sid AND shared.event_id = p.shared_event_id
 LEFT JOIN programs legacy
   ON legacy.id = CASE WHEN p.shared_sid IS NULL AND p.name IS NULL THEN (
        SELECT q.id FROM programs q
         WHERE q.nid = p.nid AND q.tsid = p.tsid
           AND q.start_at = p.start_at AND q.duration_secs = p.duration_secs
           AND q.name IS NOT NULL
         ORDER BY q.sid, q.id LIMIT 1
      ) END
"#;

const PROGRAM_READ_COLUMNS: &str = "SELECT p.id, p.nid, p.sid, p.tsid, p.event_id,
    p.start_at, p.duration_secs,
    COALESCE(p.name, shared.name, legacy.name) AS name,
    COALESCE(p.description, shared.description, legacy.description) AS description,
    COALESCE(p.extended, shared.extended, legacy.extended) AS extended,
    COALESCE(p.genre, shared.genre, legacy.genre) AS genre,
    p.free_ca_mode, p.updated_at";

impl Database {
    pub fn get_enabled_network_ids(&self) -> Result<Vec<u16>> {
        let mut stmt = self
            .connection()
            .prepare("SELECT DISTINCT nid FROM channels WHERE is_enabled = 1 ORDER BY nid")?;
        let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u16))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Batch UPSERT program rows in a single transaction.
    ///
    /// Dedupe key is `(nid, sid, tsid, event_id)` (the table's UNIQUE
    /// constraint, matching Migration 025's schema). On conflict, the existing row is
    /// only overwritten when the incoming `updated_at` is not older than
    /// what's already stored — the "keep the newest" rule from the design:
    /// a batch is flushed periodically, and a slightly stale duplicate must
    /// never clobber a newer row written by an earlier flush.
    pub fn upsert_programs(&mut self, programs: &[ProgramUpsert]) -> Result<usize> {
        self.upsert_programs_inner(programs, None)
    }

    /// Persist a batch and return only rows whose Mirakurun-visible fields changed.
    pub fn upsert_programs_changed(
        &mut self,
        programs: &[ProgramUpsert],
    ) -> Result<Vec<ProgramUpsert>> {
        let mut before = HashMap::new();
        for p in programs {
            let snapshot = self.connection().query_row(
                "SELECT start_at,duration_secs,name,description,extended,genre,free_ca_mode FROM programs WHERE nid=?1 AND sid=?2 AND tsid=?3 AND event_id=?4",
                params![p.nid as i32,p.sid as i32,p.tsid as i32,p.event_id as i32],
                |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,bool>(6)?)),
            ).optional()?;
            before.insert((p.nid, p.sid, p.tsid, p.event_id), snapshot);
        }
        self.upsert_programs(programs)?;
        let mut changed = HashMap::new();
        for p in programs {
            let after = self.connection().query_row(
                "SELECT start_at,duration_secs,name,description,extended,genre,free_ca_mode FROM programs WHERE nid=?1 AND sid=?2 AND tsid=?3 AND event_id=?4",
                params![p.nid as i32,p.sid as i32,p.tsid as i32,p.event_id as i32],
                |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,bool>(6)?)),
            ).optional()?;
            let key = (p.nid, p.sid, p.tsid, p.event_id);
            if before.get(&key) != Some(&after) {
                if let Some((
                    start_at,
                    duration,
                    name,
                    description,
                    extended,
                    genre,
                    free_ca_mode,
                )) = after
                {
                    let mut visible = p.clone();
                    visible.start_at = start_at;
                    visible.duration_secs = duration;
                    visible.name = name;
                    visible.description = description;
                    visible.extended = extended;
                    visible.genre = genre;
                    visible.free_ca_mode = free_ca_mode;
                    changed.insert(key, visible);
                }
            }
        }
        // A flush may contain p/f and schedule inputs for one DB row. Emit at
        // most one event, preferring a named payload (the client drops unnamed).
        Ok(changed.into_values().filter(|p| p.name.is_some()).collect())
    }

    fn upsert_programs_inner(
        &mut self,
        programs: &[ProgramUpsert],
        clocks: Option<&[ProgramUpsertWithClocks]>,
    ) -> Result<usize> {
        if programs.is_empty() {
            return Ok(0);
        }

        let tx = self.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO programs (
                    nid, sid, tsid, event_id, start_at, duration_secs,
                    name, description, extended, genre, free_ca_mode, updated_at,
                    source, pf_updated_at, schedule_updated_at,
                    basic_updated_at, extended_updated_at, shared_sid, shared_event_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                          ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(nid, sid, tsid, event_id) DO UPDATE SET
                    tsid = excluded.tsid,
                    start_at = CASE WHEN (excluded.source = 0 AND
                                      (programs.pf_updated_at IS NULL OR excluded.updated_at >= programs.pf_updated_at))
                                  OR (excluded.source = 1 AND
                                      (programs.schedule_updated_at IS NULL OR excluded.updated_at >= programs.schedule_updated_at))
                                  THEN excluded.start_at ELSE programs.start_at END,
                    duration_secs = CASE WHEN (excluded.source = 0 AND
                                      (programs.pf_updated_at IS NULL OR excluded.updated_at >= programs.pf_updated_at))
                                  OR (excluded.source = 1 AND
                                      (programs.schedule_updated_at IS NULL OR excluded.updated_at >= programs.schedule_updated_at))
                                  THEN excluded.duration_secs ELSE programs.duration_secs END,
                    free_ca_mode = CASE WHEN (excluded.source = 0 AND
                                      (programs.pf_updated_at IS NULL OR excluded.updated_at >= programs.pf_updated_at))
                                  OR (excluded.source = 1 AND
                                      (programs.schedule_updated_at IS NULL OR excluded.updated_at >= programs.schedule_updated_at))
                                  THEN excluded.free_ca_mode ELSE programs.free_ca_mode END,
                    name = CASE WHEN excluded.basic_updated_at IS NOT NULL AND
                                      (programs.basic_updated_at IS NULL OR excluded.basic_updated_at >= programs.basic_updated_at)
                                THEN COALESCE(excluded.name, programs.name) ELSE programs.name END,
                    description = CASE WHEN excluded.basic_updated_at IS NOT NULL AND
                                      (programs.basic_updated_at IS NULL OR excluded.basic_updated_at >= programs.basic_updated_at)
                                THEN COALESCE(excluded.description, programs.description) ELSE programs.description END,
                    genre = CASE WHEN excluded.basic_updated_at IS NOT NULL AND
                                      (programs.basic_updated_at IS NULL OR excluded.basic_updated_at >= programs.basic_updated_at)
                                THEN COALESCE(excluded.genre, programs.genre) ELSE programs.genre END,
                    extended = CASE WHEN excluded.extended_updated_at IS NOT NULL AND
                                      (programs.extended_updated_at IS NULL OR excluded.extended_updated_at >= programs.extended_updated_at)
                                THEN COALESCE(excluded.extended, programs.extended) ELSE programs.extended END,
                    updated_at = MAX(programs.updated_at, excluded.updated_at),
                    source = excluded.source,
                    pf_updated_at = CASE WHEN excluded.source = 0 THEN MAX(COALESCE(programs.pf_updated_at, 0), excluded.updated_at) ELSE programs.pf_updated_at END,
                    schedule_updated_at = CASE WHEN excluded.source = 1 THEN MAX(COALESCE(programs.schedule_updated_at, 0), excluded.updated_at) ELSE programs.schedule_updated_at END,
                    basic_updated_at = CASE WHEN excluded.basic_updated_at IS NOT NULL THEN MAX(COALESCE(programs.basic_updated_at, 0), excluded.basic_updated_at) ELSE programs.basic_updated_at END,
                    extended_updated_at = CASE WHEN excluded.extended_updated_at IS NOT NULL THEN MAX(COALESCE(programs.extended_updated_at, 0), excluded.extended_updated_at) ELSE programs.extended_updated_at END,
                    shared_sid = CASE WHEN excluded.shared_sid IS NOT NULL THEN excluded.shared_sid ELSE programs.shared_sid END,
                    shared_event_id = CASE WHEN excluded.shared_sid IS NOT NULL THEN excluded.shared_event_id ELSE programs.shared_event_id END",
            )?;

            for p in programs {
                stmt.execute(params![
                    p.nid as i32,
                    p.sid as i32,
                    p.tsid as i32,
                    p.event_id as i32,
                    p.start_at,
                    sanitize_duration(p.duration_secs),
                    p.name,
                    p.description,
                    p.extended,
                    p.genre,
                    p.free_ca_mode,
                    p.updated_at,
                    p.source as i64,
                    if p.source == EpgSource::PresentFollowing {
                        Some(p.updated_at)
                    } else {
                        None
                    },
                    if p.source == EpgSource::Schedule {
                        Some(p.updated_at)
                    } else {
                        None
                    },
                    p.basic_updated_at,
                    p.extended_updated_at,
                    p.shared_sid.map(i32::from),
                    p.shared_event_id.map(i32::from),
                ])?;
            }
        }
        if let Some(rows) = clocks {
            let mut clock_stmt = tx.prepare(
                "UPDATE programs SET
                    pf_updated_at = NULLIF(MAX(COALESCE(pf_updated_at, 0), COALESCE(?1, 0)), 0),
                    schedule_updated_at = NULLIF(MAX(COALESCE(schedule_updated_at, 0), COALESCE(?2, 0)), 0)
                 WHERE nid=?3 AND sid=?4 AND tsid=?5 AND event_id=?6",
            )?;
            for row in rows {
                clock_stmt.execute(params![
                    row.pf_updated_at,
                    row.schedule_updated_at,
                    row.program.nid as i32,
                    row.program.sid as i32,
                    row.program.tsid as i32,
                    row.program.event_id as i32,
                ])?;
            }
        }
        tx.commit()?;

        Ok(programs.len())
    }

    /// Apply rows received from a peer while retaining the independent p/f
    /// and schedule update clocks stored in the supplier database.
    pub fn upsert_programs_with_clocks(
        &mut self,
        rows: &[ProgramUpsertWithClocks],
    ) -> Result<usize> {
        let programs: Vec<_> = rows.iter().map(|row| row.program.clone()).collect();
        self.upsert_programs_inner(&programs, Some(rows))
    }

    /// Read one ordered, resumable page for node-to-node EPG synchronization.
    pub fn get_programs_for_node_page(
        &self,
        network_ids: &[u16],
        cursor_updated_at: i64,
        cursor_id: i64,
        now: i64,
        limit: u32,
    ) -> Result<(Vec<ProgramUpsertWithClocks>, bool)> {
        let mut sql = String::from(
            "SELECT id,nid,sid,tsid,event_id,start_at,duration_secs,free_ca_mode,
                    name,description,extended,genre,updated_at,source,
                    basic_updated_at,extended_updated_at,shared_sid,shared_event_id,pf_updated_at,schedule_updated_at
             FROM programs
             WHERE (updated_at > ?1 OR (updated_at = ?1 AND id > ?2))
               AND start_at + duration_secs > ?3",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(cursor_updated_at),
            Box::new(cursor_id),
            Box::new(now),
        ];
        if !network_ids.is_empty() {
            sql.push_str(" AND nid IN (");
            sql.push_str(
                &std::iter::repeat("?")
                    .take(network_ids.len())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            sql.push(')');
            values.extend(
                network_ids
                    .iter()
                    .map(|nid| Box::new(*nid as i32) as Box<dyn rusqlite::ToSql>),
            );
        }
        sql.push_str(" ORDER BY updated_at ASC, id ASC LIMIT ?");
        values.push(Box::new(limit.saturating_add(1) as i64));
        let bound: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        let mut stmt = self.connection().prepare(&sql)?;
        let rows = stmt.query_map(bound.as_slice(), |r| {
            Ok(ProgramUpsertWithClocks {
                id: r.get(0)?,
                program: ProgramUpsert {
                    nid: r.get::<_, i64>(1)? as u16,
                    sid: r.get::<_, i64>(2)? as u16,
                    tsid: r.get::<_, i64>(3)? as u16,
                    event_id: r.get::<_, i64>(4)? as u16,
                    start_at: r.get(5)?,
                    duration_secs: r.get(6)?,
                    free_ca_mode: r.get::<_, i64>(7)? != 0,
                    name: r.get(8)?,
                    description: r.get(9)?,
                    extended: r.get(10)?,
                    genre: r.get(11)?,
                    updated_at: r.get(12)?,
                    source: if r.get::<_, i64>(13)? == EpgSource::Schedule as i64 {
                        EpgSource::Schedule
                    } else {
                        EpgSource::PresentFollowing
                    },
                    basic_updated_at: r.get(14)?,
                    extended_updated_at: r.get(15)?,
                    shared_sid: r.get::<_, Option<i64>>(16)?.map(|v| v as u16),
                    shared_event_id: r.get::<_, Option<i64>>(17)?.map(|v| v as u16),
                },
                pf_updated_at: r.get(18)?,
                schedule_updated_at: r.get(19)?,
            })
        })?;
        let mut rows: Vec<_> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        Ok((rows, has_more))
    }

    /// Query programs whose `[start_at, start_at + duration_secs)` interval
    /// overlaps `[since, until)`, optionally narrowed to a single `nid`
    /// and/or `sid`. Ordered by `start_at` ascending.
    pub fn get_programs(
        &self,
        since: i64,
        until: i64,
        nid: Option<u16>,
        sid: Option<u16>,
    ) -> Result<Vec<ProgramRecord>> {
        let mut sql = format!(
            "{} FROM programs p {} WHERE p.start_at < ? AND (p.start_at + p.duration_secs) > ?",
            PROGRAM_READ_COLUMNS, PROGRAM_READ_JOINS
        );

        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(until), Box::new(since)];
        if let Some(n) = nid {
            sql.push_str(" AND p.nid = ?");
            values.push(Box::new(n as i32));
        }
        if let Some(s) = sid {
            sql.push_str(" AND p.sid = ?");
            values.push(Box::new(s as i32));
        }
        sql.push_str(" ORDER BY p.start_at ASC");

        let mut stmt = self.connection().prepare(&sql)?;
        let bound: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bound.as_slice(), Self::row_to_program_record)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.into())
    }

    pub fn get_programs_page(
        &self,
        since: i64,
        until: i64,
        nid: Option<u16>,
        sid: Option<u16>,
        services: &[(u16, u16)],
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<ProgramRecord>, u64)> {
        let mut where_sql = String::from(
            " FROM programs p WHERE p.start_at < ? AND (p.start_at + p.duration_secs) > ?",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(until), Box::new(since)];
        if let Some(n) = nid {
            where_sql.push_str(" AND p.nid = ?");
            values.push(Box::new(n as i32));
        }
        if let Some(s) = sid {
            where_sql.push_str(" AND p.sid = ?");
            values.push(Box::new(s as i32));
        }
        if !services.is_empty() {
            where_sql.push_str(" AND (p.nid, p.sid) IN (");
            where_sql.push_str(
                &std::iter::repeat("(?, ?)")
                    .take(services.len())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            where_sql.push(')');
            for (n, s) in services {
                values.push(Box::new(*n as i32));
                values.push(Box::new(*s as i32));
            }
        }
        let count_sql = format!("SELECT COUNT(*){}", where_sql);
        let bound: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let total: u64 = self
            .connection()
            .query_row(&count_sql, bound.as_slice(), |row| row.get::<_, i64>(0))?
            as u64;
        let read_where = where_sql.replacen(
            " FROM programs p",
            &format!(" FROM programs p {}", PROGRAM_READ_JOINS),
            1,
        );
        let sql = format!(
            "{}{} ORDER BY p.start_at ASC, p.id ASC LIMIT ? OFFSET ?",
            PROGRAM_READ_COLUMNS, read_where
        );
        values.push(Box::new(limit as i64));
        values.push(Box::new(offset as i64));
        let bound: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let mut stmt = self.connection().prepare(&sql)?;
        let rows = stmt.query_map(bound.as_slice(), Self::row_to_program_record)?;
        Ok((rows.collect::<std::result::Result<Vec<_>, _>>()?, total))
    }

    /// Distinct `(nid, sid)` that have at least one program overlapping
    /// `[since, until)`.
    ///
    /// The guide builds its columns from this, not from the `channels` table:
    /// channels holds every scanned service nationwide (including regions no
    /// tuner can receive), so columns taken from it are mostly empty and the
    /// services that do have EPG end up far off-screen. EDCB's EpgTimer makes
    /// the same cut (`EpgViewBase.cs`: view services ∩ services in the EPG DB).
    pub fn get_program_services(&self, since: i64, until: i64) -> Result<Vec<(u16, u16)>> {
        let mut stmt = self.connection().prepare(
            "SELECT DISTINCT nid, sid FROM programs
             WHERE start_at < ?1 AND (start_at + duration_secs) > ?2
             ORDER BY nid, sid",
        )?;
        let rows = stmt.query_map(params![until, since], |row| {
            Ok((row.get::<_, i32>(0)? as u16, row.get::<_, i32>(1)? as u16))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.into())
    }

    /// Read the service window once and derive subchannel visibility from it.
    pub fn get_program_services_with_subchannels(
        &self,
        since: i64,
        until: i64,
    ) -> Result<(Vec<(u16, u16)>, Vec<SubchannelInfo>)> {
        // Keep this query raw: distinct must inspect the subchannel's own
        // name, before event-sharing metadata is resolved for program reads.
        let mut programs_stmt = self.connection().prepare(
            "SELECT DISTINCT nid, sid, start_at, name FROM programs
             WHERE start_at < ?1 AND (start_at + duration_secs) > ?2",
        )?;
        let programs = programs_stmt
            .query_map(params![until, since], |row| {
                Ok(ProgramNameAt {
                    nid: row.get::<_, i32>(0)? as u16,
                    sid: row.get::<_, i32>(1)? as u16,
                    start_at: row.get(2)?,
                    name: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut services = programs.iter().map(|p| (p.nid, p.sid)).collect::<Vec<_>>();
        services.sort_unstable();
        services.dedup();

        let mut channels_stmt = self.connection().prepare(
            // SID/TSID 0 は物理チャンネルだけの仮行。最小 SID として親に選ばれると
            // 本物のメインがサブ扱いになるので、ここで落とす。
            "SELECT DISTINCT nid, sid, service_type FROM channels
             WHERE sid <> 0 AND tsid <> 0",
        )?;
        let channels = channels_stmt
            .query_map([], |row| {
                Ok(ServiceMeta {
                    nid: row.get::<_, i32>(0)? as u16,
                    sid: row.get::<_, i32>(1)? as u16,
                    service_type: row.get::<_, Option<i32>>(2)?.map(|v| v as u8),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let subchannels = guide_services::subchannels(&channels, &programs)
            .into_iter()
            .filter(|item| services.contains(&item.service))
            .collect();
        Ok((services, subchannels))
    }

    /// Delete programs that ended more than 24h before `now`. Intended to
    /// be called at low frequency from the EPG writer's flush loop, not on
    /// every flush (spec: "収集フラッシュ時に低頻度で呼ぶ").
    pub fn prune_old_programs(&self, now: i64) -> Result<usize> {
        let cutoff = now - 24 * 3600;
        let deleted = self.connection().execute(
            "DELETE FROM programs WHERE (start_at + duration_secs) < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }

    fn row_to_program_record(row: &rusqlite::Row) -> rusqlite::Result<ProgramRecord> {
        Ok(ProgramRecord {
            id: row.get("id")?,
            nid: row.get::<_, i32>("nid")? as u16,
            sid: row.get::<_, i32>("sid")? as u16,
            tsid: row.get::<_, i32>("tsid")? as u16,
            event_id: row.get::<_, i32>("event_id")? as u16,
            start_at: row.get("start_at")?,
            duration_secs: row.get("duration_secs")?,
            name: row.get("name")?,
            description: row.get("description")?,
            extended: row.get("extended")?,
            genre: row.get("genre")?,
            free_ca_mode: row.get::<_, i32>("free_ca_mode")? != 0,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(nid: u16, sid: u16, event_id: u16, start_at: i64, updated_at: i64) -> ProgramUpsert {
        ProgramUpsert {
            nid,
            sid,
            tsid: 100,
            event_id,
            start_at,
            duration_secs: 1800,
            name: Some(format!("Event {}", event_id)),
            description: Some("desc".to_string()),
            extended: None,
            genre: Some(0x21),
            free_ca_mode: false,
            updated_at,
            source: EpgSource::PresentFollowing,
            basic_updated_at: Some(updated_at),
            extended_updated_at: None,
            shared_sid: None,
            shared_event_id: None,
        }
    }

    #[test]
    fn changed_upsert_ignores_clocks_but_detects_visible_name_change() {
        let mut db = Database::open_in_memory().unwrap();
        let original = sample(1, 2, 3, 10_000, 100);
        assert_eq!(
            db.upsert_programs_changed(&[original.clone()])
                .unwrap()
                .len(),
            1
        );
        let duplicate = ProgramUpsert {
            updated_at: 200,
            basic_updated_at: Some(200),
            ..original.clone()
        };
        assert!(db.upsert_programs_changed(&[duplicate]).unwrap().is_empty());
        let changed = ProgramUpsert {
            name: Some("Renamed".into()),
            updated_at: 300,
            basic_updated_at: Some(300),
            ..original
        };
        assert_eq!(db.upsert_programs_changed(&[changed]).unwrap().len(), 1);
    }

    #[test]
    fn test_upsert_and_get_programs() {
        let mut db = Database::open_in_memory().unwrap();

        let batch = vec![
            sample(1, 100, 1, 1_000, 1),
            sample(1, 100, 2, 3_000, 1),
            sample(1, 200, 1, 1_000, 1), // different sid, same event_id -> distinct row
        ];
        assert_eq!(db.upsert_programs(&batch).unwrap(), 3);

        // Overlap window [500, 2000) catches only the first event of sid 100
        // (start=1000, duration=1800 -> ends at 2800) plus the sid=200 one.
        let rows = db.get_programs(500, 2000, None, None).unwrap();
        assert_eq!(rows.len(), 2);

        // Narrow to sid=100 only.
        let rows = db.get_programs(0, 10_000, None, Some(100)).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.sid == 100));

        // Narrow to nid+sid.
        let rows = db.get_programs(0, 10_000, Some(1), Some(200)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, 1);
        assert_eq!(rows[0].sid, 200);
    }

    #[test]
    fn shared_program_metadata_is_resolved_at_read_time() {
        let mut db = Database::open_in_memory().unwrap();
        let mut main = sample(1, 100, 7, 1_000, 1);
        main.description = Some("main description".into());
        main.extended = Some("main extended".into());
        main.genre = Some(0x21);
        let mut sub = sample(1, 200, 8, 1_000, 1);
        sub.name = None;
        sub.description = None;
        sub.extended = None;
        sub.genre = None;
        sub.basic_updated_at = None;
        sub.shared_sid = Some(100);
        sub.shared_event_id = Some(7);
        db.upsert_programs(&[main.clone(), sub]).unwrap();

        let row = db
            .get_programs(0, 10_000, Some(1), Some(200))
            .unwrap()
            .remove(0);
        assert_eq!(row.name.as_deref(), Some("Event 7"));
        assert_eq!(row.description.as_deref(), Some("main description"));
        assert_eq!(row.extended.as_deref(), Some("main extended"));
        assert_eq!(row.genre, Some(0x21));

        main.name = Some("updated main".into());
        main.description = Some("updated description".into());
        main.updated_at = 2;
        main.basic_updated_at = Some(2);
        db.upsert_programs(&[main]).unwrap();
        let row = db
            .get_programs(0, 10_000, Some(1), Some(200))
            .unwrap()
            .remove(0);
        assert_eq!(row.name.as_deref(), Some("updated main"));
        assert_eq!(row.description.as_deref(), Some("updated description"));
    }

    #[test]
    fn shared_program_missing_target_stays_null_and_legacy_rows_are_rescued() {
        let mut db = Database::open_in_memory().unwrap();
        let mut missing = sample(1, 200, 8, 1_000, 1);
        missing.name = None;
        missing.description = None;
        missing.extended = None;
        missing.genre = None;
        missing.basic_updated_at = None;
        missing.shared_sid = Some(100);
        missing.shared_event_id = Some(7);
        db.upsert_programs(&[missing]).unwrap();
        let row = db
            .get_programs(0, 10_000, Some(1), Some(200))
            .unwrap()
            .remove(0);
        assert!(row.name.is_none());

        let mut legacy_sub = sample(2, 200, 9, 2_000, 1);
        legacy_sub.name = None;
        legacy_sub.description = None;
        legacy_sub.extended = None;
        legacy_sub.genre = None;
        legacy_sub.basic_updated_at = None;
        db.upsert_programs(&[legacy_sub]).unwrap();
        let main = sample(2, 100, 10, 2_000, 1);
        db.upsert_programs(&[main]).unwrap();
        let row = db
            .get_programs(0, 10_000, Some(2), Some(200))
            .unwrap()
            .remove(0);
        assert_eq!(row.name.as_deref(), Some("Event 10"));
    }

    #[test]
    fn shared_read_does_not_overwrite_own_metadata() {
        let mut db = Database::open_in_memory().unwrap();
        let main = sample(1, 100, 7, 1_000, 1);
        let mut sub = sample(1, 200, 8, 1_000, 1);
        sub.name = Some("sub-specific".into());
        sub.shared_sid = Some(100);
        sub.shared_event_id = Some(7);
        db.upsert_programs(&[main, sub]).unwrap();
        let row = db
            .get_programs(0, 10_000, Some(1), Some(200))
            .unwrap()
            .remove(0);
        assert_eq!(row.name.as_deref(), Some("sub-specific"));
    }

    #[test]
    fn subchannel_distinct_uses_raw_name_before_shared_resolution() {
        let mut db = Database::open_in_memory().unwrap();
        db.connection()
            .execute_batch(
                "INSERT INTO bon_drivers(id,dll_path) VALUES (1,'test.dll');
                 INSERT INTO channels(bon_driver_id,nid,sid,tsid,service_type)
                 VALUES (1,32744,1024,1,1),(1,32744,1025,1,1);",
            )
            .unwrap();
        let mut parent = sample(32744, 1024, 1, 1_000, 1);
        parent.name = Some("主番組".into());
        let mut sub = sample(32744, 1025, 2, 1_000, 1);
        sub.name = None;
        sub.description = None;
        sub.genre = None;
        sub.basic_updated_at = None;
        sub.shared_sid = Some(1024);
        sub.shared_event_id = Some(1);
        db.upsert_programs(&[parent, sub]).unwrap();
        let (_, subchannels) = db.get_program_services_with_subchannels(0, 10_000).unwrap();
        assert_eq!(subchannels.len(), 1);
        assert!(!subchannels[0].distinct);
    }

    #[test]
    fn node_page_uses_keyset_filters_network_and_ended_rows() {
        let mut db = Database::open_in_memory().unwrap();
        db.upsert_programs(&[
            sample(1, 100, 1, 10_000, 10),
            sample(1, 100, 2, 10_000, 20),
            sample(2, 100, 3, 10_000, 30),
            sample(1, 100, 4, -10_000, 40),
        ])
        .unwrap();
        let (first, more) = db.get_programs_for_node_page(&[1], 0, 0, 100, 1).unwrap();
        assert!(more);
        assert_eq!(first.len(), 1);
        let (second, more) = db
            .get_programs_for_node_page(&[1], first[0].program.updated_at, first[0].id, 100, 10)
            .unwrap();
        assert!(!more);
        assert_eq!(second.len(), 1);
        assert!(second.iter().all(|row| row.program.nid == 1));
        assert!(second.iter().all(|row| row.program.start_at > 1));
    }

    #[test]
    fn page_filters_services_and_has_stable_start_order() {
        let mut db = Database::open_in_memory().unwrap();
        db.upsert_programs(&[
            sample(1, 100, 1, 3_000, 1),
            sample(1, 100, 2, 1_000, 1),
            sample(2, 200, 3, 2_000, 1),
        ])
        .unwrap();
        let (page, total) = db
            .get_programs_page(0, 10_000, None, None, &[(1, 100)], 1, 1)
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].event_id, 1);
    }

    #[test]
    fn program_services_lists_only_services_overlapping_the_window() {
        let mut db = Database::open_in_memory().unwrap();
        db.upsert_programs(&[
            sample(2, 200, 1, 1_000, 1),
            sample(1, 100, 1, 1_000, 1),
            sample(1, 100, 2, 2_000, 1), // same service twice -> one entry
            sample(3, 300, 1, 50_000, 1), // outside the window
        ])
        .unwrap();
        assert_eq!(
            db.get_program_services(0, 10_000).unwrap(),
            vec![(1, 100), (2, 200)]
        );
        assert!(db
            .get_program_services(100_000, 200_000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_upsert_dedupes_by_nid_sid_event_id_keeping_newest() {
        let mut db = Database::open_in_memory().unwrap();

        db.upsert_programs(&[sample(1, 100, 1, 1_000, 5)]).unwrap();
        let rows = db.get_programs(0, 10_000, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].updated_at, 5);

        // Older update (updated_at=3 < 5) must NOT clobber the newer row.
        let mut stale = sample(1, 100, 1, 9_999, 3);
        stale.name = Some("Stale".to_string());
        db.upsert_programs(&[stale]).unwrap();
        let rows = db.get_programs(0, 10_000, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].updated_at, 5);
        assert_eq!(rows[0].start_at, 1_000);

        // Newer update (updated_at=10 >= 5) must overwrite.
        let mut fresh = sample(1, 100, 1, 2_000, 10);
        fresh.name = Some("Fresh".to_string());
        db.upsert_programs(&[fresh]).unwrap();
        let rows = db.get_programs(0, 10_000, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].updated_at, 10);
        assert_eq!(rows[0].start_at, 2_000);
        assert_eq!(rows[0].name, Some("Fresh".to_string()));
    }

    #[test]
    fn test_same_service_event_on_distinct_transport_streams_is_distinct() {
        let mut db = Database::open_in_memory().unwrap();
        let mut a = sample(1, 100, 1, 1_000, 1);
        let mut b = sample(1, 100, 1, 1_000, 1);
        a.tsid = 10;
        b.tsid = 20;
        db.upsert_programs(&[a, b]).unwrap();
        let rows = db.get_programs(0, 10_000, Some(1), Some(100)).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().map(|r| r.tsid).collect::<Vec<_>>(),
            vec![10, 20]
        );
    }

    #[test]
    fn schedule_extended_empty_descriptors_do_not_erase_basic_info() {
        let mut db = Database::open_in_memory().unwrap();
        let mut basic = sample(1, 100, 1, 1_000, 10);
        db.upsert_programs(&[basic.clone()]).unwrap();
        basic.source = EpgSource::Schedule;
        basic.name = None;
        basic.description = None;
        basic.genre = None;
        basic.extended = None;
        basic.basic_updated_at = None;
        basic.extended_updated_at = None;
        basic.updated_at = 11;
        db.upsert_programs(&[basic]).unwrap();
        let row = db.get_programs(0, 10_000, None, None).unwrap().remove(0);
        assert_eq!(row.name.as_deref(), Some("Event 1"));
        assert_eq!(row.description.as_deref(), Some("desc"));
        assert_eq!(row.genre, Some(0x21));
    }

    #[test]
    fn present_following_and_schedule_have_independent_update_clocks() {
        let mut db = Database::open_in_memory().unwrap();
        let mut pf = sample(1, 100, 2, 1_000, 20);
        db.upsert_programs(&[pf.clone()]).unwrap();
        pf.source = EpgSource::Schedule;
        pf.start_at = 2_000;
        pf.name = None;
        pf.description = None;
        pf.genre = None;
        pf.basic_updated_at = None;
        pf.extended_updated_at = None;
        pf.updated_at = 30;
        db.upsert_programs(&[pf.clone()]).unwrap();
        pf.source = EpgSource::PresentFollowing;
        pf.start_at = 3_000;
        pf.updated_at = 40;
        db.upsert_programs(&[pf]).unwrap();
        let row = db.get_programs(0, 10_000, None, None).unwrap().remove(0);
        assert_eq!(row.start_at, 3_000);
        assert_eq!(row.name.as_deref(), Some("Event 2"));
    }

    #[test]
    fn test_prune_old_programs() {
        let db_result = Database::open_in_memory();
        let mut db = db_result.unwrap();

        let now = 100_000i64;
        // Ends well before cutoff (now - 24h).
        let old = sample(1, 100, 1, now - 30 * 3600, 1);
        // Ends after cutoff.
        let recent = sample(1, 100, 2, now - 1000, 1);
        db.upsert_programs(&[old, recent]).unwrap();

        let deleted = db.prune_old_programs(now).unwrap();
        assert_eq!(deleted, 1);

        let rows = db.get_programs(0, now + 1, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, 2);
    }

    #[test]
    fn test_upsert_programs_empty_batch_is_noop() {
        let mut db = Database::open_in_memory().unwrap();
        assert_eq!(db.upsert_programs(&[]).unwrap(), 0);
        assert!(db.get_programs(0, i64::MAX, None, None).unwrap().is_empty());
    }

    /// A week-long duration is the signature of the pre-`a6cf1ab` decoder
    /// BCD-decoding 0xFFFFFF ("undefined") into 165:165:165 = 604065s. Such a
    /// row covers the whole guide column for its service, so it is stored as
    /// "length undefined" (0) instead — the event itself is kept.
    #[test]
    fn test_upsert_clamps_out_of_range_duration() {
        let mut db = Database::open_in_memory().unwrap();

        let mut bogus = sample(1, 100, 1, 1_000, 1);
        bogus.duration_secs = 604_065;
        let mut negative = sample(1, 100, 2, 1_000, 1);
        negative.duration_secs = -1;
        let mut ok = sample(1, 100, 3, 1_000, 1);
        ok.duration_secs = MAX_EVENT_DURATION_SECS;
        db.upsert_programs(&[bogus, negative, ok]).unwrap();

        let rows = db.get_programs(0, i64::MAX, None, None).unwrap();
        let by_event = |id: u16| {
            rows.iter()
                .find(|r| r.event_id == id)
                .unwrap()
                .duration_secs
        };
        assert_eq!(by_event(1), 0);
        assert_eq!(by_event(2), 0);
        assert_eq!(by_event(3), MAX_EVENT_DURATION_SECS);
    }
}
