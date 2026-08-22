//! Driver quality stats database operations.

use rusqlite::{params, OptionalExtension};

use super::{BonDriverRecord, Database, DriverQualityStats, DriverRuntimeSample, Result};

impl Database {
    /// Get driver quality stats by BonDriver ID.
    pub fn get_driver_quality_stats(&self, bon_driver_id: i64) -> Result<Option<DriverQualityStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, bon_driver_id, total_packets, dropped_packets, scrambled_packets, error_packets, total_sessions, quality_score, recent_drop_rate, recent_error_rate, last_updated FROM driver_quality_stats WHERE bon_driver_id = ?1",
        )?;

        let result = stmt.query_row([bon_driver_id], |row| {
            Ok(DriverQualityStats {
                id: row.get(0)?,
                bon_driver_id: row.get(1)?,
                total_packets: row.get(2)?,
                dropped_packets: row.get(3)?,
                scrambled_packets: row.get(4)?,
                error_packets: row.get(5)?,
                total_sessions: row.get(6)?,
                quality_score: row.get(7)?,
                recent_drop_rate: row.get(8)?,
                recent_error_rate: row.get(9)?,
                last_updated: row.get(10)?,
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Upsert driver quality stats.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_driver_quality_stats(
        &self,
        bon_driver_id: i64,
        total_packets: i64,
        dropped_packets: i64,
        scrambled_packets: i64,
        error_packets: i64,
        total_sessions: i64,
        quality_score: f64,
        recent_drop_rate: f64,
        recent_error_rate: f64,
        last_updated: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO driver_quality_stats (bon_driver_id, total_packets, dropped_packets, scrambled_packets, error_packets, total_sessions, quality_score, recent_drop_rate, recent_error_rate, last_updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(bon_driver_id) DO UPDATE SET total_packets = excluded.total_packets, dropped_packets = excluded.dropped_packets, scrambled_packets = excluded.scrambled_packets, error_packets = excluded.error_packets, total_sessions = excluded.total_sessions, quality_score = excluded.quality_score, recent_drop_rate = excluded.recent_drop_rate, recent_error_rate = excluded.recent_error_rate, last_updated = excluded.last_updated",
            params![
                bon_driver_id,
                total_packets,
                dropped_packets,
                scrambled_packets,
                error_packets,
                total_sessions,
                quality_score,
                recent_drop_rate,
                recent_error_rate,
                last_updated,
            ],
        )?;
        Ok(())
    }

    /// Add a runtime observation and update latency EWMAs. The score is a
    /// deliberately conservative health multiplier: a flaky/very slow driver
    /// must not outrank a stable one merely because its TS packets are clean
    /// once it finally starts.
    pub fn record_driver_runtime_sample(&self, bon_driver_id: i64, sample: DriverRuntimeSample) -> Result<()> {
        #[allow(clippy::type_complexity)]
        let existing: Option<(i64, Option<f64>, Option<f64>, Option<f64>, i64, i64, i64, i64, i64)> = self.conn
            .query_row(
                "SELECT samples, open_latency_ewma_ms, tune_latency_ewma_ms, first_ts_latency_ewma_ms,
                        stall_count, open_failures, tune_failures, first_ts_timeouts, worker_restarts
                 FROM driver_runtime_health WHERE bon_driver_id = ?1",
                [bon_driver_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)),
            )
            .optional()?;

        let (samples, open_old, tune_old, first_old, stalls, open_failures, tune_failures, first_timeouts, worker_restarts) =
            existing.unwrap_or((0, None, None, None, 0, 0, 0, 0, 0));
        let ewma = |old: Option<f64>, new: Option<u64>| -> Option<f64> {
            match (old, new) {
                (Some(old), Some(new)) => Some(old * 0.8 + new as f64 * 0.2),
                (None, Some(new)) => Some(new as f64),
                (old, None) => old,
            }
        };
        let open = ewma(open_old, sample.open_ms);
        let tune = ewma(tune_old, sample.tune_ms);
        let first = ewma(first_old, sample.first_ts_ms);
        let stalls = stalls + sample.stalled as i64;
        let open_failures = open_failures + sample.open_failed as i64;
        let tune_failures = tune_failures + sample.tune_failed as i64;
        let first_timeouts = first_timeouts + sample.first_ts_timeout as i64;
        let worker_restarts = worker_restarts + sample.worker_restart as i64;
        let samples = samples + 1;

        let latency_score = |value: Option<f64>, soft_ms: f64, hard_ms: f64| -> f64 {
            let Some(value) = value else { return 1.0; };
            if value <= soft_ms { 1.0 }
            else if value >= hard_ms { 0.20 }
            else { 1.0 - ((value - soft_ms) / (hard_ms - soft_ms)) * 0.80 }
        };
        let mut runtime_score = latency_score(open, 500.0, 5_000.0)
            * latency_score(tune, 1_500.0, 10_000.0)
            * latency_score(first, 2_000.0, 15_000.0);
        let denom = samples.max(1) as f64;
        let failure_rate = (open_failures + tune_failures + first_timeouts) as f64 / denom;
        let stall_rate = stalls as f64 / denom;
        runtime_score *= (1.0 - failure_rate.min(0.8)).max(0.2);
        runtime_score *= (1.0 - stall_rate.min(0.7)).max(0.3);
        if worker_restarts > 0 {
            runtime_score *= 0.85_f64.powi(worker_restarts.min(5) as i32);
        }
        runtime_score = runtime_score.clamp(0.05, 1.0);

        self.conn.execute(
            "INSERT INTO driver_runtime_health
             (bon_driver_id, samples, open_latency_ewma_ms, tune_latency_ewma_ms, first_ts_latency_ewma_ms,
              stall_count, open_failures, tune_failures, first_ts_timeouts, worker_restarts, runtime_score, last_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, strftime('%s','now'))
             ON CONFLICT(bon_driver_id) DO UPDATE SET
              samples=excluded.samples,
              open_latency_ewma_ms=excluded.open_latency_ewma_ms,
              tune_latency_ewma_ms=excluded.tune_latency_ewma_ms,
              first_ts_latency_ewma_ms=excluded.first_ts_latency_ewma_ms,
              stall_count=excluded.stall_count,
              open_failures=excluded.open_failures,
              tune_failures=excluded.tune_failures,
              first_ts_timeouts=excluded.first_ts_timeouts,
              worker_restarts=excluded.worker_restarts,
              runtime_score=excluded.runtime_score,
              last_updated=excluded.last_updated",
            params![bon_driver_id, samples, open, tune, first, stalls, open_failures, tune_failures, first_timeouts, worker_restarts, runtime_score],
        )?;
        Ok(())
    }

    pub fn get_driver_runtime_health_score_by_path(&self, dll_path: &str) -> Result<f64> {
        let score = self.conn.query_row(
            "SELECT COALESCE(drh.runtime_score, 1.0)
             FROM bon_drivers bd
             LEFT JOIN driver_runtime_health drh ON bd.id = drh.bon_driver_id
             WHERE bd.dll_path = ?1",
            [dll_path],
            |row| row.get(0),
        );
        match score {
            Ok(score) => Ok(score),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(1.0),
            Err(e) => Err(e.into()),
        }
    }

    /// Get combined driver quality score by DLL path. Packet integrity and
    /// runtime health are both required: multiplication strongly demotes a
    /// path that is clean-but-slow or fast-but-corrupt without inventing a
    /// second selection policy outside `tuner::policy`.
    pub fn get_driver_quality_score_by_path(&self, dll_path: &str) -> Result<f64> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(dqs.quality_score, 1.0), COALESCE(drh.runtime_score, 1.0)
             FROM bon_drivers bd
             LEFT JOIN driver_quality_stats dqs ON bd.id = dqs.bon_driver_id
             LEFT JOIN driver_runtime_health drh ON bd.id = drh.bon_driver_id
             WHERE bd.dll_path = ?1",
        )?;

        let result = stmt.query_row([dll_path], |row| {
            let packet: f64 = row.get(0)?;
            let runtime: f64 = row.get(1)?;
            Ok((packet * runtime).clamp(0.0, 1.0))
        });

        match result {
            Ok(score) => Ok(score),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(1.0),
            Err(e) => Err(e.into()),
        }
    }

    /// Get BonDriver ranking by combined quality score.
    pub fn get_bondrivers_ranking(&self) -> Result<Vec<(BonDriverRecord, f64, f64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT bd.id, bd.dll_path, bd.driver_name, bd.version, bd.group_name, bd.auto_scan_enabled, bd.scan_interval_hours, bd.scan_priority, bd.last_scan, bd.next_scan_at, bd.passive_scan_enabled, bd.max_instances, bd.created_at, bd.updated_at,
                    (COALESCE(dqs.quality_score, 1.0) * COALESCE(drh.runtime_score, 1.0)) as quality_score,
                    COALESCE(dqs.recent_drop_rate, 0.0) as recent_drop_rate,
                    COALESCE(dqs.total_sessions, 0) as total_sessions
             FROM bon_drivers bd
             LEFT JOIN driver_quality_stats dqs ON bd.id = dqs.bon_driver_id
             LEFT JOIN driver_runtime_health drh ON bd.id = drh.bon_driver_id
             ORDER BY quality_score DESC, total_sessions DESC, bd.dll_path ASC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    BonDriverRecord {
                        id: row.get(0)?,
                        dll_path: row.get(1)?,
                        driver_name: row.get(2)?,
                        version: row.get(3)?,
                        group_name: row.get(4)?,
                        auto_scan_enabled: row.get::<_, i32>(5)? != 0,
                        scan_interval_hours: row.get(6)?,
                        scan_priority: row.get(7)?,
                        last_scan: row.get(8)?,
                        next_scan_at: row.get(9)?,
                        passive_scan_enabled: row.get::<_, i32>(10)? != 0,
                        max_instances: row.get(11)?,
                        created_at: row.get(12)?,
                        updated_at: row.get(13)?,
                    },
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::database::NewBonDriver;

    #[test]
    fn slow_but_successful_driver_is_demoted() {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert_bon_driver(&NewBonDriver::new("slow.dll")).unwrap();
        for _ in 0..5 {
            db.record_driver_runtime_sample(id, DriverRuntimeSample {
                open_ms: Some(2_000),
                tune_ms: Some(8_000),
                first_ts_ms: Some(7_000),
                ..Default::default()
            }).unwrap();
        }
        let score = db.get_driver_quality_score_by_path("slow.dll").unwrap();
        assert!(score < 0.6, "slow-but-successful driver must not remain perfect: {score}");
    }

    #[test]
    fn a_fast_driver_keeps_a_perfect_score_and_outranks_a_slow_one() {
        let db = Database::open_in_memory().unwrap();
        let fast = db.insert_bon_driver(&NewBonDriver::new("fast.dll")).unwrap();
        let slow = db.insert_bon_driver(&NewBonDriver::new("slow.dll")).unwrap();
        for _ in 0..5 {
            db.record_driver_runtime_sample(fast, DriverRuntimeSample {
                open_ms: Some(120),
                tune_ms: Some(900),
                first_ts_ms: Some(1_200),
                ..Default::default()
            }).unwrap();
            db.record_driver_runtime_sample(slow, DriverRuntimeSample {
                open_ms: Some(2_000),
                tune_ms: Some(8_000),
                first_ts_ms: Some(7_000),
                ..Default::default()
            }).unwrap();
        }
        let fast_score = db.get_driver_quality_score_by_path("fast.dll").unwrap();
        let slow_score = db.get_driver_quality_score_by_path("slow.dll").unwrap();
        assert_eq!(fast_score, 1.0, "a comfortably fast driver must not be penalised");
        assert!(
            fast_score > slow_score,
            "fast {fast_score} must outrank slow {slow_score}"
        );
    }

    /// "Says yes, then delivers nothing" is invisible to packet statistics —
    /// there are no packets to be wrong about — so it has to show up here.
    #[test]
    fn a_driver_that_never_produces_ts_is_demoted_hard() {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert_bon_driver(&NewBonDriver::new("silent.dll")).unwrap();
        for _ in 0..3 {
            db.record_driver_runtime_sample(id, DriverRuntimeSample {
                tune_ms: Some(800),
                first_ts_ms: None,
                first_ts_timeout: true,
                ..Default::default()
            }).unwrap();
        }
        let score = db.get_driver_quality_score_by_path("silent.dll").unwrap();
        assert!(score < 0.5, "a driver that never delivers TS must be demoted: {score}");
    }

    #[test]
    fn stalls_demote_a_driver_that_is_otherwise_fast() {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert_bon_driver(&NewBonDriver::new("hiccup.dll")).unwrap();
        for _ in 0..4 {
            db.record_driver_runtime_sample(id, DriverRuntimeSample {
                open_ms: Some(100),
                tune_ms: Some(500),
                first_ts_ms: Some(800),
                stalled: true,
                ..Default::default()
            }).unwrap();
        }
        let score = db.get_driver_quality_score_by_path("hiccup.dll").unwrap();
        assert!(score < 1.0, "a stalling driver must not score perfectly: {score}");
    }

    #[test]
    fn open_failures_demote_a_driver_even_after_its_cooldown_expires() {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert_bon_driver(&NewBonDriver::new("flaky.dll")).unwrap();
        for _ in 0..4 {
            db.record_driver_runtime_sample(id, DriverRuntimeSample {
                tune_ms: Some(300),
                open_failed: true,
                ..Default::default()
            }).unwrap();
        }
        let score = db.get_driver_quality_score_by_path("flaky.dll").unwrap();
        assert!(score < 0.7, "repeated open failures must persist as low health: {score}");
    }

    /// An unknown driver must not be treated as bad: selection would then
    /// avoid every newly added tuner.
    #[test]
    fn a_driver_with_no_samples_scores_neutral() {
        let db = Database::open_in_memory().unwrap();
        db.insert_bon_driver(&NewBonDriver::new("new.dll")).unwrap();
        assert_eq!(db.get_driver_quality_score_by_path("new.dll").unwrap(), 1.0);
        assert_eq!(db.get_driver_quality_score_by_path("absent.dll").unwrap(), 1.0);
    }
}
