//! Driver quality scoring and selection.

use crate::database::{BonDriverRecord, Database, DriverQualityStats, Result};

/// BonDriver with quality score info.
#[derive(Debug, Clone)]
pub struct BonDriverWithScore {
    pub driver: BonDriverRecord,
    pub quality_score: f64,
    pub recent_drop_rate: f64,
}

/// Driver quality scorer.
pub struct QualityScorer;

impl QualityScorer {
    /// Update driver quality stats after a session ends.
    pub fn update_stats(
        db: &Database,
        bon_driver_id: i64,
        packets: u64,
        dropped: u64,
        scrambled: u64,
        errors: u64,
    ) -> Result<()> {
        let current = db.get_driver_quality_stats(bon_driver_id)?;

        let total_packets = current.as_ref().map(|s| s.total_packets).unwrap_or(0) + packets as i64;
        let dropped_packets = current.as_ref().map(|s| s.dropped_packets).unwrap_or(0) + dropped as i64;
        let scrambled_packets = current.as_ref().map(|s| s.scrambled_packets).unwrap_or(0) + scrambled as i64;
        let error_packets = current.as_ref().map(|s| s.error_packets).unwrap_or(0) + errors as i64;
        let total_sessions = current.as_ref().map(|s| s.total_sessions).unwrap_or(0) + 1;

        let stats = DriverQualityStats {
            id: current.as_ref().map(|s| s.id).unwrap_or(0),
            bon_driver_id,
            total_packets,
            dropped_packets,
            scrambled_packets,
            error_packets,
            total_sessions,
            quality_score: 1.0,
            recent_drop_rate: 0.0,
            recent_error_rate: 0.0,
            last_updated: chrono::Utc::now().timestamp(),
        };

        let quality_score = Self::calculate_score(&stats);

        let session_total = packets.max(1) as f64;
        let recent_drop_rate = dropped as f64 / session_total;
        let recent_error_rate = errors as f64 / session_total;

        db.upsert_driver_quality_stats(
            bon_driver_id,
            total_packets,
            dropped_packets,
            scrambled_packets,
            error_packets,
            total_sessions,
            quality_score,
            recent_drop_rate,
            recent_error_rate,
            chrono::Utc::now().timestamp(),
        )?;

        Ok(())
    }

    /// Calculate quality score (0.0 - 1.0).
    /// score = 1.0 - (drop_rate * 0.5 + error_rate * 0.3 + scramble_rate * 0.2)
    pub fn calculate_score(stats: &DriverQualityStats) -> f64 {
        let total = stats.total_packets.max(1) as f64;
        let drop_rate = stats.dropped_packets as f64 / total;
        let error_rate = stats.error_packets as f64 / total;
        let scramble_rate = stats.scrambled_packets as f64 / total;

        let score = 1.0 - (drop_rate * 0.5 + error_rate * 0.3 + scramble_rate * 0.2);
        score.clamp(0.0, 1.0)
    }

    /// Get drivers for a channel ordered by quality score.
    pub async fn get_best_drivers_for_channel(
        db: &Database,
        nid: u16,
        tsid: u16,
    ) -> Result<Vec<BonDriverWithScore>> {
        let mut stmt = db.connection().prepare(
            "SELECT bd.id, bd.dll_path, bd.driver_name, bd.version, bd.group_name, bd.auto_scan_enabled, bd.scan_interval_hours, bd.scan_priority, bd.last_scan, bd.next_scan_at, bd.passive_scan_enabled, bd.max_instances, bd.created_at, bd.updated_at, COALESCE(dqs.quality_score, 1.0) as quality_score, COALESCE(dqs.recent_drop_rate, 0.0) as recent_drop_rate FROM channels ch JOIN bon_drivers bd ON ch.bon_driver_id = bd.id LEFT JOIN driver_quality_stats dqs ON bd.id = dqs.bon_driver_id WHERE ch.nid = ?1 AND ch.tsid = ?2 AND ch.is_enabled = 1 GROUP BY bd.id ORDER BY quality_score DESC, bd.scan_priority DESC",
        )?;

        let drivers = stmt
            .query_map([nid as i64, tsid as i64], |row| {
                Ok(BonDriverWithScore {
                    driver: BonDriverRecord {
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
                    quality_score: row.get(14)?,
                    recent_drop_rate: row.get(15)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(drivers)
    }
}
