//! Driver quality stats database operations.

use rusqlite::params;

use super::{BonDriverRecord, Database, DriverQualityStats, Result};

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

    /// Get driver quality score by DLL path.
    pub fn get_driver_quality_score_by_path(&self, dll_path: &str) -> Result<f64> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(dqs.quality_score, 1.0) FROM bon_drivers bd LEFT JOIN driver_quality_stats dqs ON bd.id = dqs.bon_driver_id WHERE bd.dll_path = ?1",
        )?;

        let result = stmt.query_row([dll_path], |row| row.get(0));

        match result {
            Ok(score) => Ok(score),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(1.0),
            Err(e) => Err(e.into()),
        }
    }

    /// Get BonDriver ranking by quality score.
    pub fn get_bondrivers_ranking(&self) -> Result<Vec<(BonDriverRecord, f64, f64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT bd.id, bd.dll_path, bd.driver_name, bd.version, bd.group_name, bd.auto_scan_enabled, bd.scan_interval_hours, bd.scan_priority, bd.last_scan, bd.next_scan_at, bd.passive_scan_enabled, bd.max_instances, bd.created_at, bd.updated_at, COALESCE(dqs.quality_score, 1.0) as quality_score, COALESCE(dqs.recent_drop_rate, 0.0) as recent_drop_rate, COALESCE(dqs.total_sessions, 0) as total_sessions FROM bon_drivers bd LEFT JOIN driver_quality_stats dqs ON bd.id = dqs.bon_driver_id ORDER BY quality_score DESC, total_sessions DESC, bd.dll_path ASC",
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
