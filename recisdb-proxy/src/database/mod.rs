//! Database module for channel information storage.
//!
//! This module provides SQLite-based persistent storage for:
//! - BonDriver registration and scan configuration
//! - Channel information (NID/SID/TSID-based identification)
//! - Scan history and statistics

mod bon_driver;
mod channel;
mod driver_quality;
mod alert;
mod session_history;
mod models;
mod schema;

pub use models::*;

use rusqlite::{Connection, Result as SqliteResult};
use std::path::Path;
use thiserror::Error;

/// Database error types.
#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("BonDriver not found: {0}")]
    BonDriverNotFound(String),

    #[error("Channel not found: NID={nid}, SID={sid}, TSID={tsid}")]
    ChannelNotFound { nid: u16, sid: u16, tsid: u16 },

    #[error("Database path error: {0}")]
    PathError(String),

    #[error("Migration failed: {0}")]
    MigrationFailed(String),
}

pub type Result<T> = std::result::Result<T, DatabaseError>;

/// Main database connection wrapper.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open or create a database at the specified path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;

        // Enable foreign keys
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        let db = Self { conn };
        db.initialize_schema()?;

        Ok(db)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        let db = Self { conn };
        db.initialize_schema()?;

        Ok(db)
    }

    /// Initialize the database schema.
    fn initialize_schema(&self) -> Result<()> {
        self.conn.execute_batch(schema::SCHEMA_SQL)?;
        self.apply_migrations()?;
        Ok(())
    }

    /// Add a column to a table if it doesn't exist.
    fn add_column_if_not_exists(
        &self,
        table: &str,
        column: &str,
        column_type: &str,
    ) -> Result<()> {
        // Check if column exists using PRAGMA table_info
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let column_exists = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == column);

        if !column_exists {
            let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, column_type);
            self.conn.execute(&sql, [])?;
            log::info!("Migration: Added column {} to table {}", column, table);
        }

        Ok(())
    }

    /// Apply pending migrations.
    fn apply_migrations(&self) -> Result<()> {
        // Migration 001: Add band_type, region_id, and terrestrial_region columns if they don't exist
        // SQLite doesn't support IF NOT EXISTS for ALTER TABLE, so we check and add individually
        self.add_column_if_not_exists("channels", "band_type", "INTEGER")?;
        self.add_column_if_not_exists("channels", "region_id", "INTEGER")?;
        self.add_column_if_not_exists("channels", "terrestrial_region", "TEXT")?;

        // Migration 003: Add webhook columns to alert_rules if they don't exist
        self.add_column_if_not_exists("alert_rules", "webhook_url", "TEXT")?;
        self.add_column_if_not_exists("alert_rules", "webhook_format", "TEXT DEFAULT 'generic'")?;

        // Migration 004: Add global scan timing config columns if they don't exist
        self.add_column_if_not_exists("scan_scheduler_config", "signal_lock_wait_ms", "INTEGER DEFAULT 500")?;
        self.add_column_if_not_exists("scan_scheduler_config", "ts_read_timeout_ms", "INTEGER DEFAULT 300000")?;

        // Migration 005: Add tuner startup timing config columns if they don't exist
        self.add_column_if_not_exists("tuner_config", "set_channel_retry_interval_ms", "INTEGER DEFAULT 500")?;
        self.add_column_if_not_exists("tuner_config", "set_channel_retry_timeout_ms", "INTEGER DEFAULT 10000")?;
        self.add_column_if_not_exists("tuner_config", "signal_poll_interval_ms", "INTEGER DEFAULT 500")?;
        self.add_column_if_not_exists("tuner_config", "signal_wait_timeout_ms", "INTEGER DEFAULT 10000")?;

        // Migration 002: Fill band_type and terrestrial_region for existing channels
        // This updates all NULL values in these columns based on NID
        self.conn.execute_batch(
            r#"
            UPDATE channels
            SET band_type = CASE
                WHEN nid = 4 OR nid = 5 OR (nid >= 0x4001 AND nid <= 0x400F) THEN 1
                WHEN nid IN (6, 7, 10) OR (nid >= 0x6001 AND nid <= 0x600F) THEN 2
                WHEN nid >= 0x7C00 AND nid <= 0x7CFF THEN 3
                WHEN nid >= 0x7F00 AND nid <= 0x7FFF THEN 0
                ELSE 4
            END
            WHERE band_type IS NULL;

            UPDATE channels
            SET terrestrial_region = CASE
                WHEN nid IN (0x7F01, 0x7FE0, 0x7FF0) THEN '北海道'
                WHEN nid = 0x7F08 THEN '青森'
                WHEN nid = 0x7F09 THEN '岩手'
                WHEN nid = 0x7F0A THEN '宮城'
                WHEN nid = 0x7F0B THEN '秋田'
                WHEN nid = 0x7F0C THEN '山形'
                WHEN nid = 0x7F0D THEN '福島'
                WHEN nid = 0x7F0E THEN '茨城'
                WHEN nid = 0x7F0F THEN '栃木'
                WHEN nid = 0x7F10 THEN '群馬'
                WHEN nid = 0x7F11 THEN '埼玉'
                WHEN nid = 0x7F12 THEN '千葉'
                WHEN nid = 0x7F13 THEN '東京'
                WHEN nid = 0x7F14 THEN '神奈川'
                WHEN nid = 0x7F15 THEN '新潟'
                WHEN nid = 0x7F16 THEN '長野'
                WHEN nid = 0x7F17 THEN '山梨'
                WHEN nid = 0x7F18 THEN '富山'
                WHEN nid = 0x7F19 THEN '石川'
                WHEN nid = 0x7F1A THEN '福井'
                WHEN nid = 0x7F1B THEN '静岡'
                WHEN nid = 0x7F1C THEN '愛知'
                WHEN nid = 0x7F1D THEN '岐阜'
                WHEN nid = 0x7F1E THEN '三重'
                WHEN nid = 0x7F1F THEN '滋賀'
                WHEN nid = 0x7F20 THEN '京都'
                WHEN nid = 0x7F21 THEN '大阪'
                WHEN nid = 0x7F22 THEN '兵庫'
                WHEN nid = 0x7F23 THEN '奈良'
                WHEN nid = 0x7F24 THEN '和歌山'
                WHEN nid = 0x7F25 THEN '鳥取'
                WHEN nid = 0x7F26 THEN '島根'
                WHEN nid = 0x7F27 THEN '岡山'
                WHEN nid = 0x7F28 THEN '広島'
                WHEN nid = 0x7F29 THEN '山口'
                WHEN nid = 0x7F2A THEN '徳島'
                WHEN nid = 0x7F2B THEN '香川'
                WHEN nid = 0x7F2C THEN '愛媛'
                WHEN nid = 0x7F2D THEN '高知'
                WHEN nid = 0x7F2E THEN '福岡'
                WHEN nid = 0x7F2F THEN '佐賀'
                WHEN nid = 0x7F30 THEN '長崎'
                WHEN nid = 0x7F31 THEN '熊本'
                WHEN nid = 0x7F32 THEN '大分'
                WHEN nid = 0x7F33 THEN '宮崎'
                WHEN nid = 0x7F34 THEN '鹿児島'
                WHEN nid = 0x7F35 THEN '沖縄'
                WHEN nid >= 0x7FE0 AND nid <= 0x7FE7 THEN '北海道'
                WHEN nid = 0x7FE8 THEN '東京'
                WHEN nid = 0x7FE9 THEN '大阪'
                WHEN nid = 0x7FEA THEN '愛知'
                WHEN nid = 0x7FEB THEN '岡山'
                WHEN nid = 0x7FEC THEN '島根'
                WHEN nid >= 0x7FF0 AND nid <= 0x7FF7 THEN '北海道'
                ELSE '不明'
            END
            WHERE band_type = 0 AND terrestrial_region IS NULL;
            "#
        )?;

        Ok(())
    }

    /// Get the underlying connection (for advanced queries).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Begin a transaction.
    pub fn transaction(&mut self) -> SqliteResult<rusqlite::Transaction<'_>> {
        self.conn.transaction()
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish_non_exhaustive()
    }
}

/// Scan scheduler configuration storage.
impl Database {
    /// Get scan scheduler configuration from database.
    pub fn get_scan_scheduler_config(&self) -> Result<(u64, usize, u64, u64, u64)> {
        let mut stmt = self.conn.prepare(
            "SELECT check_interval_secs, max_concurrent_scans, scan_timeout_secs, signal_lock_wait_ms, ts_read_timeout_ms
             FROM scan_scheduler_config WHERE id = 1"
        )?;

        let result = stmt.query_row([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, usize>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        });

        match result {
            Ok((interval, concurrent, timeout, signal_lock_wait_ms, ts_read_timeout_ms)) => {
                Ok((interval, concurrent, timeout, signal_lock_wait_ms, ts_read_timeout_ms))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // Initialize with defaults if not exists
                self.conn.execute(
                    "INSERT OR IGNORE INTO scan_scheduler_config (id, check_interval_secs, max_concurrent_scans, scan_timeout_secs, signal_lock_wait_ms, ts_read_timeout_ms)
                     VALUES (1, 60, 1, 900, 500, 300000)",
                    [],
                )?;
                Ok((60, 1, 900, 500, 300000))
            }
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Update scan scheduler configuration.
    pub fn update_scan_scheduler_config(
        &self,
        check_interval: u64,
        max_concurrent: usize,
        timeout: u64,
        signal_lock_wait_ms: u64,
        ts_read_timeout_ms: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO scan_scheduler_config (id, check_interval_secs, max_concurrent_scans, scan_timeout_secs, signal_lock_wait_ms, ts_read_timeout_ms, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, strftime('%s', 'now'))",
            rusqlite::params![
                check_interval,
                max_concurrent as i32,
                timeout,
                signal_lock_wait_ms,
                ts_read_timeout_ms
            ],
        )?;
        Ok(())
    }
}

/// Tuner optimization configuration storage.
impl Database {
    /// Get tuner optimization configuration from database.
    pub fn get_tuner_config(&self) -> Result<(u64, bool, u64, u64, u64, u64, u64)> {
        let mut stmt = self.conn.prepare(
            "SELECT keep_alive_secs, prewarm_enabled, prewarm_timeout_secs,
                    set_channel_retry_interval_ms, set_channel_retry_timeout_ms,
                    signal_poll_interval_ms, signal_wait_timeout_ms
             FROM tuner_config WHERE id = 1"
        )?;

        let result = stmt.query_row([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
            ))
        });

        match result {
            Ok((
                keep_alive,
                prewarm_enabled,
                prewarm_timeout,
                set_channel_retry_interval_ms,
                set_channel_retry_timeout_ms,
                signal_poll_interval_ms,
                signal_wait_timeout_ms,
            )) => {
                Ok((
                    keep_alive,
                    prewarm_enabled,
                    prewarm_timeout,
                    set_channel_retry_interval_ms,
                    set_channel_retry_timeout_ms,
                    signal_poll_interval_ms,
                    signal_wait_timeout_ms,
                ))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO tuner_config
                     (id, keep_alive_secs, prewarm_enabled, prewarm_timeout_secs,
                      set_channel_retry_interval_ms, set_channel_retry_timeout_ms,
                      signal_poll_interval_ms, signal_wait_timeout_ms)
                     VALUES (1, 60, 1, 30, 500, 10000, 500, 10000)",
                    [],
                )?;
                Ok((60, true, 30, 500, 10000, 500, 10000))
            }
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Update tuner optimization configuration.
    pub fn update_tuner_config(
        &self,
        keep_alive_secs: u64,
        prewarm_enabled: bool,
        prewarm_timeout_secs: u64,
        set_channel_retry_interval_ms: u64,
        set_channel_retry_timeout_ms: u64,
        signal_poll_interval_ms: u64,
        signal_wait_timeout_ms: u64,
    ) -> Result<()> {
        let prewarm_enabled = if prewarm_enabled { 1 } else { 0 };
        self.conn.execute(
            "INSERT OR REPLACE INTO tuner_config
             (id, keep_alive_secs, prewarm_enabled, prewarm_timeout_secs,
              set_channel_retry_interval_ms, set_channel_retry_timeout_ms,
              signal_poll_interval_ms, signal_wait_timeout_ms, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s', 'now'))",
            rusqlite::params![
                keep_alive_secs,
                prewarm_enabled,
                prewarm_timeout_secs,
                set_channel_retry_interval_ms,
                set_channel_retry_timeout_ms,
                signal_poll_interval_ms,
                signal_wait_timeout_ms
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.connection().is_autocommit());
    }

    #[test]
    fn test_schema_creation() {
        let db = Database::open_in_memory().unwrap();

        // Verify tables exist
        let count: i32 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('bon_drivers', 'channels', 'scan_history', 'session_history', 'alert_rules', 'alert_history', 'driver_quality_stats', 'tuner_config')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 8);
    }
}
