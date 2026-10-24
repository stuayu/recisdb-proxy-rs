//! Persistent EIT-section coverage and mux aggregation.

use crate::database::{
    Database, EpgMuxCoverage, EpgScanStatus, EpgServiceCoverage, EpgServiceCoverageUpsert, Result,
};
use recisdb_protocol::broadcast_region::is_real_broadcast_service;
use rusqlite::params;

impl Database {
    /// EIT section 由来のサービス coverage を UPSERT する。
    pub fn upsert_epg_service_coverage(&self, rows: &[EpgServiceCoverageUpsert]) -> Result<usize> {
        let mut changed = 0;
        for row in rows {
            if !is_real_broadcast_service(row.service_id, row.tsid) {
                continue;
            }
            changed += self.connection().execute(
                "INSERT INTO epg_service_coverage (
                    network_id, tsid, service_id, pf_complete,
                    schedule_basic_complete, schedule_extended_complete,
                    coverage_until, sections_seen, last_section_at, updated_at,
                    last_complete_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                    CASE WHEN ?5 != 0 THEN ?10 ELSE NULL END)
                ON CONFLICT(network_id, tsid, service_id) DO UPDATE SET
                    pf_complete = excluded.pf_complete,
                    schedule_basic_complete = excluded.schedule_basic_complete,
                    schedule_extended_complete = excluded.schedule_extended_complete,
                    -- スキャンのたびに tracker は作り直される。途中で打ち切った
                    -- スキャンは前回より狭い範囲しか見ていないので、まだ未来を
                    -- 指している前回値を狭い値や NULL で潰さない。
                    coverage_until = CASE
                        WHEN epg_service_coverage.coverage_until IS NOT NULL
                             AND epg_service_coverage.coverage_until > excluded.updated_at
                             AND (excluded.coverage_until IS NULL
                                  OR excluded.coverage_until
                                     < epg_service_coverage.coverage_until)
                        THEN epg_service_coverage.coverage_until
                        ELSE excluded.coverage_until END,
                    sections_seen = excluded.sections_seen,
                    last_section_at = excluded.last_section_at,
                    updated_at = excluded.updated_at,
                    last_complete_at = CASE
                        WHEN excluded.schedule_basic_complete != 0
                        THEN excluded.updated_at
                        ELSE epg_service_coverage.last_complete_at
                    END",
                params![
                    row.network_id,
                    row.tsid,
                    row.service_id,
                    i64::from(row.pf_complete),
                    i64::from(row.schedule_basic_complete),
                    i64::from(row.schedule_extended_complete),
                    row.coverage_until,
                    row.sections_seen,
                    row.last_section_at,
                    row.updated_at,
                ],
            )?;
        }
        Ok(changed)
    }

    /// 指定 mux のサービス coverage 一覧(service_id 昇順)。
    pub fn get_epg_service_coverage(
        &self,
        network_id: u16,
        tsid: u16,
    ) -> Result<Vec<EpgServiceCoverage>> {
        let mut stmt = self.connection().prepare(
            "SELECT network_id, tsid, service_id, pf_complete,
                    schedule_basic_complete, schedule_extended_complete,
                    coverage_until, sections_seen, last_section_at,
                    last_complete_at, updated_at
             FROM epg_service_coverage
             WHERE network_id = ?1 AND tsid = ?2
             ORDER BY service_id",
        )?;
        let rows = stmt
            .query_map(params![network_id, tsid], |row| {
                Ok(EpgServiceCoverage {
                    network_id: row.get::<_, i64>(0)? as u16,
                    tsid: row.get::<_, i64>(1)? as u16,
                    service_id: row.get::<_, i64>(2)? as u16,
                    pf_complete: row.get::<_, i64>(3)? != 0,
                    schedule_basic_complete: row.get::<_, i64>(4)? != 0,
                    schedule_extended_complete: row.get::<_, i64>(5)? != 0,
                    coverage_until: row.get(6)?,
                    sections_seen: row.get(7)?,
                    last_section_at: row.get(8)?,
                    last_complete_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 全 mux の集約。母数は channels の番組表対象サービスを使う。
    pub fn get_epg_mux_coverage(&self) -> Result<Vec<EpgMuxCoverage>> {
        let mut stmt = self.connection().prepare(
            "WITH target_services AS (
                 SELECT nid AS network_id, tsid, sid AS service_id
                 FROM channels
                 -- Keep this in sync with
                 -- recisdb_protocol::broadcast_region::is_real_broadcast_service:
                 -- sid != 0 AND tsid != 0.
                 WHERE (service_type IN (1, 2) OR service_type IS NULL)
                   AND sid != 0 AND tsid != 0
                 GROUP BY nid, tsid, sid
             ), joined AS (
                 -- 母数は channels 側の対象サービスだけ。coverage 行しか無い
                 -- (= まだスキャンしていない other-TS 由来の) サービスを
                 -- 母数に混ぜると coverage を勝手に悪化させてしまう。
                 -- `c.*` を展開せず列を明示するのは、network_id/tsid/service_id が
                 -- 両側に存在して外側の GROUP BY が曖昧になるため。
                 SELECT t.network_id            AS network_id,
                        t.tsid                  AS tsid,
                        t.service_id            AS service_id,
                        c.schedule_basic_complete AS schedule_basic_complete,
                        c.coverage_until        AS coverage_until,
                        c.last_section_at       AS last_section_at,
                        c.last_complete_at      AS last_complete_at
                 FROM target_services t
                 LEFT JOIN epg_service_coverage c
                   ON c.network_id = t.network_id
                  AND c.tsid = t.tsid
                  AND c.service_id = t.service_id
             )
             SELECT network_id, tsid,
                    COUNT(service_id),
                    COALESCE(SUM(CASE WHEN last_complete_at IS NOT NULL
                                           AND coverage_until IS NOT NULL
                                      THEN 1 ELSE 0 END), 0),
                    CASE WHEN COUNT(service_id) = 0
                              OR SUM(CASE WHEN coverage_until IS NULL
                                          THEN 1 ELSE 0 END) != 0
                         THEN NULL ELSE MIN(coverage_until) END,
                    MAX(last_section_at),
                    -- mux の完了時刻は「全対象サービスが一度は完成した」ときの
                    -- 最も古い完成時刻。直近スキャンのフラグで見ると、途中で
                    -- 打ち切ったスキャン1回で完了済みの mux が未完了へ戻る。
                    CASE WHEN COUNT(service_id) > 0
                               AND SUM(CASE WHEN last_complete_at IS NOT NULL
                                                  AND coverage_until IS NOT NULL
                                             THEN 1 ELSE 0 END) = COUNT(service_id)
                         THEN MIN(last_complete_at)
                         ELSE NULL END
             FROM joined
             GROUP BY network_id, tsid
             ORDER BY network_id, tsid",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(EpgMuxCoverage {
                    network_id: row.get::<_, i64>(0)? as u16,
                    tsid: row.get::<_, i64>(1)? as u16,
                    services_total: row.get(2)?,
                    services_complete: row.get(3)?,
                    coverage_until: row.get(4)?,
                    last_section_at: row.get(5)?,
                    last_complete_at: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 集約したsection coverageを `epg_scan_states` に反映する。
    pub fn refresh_epg_section_coverage(&self) -> Result<usize> {
        let rows = self.get_epg_mux_coverage()?;
        let mut changed = 0;
        for row in rows {
            changed += self.connection().execute(
                "INSERT INTO epg_scan_states (
                    network_id, tsid, section_coverage_until, services_total,
                    services_complete, last_complete_at, last_eit_received_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                -- ペアノードから受け取った coverage (set_epg_remote_coverage /
                -- merge_epg_remote_coverage) を、ローカル集計で後退させない。
                -- ローカルに section が無い mux は NULL になるし、以前ローカルで
                -- 受信したことがある mux も古い値になる。まだ未来を指している
                -- 既存値は、より大きいローカル値が出たときだけ置き換える。
                ON CONFLICT(network_id, tsid) DO UPDATE SET
                    section_coverage_until = CASE
                        WHEN epg_scan_states.section_coverage_until IS NOT NULL
                             AND epg_scan_states.section_coverage_until
                                 > CAST(strftime('%s', 'now') AS INTEGER)
                             AND (excluded.section_coverage_until IS NULL
                                  OR excluded.section_coverage_until
                                     < epg_scan_states.section_coverage_until)
                        THEN epg_scan_states.section_coverage_until
                        ELSE excluded.section_coverage_until END,
                    services_total = excluded.services_total,
                    services_complete = excluded.services_complete,
                    last_complete_at = CASE
                        WHEN excluded.last_complete_at IS NULL
                        THEN epg_scan_states.last_complete_at
                        ELSE MAX(COALESCE(epg_scan_states.last_complete_at, 0),
                                 excluded.last_complete_at) END,
                    last_eit_received_at = CASE
                        WHEN excluded.last_eit_received_at IS NULL
                        THEN epg_scan_states.last_eit_received_at
                        ELSE MAX(COALESCE(epg_scan_states.last_eit_received_at, 0),
                                 excluded.last_eit_received_at) END",
                params![
                    row.network_id,
                    row.tsid,
                    row.coverage_until,
                    row.services_total,
                    row.services_complete,
                    row.last_complete_at,
                    row.last_section_at,
                ],
            )?;
        }
        Ok(changed)
    }

    /// 直近スキャン結果を記録する。
    pub fn set_epg_scan_status(
        &self,
        network_id: u16,
        tsid: u16,
        status: EpgScanStatus,
        at: i64,
    ) -> Result<()> {
        self.connection().execute(
            "INSERT INTO epg_scan_states (
                 network_id, tsid, last_scan_status, last_scan_completed_at
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(network_id, tsid) DO UPDATE SET
                 last_scan_status = excluded.last_scan_status,
                 last_scan_completed_at = ?4",
            params![network_id, tsid, status.as_str(), at],
        )?;
        Ok(())
    }

    /// Remote metadata has no local EIT sections. Store its mux-level coverage
    /// in scan state without fabricating rows in `epg_service_coverage`.
    pub fn set_epg_remote_coverage(
        &self,
        network_id: u16,
        tsid: u16,
        coverage_until: Option<i64>,
        at: i64,
    ) -> Result<()> {
        self.connection().execute(
            "INSERT INTO epg_scan_states (
                 network_id, tsid, section_coverage_until,
                 last_complete_at, last_eit_received_at
             ) VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(network_id, tsid) DO UPDATE SET
                 section_coverage_until = excluded.section_coverage_until,
                 last_complete_at = excluded.last_complete_at,
                 last_eit_received_at = excluded.last_eit_received_at",
            params![network_id, tsid, coverage_until, at],
        )?;
        Ok(())
    }

    /// Merge peer coverage without allowing a stale peer response to regress
    /// locally observed section progress.
    pub fn merge_epg_remote_coverage(
        &self,
        network_id: u16,
        tsid: u16,
        section_coverage_until: Option<i64>,
        last_eit_received_at: Option<i64>,
        last_complete_at: Option<i64>,
    ) -> Result<()> {
        self.connection().execute(
            "INSERT INTO epg_scan_states(network_id,tsid,section_coverage_until,last_eit_received_at,last_complete_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(network_id,tsid) DO UPDATE SET
               section_coverage_until = CASE WHEN excluded.section_coverage_until IS NULL THEN epg_scan_states.section_coverage_until ELSE MAX(COALESCE(epg_scan_states.section_coverage_until, 0), excluded.section_coverage_until) END,
               last_eit_received_at = CASE WHEN excluded.last_eit_received_at IS NULL THEN epg_scan_states.last_eit_received_at ELSE MAX(COALESCE(epg_scan_states.last_eit_received_at, 0), excluded.last_eit_received_at) END,
               last_complete_at = CASE WHEN excluded.last_complete_at IS NULL THEN epg_scan_states.last_complete_at ELSE MAX(COALESCE(epg_scan_states.last_complete_at, 0), excluded.last_complete_at) END",
            params![network_id, tsid, section_coverage_until, last_eit_received_at, last_complete_at],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn driver_id(db: &Database) -> i64 {
        db.connection()
            .execute("INSERT INTO bon_drivers (dll_path) VALUES ('test')", [])
            .unwrap();
        db.connection().last_insert_rowid()
    }

    fn channel(db: &Database, driver_id: i64, nid: u16, tsid: u16, sid: u16, kind: Option<u8>) {
        db.connection()
            .execute(
                "INSERT INTO channels (bon_driver_id, nid, sid, tsid, service_type)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![driver_id, nid, sid, tsid, kind.map(i64::from)],
            )
            .unwrap();
    }

    fn coverage(
        nid: u16,
        tsid: u16,
        sid: u16,
        until: Option<i64>,
        complete: bool,
    ) -> EpgServiceCoverageUpsert {
        EpgServiceCoverageUpsert {
            network_id: nid,
            tsid,
            service_id: sid,
            pf_complete: complete,
            schedule_basic_complete: complete,
            schedule_extended_complete: complete,
            coverage_until: until,
            sections_seen: 3,
            last_section_at: Some(10),
            updated_at: 20,
        }
    }

    #[test]
    fn migration_creates_coverage_table_and_columns() {
        let db = db();
        assert!(db
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='epg_service_coverage')",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap());
        assert!(db
            .connection()
            .prepare("SELECT section_coverage_until FROM epg_scan_states")
            .is_ok());
    }

    #[test]
    fn migrations_replay_from_user_version_zero() {
        let db = db();
        db.connection()
            .execute(
                "INSERT INTO epg_service_coverage (network_id, tsid, service_id)
                 VALUES (1, 2, 3)",
                [],
            )
            .unwrap();
        db.connection()
            .execute_batch("PRAGMA user_version = 0")
            .unwrap();
        db.apply_migrations().unwrap();
        assert_eq!(
            db.connection()
                .query_row("SELECT COUNT(*) FROM epg_service_coverage", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn mux_coverage_is_minimum_across_target_services() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        channel(&db, driver, 1, 2, 11, Some(1));
        db.upsert_epg_service_coverage(&[
            coverage(1, 2, 10, Some(100), true),
            coverage(1, 2, 11, Some(50), true),
        ])
        .unwrap();
        assert_eq!(
            db.get_epg_mux_coverage().unwrap()[0].coverage_until,
            Some(50)
        );
    }

    #[test]
    fn mux_coverage_is_none_when_any_target_service_is_missing() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        channel(&db, driver, 1, 2, 11, Some(1));
        db.upsert_epg_service_coverage(&[
            coverage(1, 2, 10, Some(100), true),
            coverage(1, 2, 11, None, false),
        ])
        .unwrap();
        let mux = &db.get_epg_mux_coverage().unwrap()[0];
        assert_eq!(mux.coverage_until, None);
        assert_eq!(mux.last_complete_at, None);
    }

    #[test]
    fn null_service_type_without_coverage_is_an_incomplete_target() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, None);
        channel(&db, driver, 1, 2, 11, None);
        db.upsert_epg_service_coverage(&[coverage(1, 2, 10, Some(100), true)])
            .unwrap();
        let mux = db.get_epg_mux_coverage().unwrap();
        assert_eq!(mux.len(), 1);
        assert_eq!(mux[0].services_total, 2);
        assert_eq!(mux[0].services_complete, 1);
        assert_eq!(mux[0].coverage_until, None);
    }

    #[test]
    fn refresh_keeps_peer_coverage_for_mux_without_local_sections() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        let future = chrono::Utc::now().timestamp() + 86_400;
        db.merge_epg_remote_coverage(1, 2, Some(future), Some(400), Some(400))
            .unwrap();
        db.refresh_epg_section_coverage().unwrap();
        let find = |db: &Database| {
            db.get_epg_scan_states()
                .unwrap()
                .into_iter()
                .find(|state| state.network_id == 1 && state.tsid == 2)
                .unwrap()
        };
        let state = find(&db);
        assert_eq!(state.section_coverage_until, Some(future));
        assert_eq!(state.last_complete_at, Some(400));
        assert_eq!(state.last_eit_received_at, Some(400));

        // 以前ローカルで受信したことがある mux でも、古いローカル値で後退させない。
        db.upsert_epg_service_coverage(&[coverage(1, 2, 10, Some(100), true)])
            .unwrap();
        db.refresh_epg_section_coverage().unwrap();
        let state = find(&db);
        assert_eq!(state.section_coverage_until, Some(future));
        assert_eq!(state.last_complete_at, Some(400));
        assert_eq!(state.last_eit_received_at, Some(400));
    }

    #[test]
    fn refresh_drops_peer_coverage_once_it_is_in_the_past() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        db.merge_epg_remote_coverage(1, 2, Some(500), Some(400), Some(400))
            .unwrap();
        db.refresh_epg_section_coverage().unwrap();
        let state = db
            .get_epg_scan_states()
            .unwrap()
            .into_iter()
            .find(|state| state.network_id == 1 && state.tsid == 2)
            .unwrap();
        assert_eq!(state.section_coverage_until, None);
    }

    #[test]
    fn null_service_type_with_coverage_is_a_target() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, None);
        db.upsert_epg_service_coverage(&[coverage(1, 2, 10, Some(100), true)])
            .unwrap();
        let mux = &db.get_epg_mux_coverage().unwrap()[0];
        assert_eq!(mux.services_total, 1);
        assert_eq!(mux.coverage_until, Some(100));
    }

    #[test]
    fn non_program_service_types_are_excluded() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        channel(&db, driver, 1, 2, 11, Some(0xc0));
        db.upsert_epg_service_coverage(&[
            coverage(1, 2, 10, Some(100), true),
            coverage(1, 2, 11, Some(1), true),
        ])
        .unwrap();
        let mux = &db.get_epg_mux_coverage().unwrap()[0];
        assert_eq!(mux.services_total, 1);
        assert_eq!(mux.coverage_until, Some(100));
    }

    #[test]
    fn zero_sid_or_tsid_is_excluded_from_coverage_targets() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 0, None);
        channel(&db, driver, 1, 2, 10, None);
        channel(&db, driver, 1, 0, 11, None);
        db.upsert_epg_service_coverage(&[coverage(1, 2, 10, Some(100), true)])
            .unwrap();

        let muxes = db.get_epg_mux_coverage().unwrap();
        assert_eq!(muxes.len(), 1);
        assert_eq!((muxes[0].network_id, muxes[0].tsid), (1, 2));
        assert_eq!(muxes[0].services_total, 1);
        assert_eq!(muxes[0].coverage_until, Some(100));
    }

    #[test]
    fn services_not_in_channels_are_ignored() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        db.upsert_epg_service_coverage(&[
            coverage(1, 2, 10, Some(100), true),
            coverage(1, 2, 99, Some(1), true),
        ])
        .unwrap();
        let mux = &db.get_epg_mux_coverage().unwrap()[0];
        assert_eq!(mux.services_total, 1);
        assert_eq!(mux.coverage_until, Some(100));
    }

    #[test]
    fn upsert_is_idempotent_and_updates_last_complete_at() {
        let db = db();
        let mut row = coverage(1, 2, 10, Some(100), true);
        db.upsert_epg_service_coverage(&[row]).unwrap();
        row.updated_at = 30;
        db.upsert_epg_service_coverage(&[row]).unwrap();
        assert_eq!(db.get_epg_service_coverage(1, 2).unwrap().len(), 1);
        assert_eq!(
            db.get_epg_service_coverage(1, 2).unwrap()[0].last_complete_at,
            Some(30)
        );
        row.schedule_basic_complete = false;
        row.updated_at = 40;
        db.upsert_epg_service_coverage(&[row]).unwrap();
        assert_eq!(
            db.get_epg_service_coverage(1, 2).unwrap()[0].last_complete_at,
            Some(30)
        );
    }

    #[test]
    fn partial_rescan_does_not_regress_coverage_that_is_still_in_the_future() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        db.upsert_epg_service_coverage(&[coverage(1, 2, 10, Some(1000), true)])
            .unwrap();
        // 途中で打ち切ったスキャン: coverage が取れず、完成もしていない。
        let mut partial = coverage(1, 2, 10, None, false);
        partial.updated_at = 500;
        db.upsert_epg_service_coverage(&[partial]).unwrap();
        let mux = &db.get_epg_mux_coverage().unwrap()[0];
        assert_eq!(mux.coverage_until, Some(1000));
        assert_eq!(mux.last_complete_at, Some(20));
        assert_eq!(mux.services_complete, 1);

        // 前回値が過去になったら、もう守らない。
        partial.updated_at = 1500;
        db.upsert_epg_service_coverage(&[partial]).unwrap();
        assert_eq!(db.get_epg_mux_coverage().unwrap()[0].coverage_until, None);
    }

    #[test]
    fn refresh_writes_back_into_epg_scan_states() {
        let db = db();
        let driver = driver_id(&db);
        channel(&db, driver, 1, 2, 10, Some(1));
        db.upsert_epg_service_coverage(&[coverage(1, 2, 10, Some(100), true)])
            .unwrap();
        db.refresh_epg_section_coverage().unwrap();
        let values: (Option<i64>, i64, i64, Option<i64>) = db
            .connection()
            .query_row(
                "SELECT section_coverage_until, services_total, services_complete,
                        last_eit_received_at FROM epg_scan_states
                 WHERE network_id=1 AND tsid=2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(values, (Some(100), 1, 1, Some(10)));
    }

    #[test]
    fn set_epg_scan_status_roundtrips_every_variant() {
        let db = db();
        let variants = [
            EpgScanStatus::Complete,
            EpgScanStatus::Partial,
            EpgScanStatus::Failed,
            EpgScanStatus::Preempted,
            EpgScanStatus::NoData,
            EpgScanStatus::CpuAborted,
        ];
        for (index, status) in variants.into_iter().enumerate() {
            db.set_epg_scan_status(1, index as u16, status, index as i64)
                .unwrap();
            let stored: String = db
                .connection()
                .query_row(
                    "SELECT last_scan_status FROM epg_scan_states
                     WHERE network_id=1 AND tsid=?1",
                    [index as u16],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(EpgScanStatus::from_str_opt(&stored), Some(status));
        }
    }
}
