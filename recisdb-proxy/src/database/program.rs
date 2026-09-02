//! Program (EPG) storage: EIT-collected event data (Migration 015).
//!
//! Rows are written by `crate::epg_writer::EpgWriter`, in batches produced
//! from `tuner::epg_collector::EpgCollector`'s parsed EIT sections. Read by
//! the dashboard API (`web/api/programs.rs`) and the Mirakurun-compatible
//! API (`web/mirakurun.rs::get_programs`).

use super::{Database, EpgSource, ProgramRecord, ProgramUpsert, Result};
use rusqlite::params;

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

impl Database {
    /// Batch UPSERT program rows in a single transaction.
    ///
    /// Dedupe key is `(nid, sid, tsid, event_id)` (the table's UNIQUE
    /// constraint, matching Migration 025's schema). On conflict, the existing row is
    /// only overwritten when the incoming `updated_at` is not older than
    /// what's already stored — the "keep the newest" rule from the design:
    /// a batch is flushed periodically, and a slightly stale duplicate must
    /// never clobber a newer row written by an earlier flush.
    pub fn upsert_programs(&mut self, programs: &[ProgramUpsert]) -> Result<usize> {
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
                    basic_updated_at, extended_updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                          ?13, ?14, ?15, ?16, ?17)
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
                    extended_updated_at = CASE WHEN excluded.extended_updated_at IS NOT NULL THEN MAX(COALESCE(programs.extended_updated_at, 0), excluded.extended_updated_at) ELSE programs.extended_updated_at END",
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
                    if p.source == EpgSource::PresentFollowing { Some(p.updated_at) } else { None },
                    if p.source == EpgSource::Schedule { Some(p.updated_at) } else { None },
                    p.basic_updated_at,
                    p.extended_updated_at,
                ])?;
            }
        }
        tx.commit()?;

        Ok(programs.len())
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
        let mut sql = String::from(
            "SELECT id, nid, sid, tsid, event_id, start_at, duration_secs,
                    name, description, extended, genre, free_ca_mode, updated_at
             FROM programs
             WHERE start_at < ? AND (start_at + duration_secs) > ?",
        );

        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(until), Box::new(since)];
        if let Some(n) = nid {
            sql.push_str(" AND nid = ?");
            values.push(Box::new(n as i32));
        }
        if let Some(s) = sid {
            sql.push_str(" AND sid = ?");
            values.push(Box::new(s as i32));
        }
        sql.push_str(" ORDER BY start_at ASC");

        let mut stmt = self.connection().prepare(&sql)?;
        let bound: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bound.as_slice(), Self::row_to_program_record)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.into())
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
        }
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
