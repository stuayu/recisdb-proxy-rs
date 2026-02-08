//! Session history database operations.

use rusqlite::params;

use super::{Database, Result, SessionHistoryRecord};

impl Database {
    /// Insert session start record.
    pub fn insert_session_start(
        &self,
        session_id: u64,
        client_address: &str,
        tuner_path: Option<&str>,
        channel_info: Option<&str>,
        channel_name: Option<&str>,
        started_at: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO session_history (session_id, client_address, tuner_path, channel_info, channel_name, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id as i64, client_address, tuner_path, channel_info, channel_name, started_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update session end stats.
    #[allow(clippy::too_many_arguments)]
    pub fn update_session_end(
        &self,
        id: i64,
        ended_at: i64,
        duration_secs: i64,
        packets_sent: u64,
        packets_dropped: u64,
        packets_scrambled: u64,
        packets_error: u64,
        bytes_sent: u64,
        average_bitrate_mbps: Option<f64>,
        average_signal_level: Option<f64>,
        disconnect_reason: Option<&str>,
        tuner_path: Option<&str>,
        channel_info: Option<&str>,
        channel_name: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE session_history SET ended_at = ?2, duration_secs = ?3, packets_sent = ?4, packets_dropped = ?5, packets_scrambled = ?6, packets_error = ?7, bytes_sent = ?8, average_bitrate_mbps = ?9, average_signal_level = ?10, disconnect_reason = ?11, tuner_path = ?12, channel_info = ?13, channel_name = ?14 WHERE id = ?1",
            params![
                id,
                ended_at,
                duration_secs,
                packets_sent as i64,
                packets_dropped as i64,
                packets_scrambled as i64,
                packets_error as i64,
                bytes_sent as i64,
                average_bitrate_mbps,
                average_signal_level,
                disconnect_reason,
                tuner_path,
                channel_info,
                channel_name,
            ],
        )?;
        Ok(())
    }

    /// Update session progress (periodic update during streaming, does NOT set ended_at).
    #[allow(clippy::too_many_arguments)]
    pub fn update_session_progress(
        &self,
        id: i64,
        duration_secs: i64,
        packets_sent: u64,
        packets_dropped: u64,
        packets_scrambled: u64,
        packets_error: u64,
        bytes_sent: u64,
        average_bitrate_mbps: Option<f64>,
        average_signal_level: Option<f64>,
        tuner_path: Option<&str>,
        channel_info: Option<&str>,
        channel_name: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE session_history SET duration_secs = ?2, packets_sent = ?3, packets_dropped = ?4, packets_scrambled = ?5, packets_error = ?6, bytes_sent = ?7, average_bitrate_mbps = ?8, average_signal_level = ?9, tuner_path = ?10, channel_info = ?11, channel_name = ?12 WHERE id = ?1",
            params![
                id,
                duration_secs,
                packets_sent as i64,
                packets_dropped as i64,
                packets_scrambled as i64,
                packets_error as i64,
                bytes_sent as i64,
                average_bitrate_mbps,
                average_signal_level,
                tuner_path,
                channel_info,
                channel_name,
            ],
        )?;
        Ok(())
    }

    /// Get total session count from database.
    pub fn get_total_session_count(&self) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_history",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Get session history with pagination and optional client address filter.
    pub fn get_session_history(
        &self,
        page: u32,
        per_page: u32,
        client_address: Option<&str>,
    ) -> Result<(Vec<SessionHistoryRecord>, u32)> {
        let offset = (page.saturating_sub(1) * per_page) as i64;
        let limit = per_page as i64;

        let (count_sql, list_sql, params_vec): (String, String, Vec<rusqlite::types::Value>) =
            if let Some(addr) = client_address {
                let like = format!("%{}%", addr);
                (
                    "SELECT COUNT(*) FROM session_history WHERE client_address LIKE ?1".to_string(),
                    "SELECT id, session_id, client_address, tuner_path, channel_info, channel_name, started_at, ended_at, duration_secs, packets_sent, packets_dropped, packets_scrambled, packets_error, bytes_sent, average_bitrate_mbps, average_signal_level, disconnect_reason, created_at FROM session_history WHERE client_address LIKE ?1 ORDER BY started_at DESC LIMIT ?2 OFFSET ?3".to_string(),
                    vec![like.into(), limit.into(), offset.into()],
                )
            } else {
                (
                    "SELECT COUNT(*) FROM session_history".to_string(),
                    "SELECT id, session_id, client_address, tuner_path, channel_info, channel_name, started_at, ended_at, duration_secs, packets_sent, packets_dropped, packets_scrambled, packets_error, bytes_sent, average_bitrate_mbps, average_signal_level, disconnect_reason, created_at FROM session_history ORDER BY started_at DESC LIMIT ?1 OFFSET ?2".to_string(),
                    vec![limit.into(), offset.into()],
                )
            };

        let total: u32 = if let Some(addr) = client_address {
            self.conn
                .query_row(&count_sql, params![format!("%{}%", addr)], |row| row.get(0))?
        } else {
            self.conn.query_row(&count_sql, [], |row| row.get(0))?
        };

        let mut stmt = self.conn.prepare(&list_sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec), |row| {
                Ok(SessionHistoryRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    client_address: row.get(2)?,
                    tuner_path: row.get(3)?,
                    channel_info: row.get(4)?,
                    channel_name: row.get(5)?,
                    started_at: row.get(6)?,
                    ended_at: row.get(7)?,
                    duration_secs: row.get(8)?,
                    packets_sent: row.get(9)?,
                    packets_dropped: row.get(10)?,
                    packets_scrambled: row.get(11)?,
                    packets_error: row.get(12)?,
                    bytes_sent: row.get(13)?,
                    average_bitrate_mbps: row.get(14)?,
                    average_signal_level: row.get(15)?,
                    disconnect_reason: row.get(16)?,
                    created_at: row.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok((rows, total))
    }
}
