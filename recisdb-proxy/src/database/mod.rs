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
mod encode_profile;
mod session_history;
mod program;
mod models;
mod schema;

pub use models::*;

use rusqlite::{Connection, Result as SqliteResult};
use std::path::Path;
use thiserror::Error;

const DEFAULT_TSREPLACE_COMMAND_PATH: &str = "tsreplace";
const DEFAULT_TSREPLACE_ARGUMENTS: &str = "-i - -o - --preserve-other-services -e QSVEncC64.exe -i - --input-format mpegts --tff --vpp-deinterlace normal -c hevc --icq 19 --gop-len 90 --output-format mpegts -o -";
const DEFAULT_TSREPLACE_READ_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_TSREPLACE_PASSTHROUGH_ON_ERROR: bool = true;
const DEFAULT_TSREPLACE_MAX_CONCURRENT_ENCODERS: i64 = 2;

/// Recommended tsreadex (stage-1) arguments for the browser-preview pipeline,
/// seeded into `preview_encoder_config.preprocessor_arguments` so the
/// dashboard starts from a working template (KonomiTV-equivalent settings):
/// `-x 18/38/39` drops EIT, `-n {SID}` selects the target service (the
/// placeholder is substituted per stream, see `encoder_pool::SID_PLACEHOLDER`),
/// `-a 13 -b 5 -c 1 -u 1` keep audio/caption/superimpose streams always
/// present, `-d 13` converts captions to ID3 timed-metadata for aribb24.js,
/// and the trailing `-` reads from stdin.
pub(crate) const DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS: &str =
    "-x 18/38/39 -n {SID} -a 13 -b 5 -c 1 -u 1 -d 13 -";

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

/// Migration ledger entry body: a fn pointer taking the open `Database`.
type MigrationFn = fn(&Database) -> Result<()>;

/// Main database connection wrapper.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open or create a database at the specified path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;

        // Enable foreign keys, and configure WAL journaling for concurrent
        // reader/writer access (see docs/SYSTEM_REVIEW_2026-07.md Phase 0):
        // - journal_mode=WAL allows readers to proceed while a writer is active.
        // - busy_timeout=3000 waits up to 3s for a lock instead of failing fast.
        // - synchronous=NORMAL is the recommended durability level under WAL.
        // WAL is a no-op for `:memory:` databases (see open_in_memory).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA busy_timeout = 3000;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA foreign_keys = ON;",
        )?;

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
        // STREAMING_DESIGN.md §5.3/§9 P5: seed the default 'preview' encode
        // profile. Idempotent (checks for an existing row by name), so it is
        // safe to call unconditionally on every open().
        self.seed_default_encode_profiles()?;
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

    /// Ordered migration ledger, tracked via `PRAGMA user_version` (the
    /// index of the next migration to apply). This list preserves the
    /// EFFECTIVE order the old ad-hoc, comment-numbered steps actually ran
    /// in (001, 003, 004, 005, 006, 007, 011, 012, 013, 008, 009, 010, 002)
    /// — entries are relabeled 001..013 in that same order, not reordered.
    ///
    /// CRITICAL: every body here MUST stay idempotent
    /// (`add_column_if_not_exists`, `CREATE TABLE IF NOT EXISTS`, `INSERT OR
    /// IGNORE`, etc). Databases created before this ledger existed have
    /// `user_version = 0` but may already have every column, because they
    /// were migrated by the old ad-hoc code path. On such a DB every body in
    /// this list replays from index 0 and must be a harmless no-op wherever
    /// the work was already done.
    const MIGRATIONS: &'static [(&'static str, MigrationFn)] = &[
        ("001_channels_band_region_columns", Database::migration_001_channels_band_region_columns),
        ("002_alert_rules_webhook_columns", Database::migration_002_alert_rules_webhook_columns),
        ("003_scan_scheduler_timing_columns", Database::migration_003_scan_scheduler_timing_columns),
        ("004_tuner_startup_timing_columns", Database::migration_004_tuner_startup_timing_columns),
        ("005_session_history_loss_summary", Database::migration_005_session_history_loss_summary),
        ("006_tsreplace_max_concurrent_encoders", Database::migration_006_tsreplace_max_concurrent_encoders),
        ("007_tsreplace_preprocessor_columns", Database::migration_007_tsreplace_preprocessor_columns),
        ("008_tsreplace_preview_enabled_legacy", Database::migration_008_tsreplace_preview_enabled_legacy),
        ("009_preview_encoder_config_seed", Database::migration_009_preview_encoder_config_seed),
        ("010_session_history_stream_class", Database::migration_010_session_history_stream_class),
        ("011_tuner_prefill_jitter_columns", Database::migration_011_tuner_prefill_jitter_columns),
        ("012_encode_profiles_noop", Database::migration_012_encode_profiles_noop),
        (
            "013_backfill_channels_band_terrestrial_region",
            Database::migration_013_backfill_channels_band_terrestrial_region,
        ),
        (
            "014_periodic_auto_scan_opt_in",
            Database::migration_014_periodic_auto_scan_opt_in,
        ),
        ("015_programs_table", Database::migration_015_programs_table),
        (
            "016_reclassify_band_region_from_nid",
            Database::migration_016_reclassify_band_region_from_nid,
        ),
    ];

    // Migration 016: re-derive band_type / region_id / terrestrial_region
    // from NID using the protocol crate as the single source of truth.
    // Migration 013 did this in raw SQL with a terrestrial range of
    // 0x7F00-0x7FFF, but real terrestrial NIDs span 0x7880-0x7FE8, so rows
    // in 0x7880-0x7EFF were misclassified as band_type=4 (Other) and got no
    // region. Rules (idempotent by construction):
    //   - band_type: fill when NULL; also correct 4 (Other) when the NID
    //     derivation disagrees (undoing 013's damage). Other non-NULL
    //     values are left alone.
    //   - region_id / terrestrial_region: fill only when NULL.
    fn migration_016_reclassify_band_region_from_nid(&self) -> Result<()> {
        use recisdb_protocol::broadcast_region::{get_prefecture_name, get_region_id_from_nid};
        use recisdb_protocol::BandType;

        let rows: Vec<(i64, i64, Option<i64>)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, nid, band_type FROM channels")?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            mapped.collect::<std::result::Result<Vec<_>, _>>()?
        };

        for (id, nid, band_type) in rows {
            let nid = nid as u16;
            let derived = BandType::from_nid(nid) as i64;
            let new_band = match band_type {
                None => Some(derived),
                Some(4) if derived != 4 => Some(derived),
                _ => None,
            };
            self.conn.execute(
                "UPDATE channels SET
                    band_type = COALESCE(?2, band_type),
                    region_id = COALESCE(region_id, ?3),
                    terrestrial_region = COALESCE(terrestrial_region, ?4)
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    new_band,
                    get_region_id_from_nid(nid).map(|v| v as i64),
                    get_prefecture_name(nid),
                ],
            )?;
        }
        Ok(())
    }

    // Migration 015: EPG (program guide) storage, collected from live EIT
    // sections (`tuner/epg_collector.rs`, `crate::epg_writer`). Brand-new
    // table, so `CREATE TABLE IF NOT EXISTS` alone is idempotent for both
    // fresh and pre-existing databases — no `add_column_if_not_exists` step
    // is needed (same reasoning as Migration 012's `encode_profiles`).
    fn migration_015_programs_table(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS programs (
                id INTEGER PRIMARY KEY,
                nid INTEGER NOT NULL,
                sid INTEGER NOT NULL,
                tsid INTEGER NOT NULL,
                event_id INTEGER NOT NULL,
                start_at INTEGER NOT NULL,
                duration_secs INTEGER NOT NULL,
                name TEXT,
                description TEXT,
                extended TEXT,
                genre INTEGER,
                updated_at INTEGER NOT NULL,
                UNIQUE(nid, sid, event_id)
            );
            CREATE INDEX IF NOT EXISTS idx_programs_sid_start_at ON programs(sid, start_at);
            CREATE INDEX IF NOT EXISTS idx_programs_start_at ON programs(start_at);
            "#,
        )?;
        Ok(())
    }

    // Migration 014: periodic auto rescan becomes opt-in (schema default
    // flipped from 1 to 0). Existing rows almost certainly have
    // auto_scan_enabled = 1 only because the old default was ON, so turn it
    // off everywhere. Clearing next_scan_at for already-scanned drivers stops
    // the pending +24h reschedule; drivers that never completed a scan keep
    // their next_scan_at (an in-flight initial one-shot must still run).
    fn migration_014_periodic_auto_scan_opt_in(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            UPDATE bon_drivers SET auto_scan_enabled = 0;
            UPDATE bon_drivers SET next_scan_at = NULL WHERE last_scan IS NOT NULL;
            "#,
        )?;
        Ok(())
    }

    /// Apply pending migrations, tracked via `PRAGMA user_version` as the
    /// ledger position. See [`Self::MIGRATIONS`] for ordering/idempotency
    /// requirements.
    fn apply_migrations(&self) -> Result<()> {
        let applied: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let applied = applied.max(0) as usize;

        for (name, body) in Self::MIGRATIONS.iter().skip(applied) {
            body(self)?;
            log::debug!("Migration applied: {}", name);
        }

        let target = Self::MIGRATIONS.len();
        if applied < target {
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {}", target))?;
        }

        Ok(())
    }

    // Migration 001: Add band_type, region_id, and terrestrial_region columns
    // to channels if they don't exist. SQLite doesn't support IF NOT EXISTS
    // for ALTER TABLE, so we check and add individually.
    fn migration_001_channels_band_region_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("channels", "band_type", "INTEGER")?;
        self.add_column_if_not_exists("channels", "region_id", "INTEGER")?;
        self.add_column_if_not_exists("channels", "terrestrial_region", "TEXT")?;
        Ok(())
    }

    // Migration 002 (was 003): Add webhook columns to alert_rules if they don't exist.
    fn migration_002_alert_rules_webhook_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("alert_rules", "webhook_url", "TEXT")?;
        self.add_column_if_not_exists("alert_rules", "webhook_format", "TEXT DEFAULT 'generic'")?;
        Ok(())
    }

    // Migration 003 (was 004): Add global scan timing config columns if they don't exist.
    fn migration_003_scan_scheduler_timing_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("scan_scheduler_config", "signal_lock_wait_ms", "INTEGER DEFAULT 500")?;
        self.add_column_if_not_exists("scan_scheduler_config", "ts_read_timeout_ms", "INTEGER DEFAULT 300000")?;
        Ok(())
    }

    // Migration 004 (was 005): Add tuner startup timing config columns if they don't exist.
    fn migration_004_tuner_startup_timing_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("tuner_config", "set_channel_retry_interval_ms", "INTEGER DEFAULT 500")?;
        self.add_column_if_not_exists("tuner_config", "set_channel_retry_timeout_ms", "INTEGER DEFAULT 10000")?;
        self.add_column_if_not_exists("tuner_config", "signal_poll_interval_ms", "INTEGER DEFAULT 500")?;
        self.add_column_if_not_exists("tuner_config", "signal_wait_timeout_ms", "INTEGER DEFAULT 10000")?;
        Ok(())
    }

    // Migration 005 (was 006): Add loss_summary column to session_history if
    // it doesn't exist (STREAMING_DESIGN.md P1: per-loss-source counters +
    // top-loss PIDs, JSON encoded).
    fn migration_005_session_history_loss_summary(&self) -> Result<()> {
        self.add_column_if_not_exists("session_history", "loss_summary", "TEXT")?;
        Ok(())
    }

    // Migration 006 (was 007): Add max_concurrent_encoders column to
    // tsreplace_config if it doesn't exist (STREAMING_DESIGN.md §5/§9 P4:
    // shared encoder pool).
    fn migration_006_tsreplace_max_concurrent_encoders(&self) -> Result<()> {
        self.add_column_if_not_exists("tsreplace_config", "max_concurrent_encoders", "INTEGER DEFAULT 2")?;
        Ok(())
    }

    // Migration 007 (was 011): Add optional preprocessor (stage-1 command,
    // e.g. tsreadex) columns to tsreplace_config. Empty string = no
    // preprocessor (legacy single-stage behavior), so existing rows keep
    // working unchanged. `preprocessor_path` is TOML-only like
    // `command_path` (REVIEW S1); `preprocessor_arguments` is API-editable
    // like `arguments`.
    fn migration_007_tsreplace_preprocessor_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("tsreplace_config", "preprocessor_path", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("tsreplace_config", "preprocessor_arguments", "TEXT DEFAULT ''")?;
        Ok(())
    }

    // Migration 008 (was 012, legacy): `tsreplace_config.preview_enabled`
    // briefly gated the HTTP `?profile=preview` path. Superseded by
    // Migration 009's dedicated `preview_encoder_config` table. The column
    // is kept (never dropped) but is NO LONGER REFERENCED anywhere except
    // the one-time carry-over in `migration_009_preview_encoder_config_seed`.
    fn migration_008_tsreplace_preview_enabled_legacy(&self) -> Result<()> {
        self.add_column_if_not_exists("tsreplace_config", "preview_enabled", "INTEGER DEFAULT 0")?;
        Ok(())
    }

    // Migration 009 (was 013): dedicated browser-preview encoder settings
    // table, fully separated from the BNDP (TVTest) `tsreplace_config`. The
    // table itself is created by `schema::SCHEMA_SQL`; this step seeds it
    // (carrying `tsreplace_config.preview_enabled` over into its `enabled`).
    fn migration_009_preview_encoder_config_seed(&self) -> Result<()> {
        self.ensure_preview_encoder_config_compat()
    }

    // Migration 010 (was 008): Add stream_class column to session_history if
    // it doesn't exist (STREAMING_DESIGN.md §2 P2: stream reliability class,
    // recorded at session end).
    fn migration_010_session_history_stream_class(&self) -> Result<()> {
        self.add_column_if_not_exists("session_history", "stream_class", "TEXT")?;
        Ok(())
    }

    // Migration 011 (was 009): Add prefill/jitter buffer columns to
    // tuner_config if they don't exist (STREAMING_DESIGN.md §4/§9 P3:
    // fixed-duration prefill/jitter buffer, sized per stream class).
    fn migration_011_tuner_prefill_jitter_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("tuner_config", "prefill_view_ms", "INTEGER DEFAULT 1000")?;
        self.add_column_if_not_exists("tuner_config", "prefill_preview_ms", "INTEGER DEFAULT 2000")?;
        self.add_column_if_not_exists("tuner_config", "prefill_record_ms", "INTEGER DEFAULT 6000")?;
        self.add_column_if_not_exists("tuner_config", "jitter_safety_factor", "REAL DEFAULT 1.5")?;
        Ok(())
    }

    // Migration 012 (was 010): encode_profiles is a brand-new table
    // (STREAMING_DESIGN.md §5.3/§9 P5), so `CREATE TABLE IF NOT EXISTS` in
    // schema.rs already creates it for both fresh and pre-existing databases
    // — no add_column_if_not_exists step is needed here (that mechanism is
    // only for adding columns to tables that already exist). Default-row
    // seeding happens separately in `initialize_schema` via
    // `seed_default_encode_profiles`, since schema DDL can't express data
    // seeding. Kept as an explicit no-op ledger entry to preserve the
    // effective step order.
    fn migration_012_encode_profiles_noop(&self) -> Result<()> {
        Ok(())
    }

    // Migration 013 (was 002): Fill band_type and terrestrial_region for
    // existing channels. This updates all NULL values in these columns
    // based on NID.
    fn migration_013_backfill_channels_band_terrestrial_region(&self) -> Result<()> {
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
    ///
    /// The last four fields are the fixed-duration prefill/jitter buffer
    /// settings (STREAMING_DESIGN.md §4/§9 P3): `prefill_view_ms`,
    /// `prefill_preview_ms`, `prefill_record_ms`, `jitter_safety_factor`.
    #[allow(clippy::type_complexity)]
    pub fn get_tuner_config(&self) -> Result<(u64, bool, u64, u64, u64, u64, u64, u64, u64, u64, f64)> {
        let mut stmt = self.conn.prepare(
            "SELECT keep_alive_secs, prewarm_enabled, prewarm_timeout_secs,
                    set_channel_retry_interval_ms, set_channel_retry_timeout_ms,
                    signal_poll_interval_ms, signal_wait_timeout_ms,
                    prefill_view_ms, prefill_preview_ms, prefill_record_ms,
                    jitter_safety_factor
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
                row.get::<_, u64>(7)?,
                row.get::<_, u64>(8)?,
                row.get::<_, u64>(9)?,
                row.get::<_, f64>(10)?,
            ))
        });

        match result {
            Ok(config) => Ok(config),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO tuner_config
                     (id, keep_alive_secs, prewarm_enabled, prewarm_timeout_secs,
                      set_channel_retry_interval_ms, set_channel_retry_timeout_ms,
                      signal_poll_interval_ms, signal_wait_timeout_ms,
                      prefill_view_ms, prefill_preview_ms, prefill_record_ms,
                      jitter_safety_factor)
                     VALUES (1, 60, 1, 30, 500, 10000, 500, 10000, 1000, 2000, 6000, 1.5)",
                    [],
                )?;
                Ok((60, true, 30, 500, 10000, 500, 10000, 1000, 2000, 6000, 1.5))
            }
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Update tuner optimization configuration.
    ///
    /// See [`Database::get_tuner_config`] for the meaning of the last four
    /// (prefill/jitter) parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn update_tuner_config(
        &self,
        keep_alive_secs: u64,
        prewarm_enabled: bool,
        prewarm_timeout_secs: u64,
        set_channel_retry_interval_ms: u64,
        set_channel_retry_timeout_ms: u64,
        signal_poll_interval_ms: u64,
        signal_wait_timeout_ms: u64,
        prefill_view_ms: u64,
        prefill_preview_ms: u64,
        prefill_record_ms: u64,
        jitter_safety_factor: f64,
    ) -> Result<()> {
        let prewarm_enabled = if prewarm_enabled { 1 } else { 0 };
        self.conn.execute(
            "INSERT OR REPLACE INTO tuner_config
             (id, keep_alive_secs, prewarm_enabled, prewarm_timeout_secs,
              set_channel_retry_interval_ms, set_channel_retry_timeout_ms,
              signal_poll_interval_ms, signal_wait_timeout_ms,
              prefill_view_ms, prefill_preview_ms, prefill_record_ms,
              jitter_safety_factor, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, strftime('%s', 'now'))",
            rusqlite::params![
                keep_alive_secs,
                prewarm_enabled,
                prewarm_timeout_secs,
                set_channel_retry_interval_ms,
                set_channel_retry_timeout_ms,
                signal_poll_interval_ms,
                signal_wait_timeout_ms,
                prefill_view_ms,
                prefill_preview_ms,
                prefill_record_ms,
                jitter_safety_factor,
            ],
        )?;
        Ok(())
    }
}

/// tsreplace configuration storage.
impl Database {
    // NOTE: `tsreplace_config`'s table itself is created by
    // `schema::SCHEMA_SQL` (`CREATE TABLE IF NOT EXISTS`); this fn only
    // guards individual columns added by later migrations, plus seeding.
    fn ensure_tsreplace_config_compat(&self) -> Result<()> {
        self.add_column_if_not_exists("tsreplace_config", "enabled", "INTEGER DEFAULT 0")?;
        self.add_column_if_not_exists("tsreplace_config", "command_path", "TEXT DEFAULT 'tsreplace'")?;
        self.add_column_if_not_exists("tsreplace_config", "arguments", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("tsreplace_config", "read_timeout_ms", "INTEGER DEFAULT 10000")?;
        self.add_column_if_not_exists("tsreplace_config", "passthrough_on_error", "INTEGER DEFAULT 1")?;
        self.add_column_if_not_exists("tsreplace_config", "max_concurrent_encoders", "INTEGER DEFAULT 2")?;
        self.add_column_if_not_exists("tsreplace_config", "preprocessor_path", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("tsreplace_config", "preprocessor_arguments", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("tsreplace_config", "preview_enabled", "INTEGER DEFAULT 0")?;
        self.add_column_if_not_exists(
            "tsreplace_config",
            "updated_at",
            "INTEGER DEFAULT (strftime('%s', 'now'))",
        )?;

        self.conn.execute(
            "INSERT OR IGNORE INTO tsreplace_config
             (id, enabled, command_path, arguments, read_timeout_ms, passthrough_on_error, max_concurrent_encoders, updated_at)
             VALUES (1, 0, ?1, ?2, ?3, ?4, ?5, strftime('%s', 'now'))",
            rusqlite::params![
                DEFAULT_TSREPLACE_COMMAND_PATH,
                DEFAULT_TSREPLACE_ARGUMENTS,
                DEFAULT_TSREPLACE_READ_TIMEOUT_MS,
                if DEFAULT_TSREPLACE_PASSTHROUGH_ON_ERROR { 1 } else { 0 },
                DEFAULT_TSREPLACE_MAX_CONCURRENT_ENCODERS
            ],
        )?;

        // If parameters are unspecified in legacy DB rows, apply requested defaults.
        self.conn.execute(
            "UPDATE tsreplace_config
             SET command_path = ?1,
                 updated_at = strftime('%s', 'now')
             WHERE id = 1 AND (command_path IS NULL OR trim(command_path) = '')",
            rusqlite::params![DEFAULT_TSREPLACE_COMMAND_PATH],
        )?;
        self.conn.execute(
            "UPDATE tsreplace_config
             SET arguments = ?1,
                 updated_at = strftime('%s', 'now')
             WHERE id = 1 AND (arguments IS NULL OR trim(arguments) = '')",
            rusqlite::params![DEFAULT_TSREPLACE_ARGUMENTS],
        )?;
        self.conn.execute(
            "UPDATE tsreplace_config
             SET read_timeout_ms = ?1,
                 updated_at = strftime('%s', 'now')
             WHERE id = 1 AND (read_timeout_ms IS NULL OR read_timeout_ms <= 0)",
            rusqlite::params![DEFAULT_TSREPLACE_READ_TIMEOUT_MS],
        )?;
        self.conn.execute(
            "UPDATE tsreplace_config
             SET max_concurrent_encoders = ?1,
                 updated_at = strftime('%s', 'now')
             WHERE id = 1 AND (max_concurrent_encoders IS NULL OR max_concurrent_encoders <= 0)",
            rusqlite::params![DEFAULT_TSREPLACE_MAX_CONCURRENT_ENCODERS],
        )?;

        Ok(())
    }

    /// Get tsreplace (BNDP/TVTest session encode pipeline) configuration.
    ///
    /// Returns `(enabled, command_path, arguments, read_timeout_ms, passthrough_on_error,
    /// max_concurrent_encoders, preprocessor_path, preprocessor_arguments)`.
    /// `max_concurrent_encoders` caps the number of concurrently-running
    /// shared encoder chains (STREAMING_DESIGN.md §5 P4); sessions sharing the same
    /// channel/SID-set/config generation join a single running encoder instead of
    /// consuming a slot. `preprocessor_path` (empty = none) is an optional
    /// stage-1 command (e.g. tsreadex) piped before `command_path`; like
    /// `command_path` it is TOML-only (REVIEW S1).
    ///
    /// This table gates ONLY the BNDP (TVTest) session pipeline. The HTTP
    /// `?profile=preview` path has its own fully separate settings — see
    /// [`Self::get_preview_encoder_config`]. (The legacy `preview_enabled`
    /// column still physically exists but is deliberately not returned.)
    #[allow(clippy::type_complexity)]
    pub fn get_tsreplace_config(&self) -> Result<(bool, String, String, u64, bool, i64, String, String)> {
        self.ensure_tsreplace_config_compat()?;

        let mut stmt = self.conn.prepare(
            "SELECT enabled, command_path, arguments, read_timeout_ms, passthrough_on_error, max_concurrent_encoders,
                    preprocessor_path, preprocessor_arguments
             FROM tsreplace_config WHERE id = 1"
        )?;

        let result = stmt.query_row([], |row| {
            Ok((
                row.get::<_, i64>(0)? != 0,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, i64>(4)? != 0,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            ))
        });

        match result {
            Ok(config) => Ok(config),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO tsreplace_config
                     (id, enabled, command_path, arguments, read_timeout_ms, passthrough_on_error, max_concurrent_encoders)
                     VALUES (1, 0, ?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        DEFAULT_TSREPLACE_COMMAND_PATH,
                        DEFAULT_TSREPLACE_ARGUMENTS,
                        DEFAULT_TSREPLACE_READ_TIMEOUT_MS,
                        if DEFAULT_TSREPLACE_PASSTHROUGH_ON_ERROR { 1 } else { 0 },
                        DEFAULT_TSREPLACE_MAX_CONCURRENT_ENCODERS
                    ],
                )?;
                Ok((
                    false,
                    DEFAULT_TSREPLACE_COMMAND_PATH.to_string(),
                    DEFAULT_TSREPLACE_ARGUMENTS.to_string(),
                    DEFAULT_TSREPLACE_READ_TIMEOUT_MS,
                    DEFAULT_TSREPLACE_PASSTHROUGH_ON_ERROR,
                    DEFAULT_TSREPLACE_MAX_CONCURRENT_ENCODERS,
                    String::new(),
                    String::new(),
                ))
            }
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Update tsreplace configuration.
    ///
    /// `preprocessor_path` is included so `INSERT OR REPLACE` never clobbers
    /// it — API callers must pass back the value they read (same convention
    /// as `command_path`; both are TOML-only, REVIEW S1).
    #[allow(clippy::too_many_arguments)]
    pub fn update_tsreplace_config(
        &self,
        enabled: bool,
        command_path: &str,
        arguments: &str,
        read_timeout_ms: u64,
        passthrough_on_error: bool,
        max_concurrent_encoders: i64,
        preprocessor_path: &str,
        preprocessor_arguments: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO tsreplace_config
             (id, enabled, command_path, arguments, read_timeout_ms, passthrough_on_error, max_concurrent_encoders,
              preprocessor_path, preprocessor_arguments, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%s', 'now'))",
            rusqlite::params![
                if enabled { 1 } else { 0 },
                command_path,
                arguments,
                read_timeout_ms,
                if passthrough_on_error { 1 } else { 0 },
                max_concurrent_encoders,
                preprocessor_path,
                preprocessor_arguments
            ],
        )?;
        Ok(())
    }

    /// Set the tsreplace external-command path.
    ///
    /// # Security (trust boundary — REVIEW_2026-07.md S1)
    ///
    /// `command_path` is the program the server executes via
    /// `Command::new(command_path)` to pipe TS through an external encoder.
    /// It **must only ever be set from the TOML config file** (see
    /// `main.rs`, which calls this once at startup from `[tsreplace]
    /// command_path`). It must never be reachable from a Web API handler:
    /// the API is accessible to anyone who can reach the dashboard (LAN, or
    /// a CSRF'd browser request), and allowing `command_path` to be changed
    /// there would be a straightforward path to remote code execution.
    /// `arguments` (also user-influenced) stays API-editable because it is
    /// passed as an argument vector, not resolved as a program to execute.
    pub fn set_tsreplace_command_path(&self, command_path: &str) -> Result<()> {
        self.ensure_tsreplace_config_compat()?;
        self.conn.execute(
            "UPDATE tsreplace_config SET command_path = ?1, updated_at = strftime('%s', 'now') WHERE id = 1",
            rusqlite::params![command_path],
        )?;
        Ok(())
    }

    /// Set the optional preprocessor (stage-1) command path.
    ///
    /// # Security (trust boundary — REVIEW_2026-07.md S1)
    ///
    /// Exactly like [`Self::set_tsreplace_command_path`]: this is a program
    /// the server executes (`Command::new(preprocessor_path)`), so it **must
    /// only ever be set from the TOML config file** (`[tsreplace]
    /// preprocessor_path` in `main.rs`) and never from a Web API handler.
    /// An empty string disables the preprocessor stage.
    pub fn set_tsreplace_preprocessor_path(&self, preprocessor_path: &str) -> Result<()> {
        self.ensure_tsreplace_config_compat()?;
        self.conn.execute(
            "UPDATE tsreplace_config SET preprocessor_path = ?1, updated_at = strftime('%s', 'now') WHERE id = 1",
            rusqlite::params![preprocessor_path],
        )?;
        Ok(())
    }
}

/// Browser-preview encoder configuration storage (Migration 009).
///
/// Fully separate from `tsreplace_config`: this table gates and configures
/// ONLY the HTTP `?profile=preview` streaming path (`web/stream.rs`), while
/// `tsreplace_config` gates and configures ONLY the BNDP (TVTest) session
/// pipeline (`server/session.rs`). The two pipelines share nothing but the
/// `EncoderPool` itself (and thus `tsreplace_config.max_concurrent_encoders`,
/// which caps the pool's total concurrently-running chains as a hardware
/// resource limit, not a per-pipeline setting).
impl Database {
    // NOTE: `preview_encoder_config`'s table itself is created by
    // `schema::SCHEMA_SQL` (`CREATE TABLE IF NOT EXISTS`); this fn only
    // guards individual columns plus one-time seeding.
    fn ensure_preview_encoder_config_compat(&self) -> Result<()> {
        // The seed below carries the legacy `tsreplace_config.preview_enabled`
        // over, so make sure that table/column exists first.
        self.ensure_tsreplace_config_compat()?;

        self.add_column_if_not_exists("preview_encoder_config", "enabled", "INTEGER DEFAULT 0")?;
        self.add_column_if_not_exists("preview_encoder_config", "command_path", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("preview_encoder_config", "preprocessor_path", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("preview_encoder_config", "preprocessor_arguments", "TEXT DEFAULT ''")?;
        self.add_column_if_not_exists("preview_encoder_config", "read_timeout_ms", "INTEGER DEFAULT 10000")?;
        self.add_column_if_not_exists(
            "preview_encoder_config",
            "updated_at",
            "INTEGER DEFAULT (strftime('%s', 'now'))",
        )?;

        // One-time seed. `enabled` carries over the short-lived legacy
        // `tsreplace_config.preview_enabled` flag (Migration 012) so anyone
        // who already turned browser preview on does not silently lose it.
        // `preprocessor_arguments` starts from the recommended tsreadex
        // template; the executable paths stay empty (TOML-only, REVIEW S1).
        self.conn.execute(
            "INSERT OR IGNORE INTO preview_encoder_config
             (id, enabled, command_path, preprocessor_path, preprocessor_arguments, read_timeout_ms, updated_at)
             SELECT 1,
                    COALESCE((SELECT preview_enabled FROM tsreplace_config WHERE id = 1), 0),
                    '', '', ?1, 10000, strftime('%s', 'now')",
            rusqlite::params![DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS],
        )?;

        // Same "fill if unspecified" convention as tsreplace_config above: an
        // empty preprocessor_arguments is never functional (tsreadex needs at
        // least the trailing `-`), so backfill the recommended template into
        // rows seeded before it existed.
        self.conn.execute(
            "UPDATE preview_encoder_config
             SET preprocessor_arguments = ?1,
                 updated_at = strftime('%s', 'now')
             WHERE id = 1 AND (preprocessor_arguments IS NULL OR trim(preprocessor_arguments) = '')",
            rusqlite::params![DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS],
        )?;

        Ok(())
    }

    /// Get the browser-preview encoder configuration.
    ///
    /// Returns `(enabled, command_path, preprocessor_path,
    /// preprocessor_arguments, read_timeout_ms)`. `command_path` and
    /// `preprocessor_path` are TOML-only (`[preview]` section, REVIEW S1);
    /// empty `command_path` means "not configured yet" and the preview path
    /// refuses to start. The encode arguments themselves come from the
    /// `encode_profiles` row with `purpose='preview'`, not from here.
    pub fn get_preview_encoder_config(&self) -> Result<(bool, String, String, String, u64)> {
        self.ensure_preview_encoder_config_compat()?;

        let mut stmt = self.conn.prepare(
            "SELECT enabled, command_path, preprocessor_path, preprocessor_arguments, read_timeout_ms
             FROM preview_encoder_config WHERE id = 1",
        )?;

        stmt.query_row([], |row| {
            Ok((
                row.get::<_, i64>(0)? != 0,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<u64>>(4)?.unwrap_or(10_000),
            ))
        })
        .map_err(DatabaseError::Sqlite)
    }

    /// Update the API-editable browser-preview settings: `enabled`,
    /// `preprocessor_arguments`, `read_timeout_ms`. The two executable paths
    /// are deliberately NOT parameters here — they stay TOML-only (REVIEW
    /// S1, see [`Self::set_preview_command_path`]).
    pub fn update_preview_encoder_config(
        &self,
        enabled: bool,
        preprocessor_arguments: &str,
        read_timeout_ms: u64,
    ) -> Result<()> {
        self.ensure_preview_encoder_config_compat()?;
        self.conn.execute(
            "UPDATE preview_encoder_config
             SET enabled = ?1, preprocessor_arguments = ?2, read_timeout_ms = ?3,
                 updated_at = strftime('%s', 'now')
             WHERE id = 1",
            rusqlite::params![if enabled { 1 } else { 0 }, preprocessor_arguments, read_timeout_ms],
        )?;
        Ok(())
    }

    /// Set the preview encoder executable path.
    ///
    /// # Security (trust boundary — REVIEW_2026-07.md S1)
    /// Same rule as [`Self::set_tsreplace_command_path`]: this is a program
    /// the server executes, so it must only ever be set from the TOML config
    /// file (`[preview] command_path` in `main.rs`), never from a Web API
    /// handler.
    pub fn set_preview_command_path(&self, command_path: &str) -> Result<()> {
        self.ensure_preview_encoder_config_compat()?;
        self.conn.execute(
            "UPDATE preview_encoder_config SET command_path = ?1, updated_at = strftime('%s', 'now') WHERE id = 1",
            rusqlite::params![command_path],
        )?;
        Ok(())
    }

    /// Set the preview preprocessor (stage-1, e.g. tsreadex) executable path.
    ///
    /// # Security (trust boundary — REVIEW_2026-07.md S1)
    /// TOML-only (`[preview] preprocessor_path`), same as
    /// [`Self::set_preview_command_path`]. Empty string disables the
    /// preprocessor stage.
    pub fn set_preview_preprocessor_path(&self, preprocessor_path: &str) -> Result<()> {
        self.ensure_preview_encoder_config_compat()?;
        self.conn.execute(
            "UPDATE preview_encoder_config SET preprocessor_path = ?1, updated_at = strftime('%s', 'now') WHERE id = 1",
            rusqlite::params![preprocessor_path],
        )?;
        Ok(())
    }
}

/// Web API authentication token storage (REVIEW_2026-07.md S2).
///
/// NOTE: `web_auth_config`'s table is created by `schema::SCHEMA_SQL`
/// (`CREATE TABLE IF NOT EXISTS`) — no ad-hoc columns have ever been added
/// to it, so unlike the other config tables above there is no
/// `ensure_*_compat` guard needed here.
impl Database {
    /// Get the persisted Web API bearer token, if one has been generated or
    /// configured before.
    pub fn get_web_auth_token(&self) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT auth_token FROM web_auth_config WHERE id = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        );

        match result {
            Ok(token) => Ok(token.filter(|t| !t.trim().is_empty())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Persist the Web API bearer token (generated once at first startup, or
    /// set explicitly via TOML `[web] auth_token`).
    pub fn set_web_auth_token(&self, token: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO web_auth_config (id, auth_token, updated_at)
             VALUES (1, ?1, strftime('%s', 'now'))
             ON CONFLICT(id) DO UPDATE SET auth_token = excluded.auth_token, updated_at = excluded.updated_at",
            rusqlite::params![token],
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

    /// Migration 016 must correct terrestrial rows that migration 013's
    /// narrow 0x7F00-0x7FFF SQL range misclassified as band_type=4 (Other),
    /// and backfill NULL region columns — idempotently.
    #[test]
    fn migration_016_reclassifies_misbanded_terrestrial_rows() {
        let db = Database::open_in_memory().unwrap();
        let driver_id = db.get_or_create_bon_driver("Test.dll").unwrap();
        // 0x7880 (福岡・北九州) sits below 013's 0x7F00 cutoff.
        db.connection()
            .execute(
                "INSERT INTO channels (bon_driver_id, nid, sid, tsid, band_type)
                 VALUES (?1, 0x7880, 1, 1, 4)",
                [driver_id],
            )
            .unwrap();

        db.migration_016_reclassify_band_region_from_nid().unwrap();
        let (band, region): (i64, Option<String>) = db
            .connection()
            .query_row(
                "SELECT band_type, terrestrial_region FROM channels WHERE nid = 0x7880",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(band, 0, "0x7880 is terrestrial, not Other");
        assert!(region.is_some(), "terrestrial region should be backfilled");

        // Idempotent: a second run changes nothing.
        db.migration_016_reclassify_band_region_from_nid().unwrap();
        let band2: i64 = db
            .connection()
            .query_row("SELECT band_type FROM channels WHERE nid = 0x7880", [], |row| row.get(0))
            .unwrap();
        assert_eq!(band2, 0);
    }

    #[test]
    fn web_auth_token_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.get_web_auth_token().unwrap(), None);

        db.set_web_auth_token("abc123").unwrap();
        assert_eq!(db.get_web_auth_token().unwrap(), Some("abc123".to_string()));

        // Overwriting replaces the stored token rather than erroring.
        db.set_web_auth_token("def456").unwrap();
        assert_eq!(db.get_web_auth_token().unwrap(), Some("def456".to_string()));
    }

    #[test]
    fn set_tsreplace_command_path_only_changes_command_path() {
        let db = Database::open_in_memory().unwrap();
        db.update_tsreplace_config(true, "old-path", "--foo", 5000, false, 3, "pre-path", "--pre")
            .unwrap();

        db.set_tsreplace_command_path("/usr/local/bin/tsreplace").unwrap();

        let (enabled, command_path, arguments, read_timeout_ms, passthrough_on_error, max_concurrent_encoders, preprocessor_path, preprocessor_arguments) =
            db.get_tsreplace_config().unwrap();
        assert_eq!(command_path, "/usr/local/bin/tsreplace");
        // Everything else must be preserved.
        assert!(enabled);
        assert_eq!(arguments, "--foo");
        assert_eq!(read_timeout_ms, 5000);
        assert!(!passthrough_on_error);
        assert_eq!(max_concurrent_encoders, 3);
        assert_eq!(preprocessor_path, "pre-path");
        assert_eq!(preprocessor_arguments, "--pre");
    }

    #[test]
    fn tsreplace_preprocessor_defaults_empty_and_roundtrips() {
        let db = Database::open_in_memory().unwrap();

        // Fresh DB: preprocessor is absent (empty strings).
        let (_, _, _, _, _, _, pre_path, pre_args) = db.get_tsreplace_config().unwrap();
        assert_eq!(pre_path, "");
        assert_eq!(pre_args, "");

        // TOML-only setter changes only the path.
        db.set_tsreplace_preprocessor_path("C:/DTV/tsreadex/tsreadex.exe").unwrap();
        let (_, _, _, _, _, _, pre_path, pre_args) = db.get_tsreplace_config().unwrap();
        assert_eq!(pre_path, "C:/DTV/tsreadex/tsreadex.exe");
        assert_eq!(pre_args, "");

        // update_tsreplace_config persists preprocessor_arguments.
        db.update_tsreplace_config(
            true,
            "QSVEncC",
            "-i - -o -",
            5000,
            true,
            2,
            "C:/DTV/tsreadex/tsreadex.exe",
            "-x 18 -n {SID} -",
        )
        .unwrap();
        let (_, _, _, _, _, _, pre_path, pre_args) = db.get_tsreplace_config().unwrap();
        assert_eq!(pre_path, "C:/DTV/tsreadex/tsreadex.exe");
        assert_eq!(pre_args, "-x 18 -n {SID} -");
    }

    #[test]
    fn preview_encoder_config_defaults_and_roundtrips() {
        let db = Database::open_in_memory().unwrap();

        // Fresh DB: disabled, no paths, recommended tsreadex arguments
        // pre-seeded, default timeout.
        let (enabled, cmd, pre_path, pre_args, timeout) = db.get_preview_encoder_config().unwrap();
        assert!(!enabled);
        assert_eq!(cmd, "");
        assert_eq!(pre_path, "");
        assert_eq!(pre_args, DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS);
        assert_eq!(timeout, 10_000);

        // TOML-only setters change only their own column.
        db.set_preview_command_path("C:/enc/QSVEncC64.exe").unwrap();
        db.set_preview_preprocessor_path("C:/pre/tsreadex.exe").unwrap();
        // API-editable trio roundtrips.
        db.update_preview_encoder_config(true, "-x 18 -n {SID} -", 20_000).unwrap();

        let (enabled, cmd, pre_path, pre_args, timeout) = db.get_preview_encoder_config().unwrap();
        assert!(enabled);
        assert_eq!(cmd, "C:/enc/QSVEncC64.exe");
        assert_eq!(pre_path, "C:/pre/tsreadex.exe");
        assert_eq!(pre_args, "-x 18 -n {SID} -");
        assert_eq!(timeout, 20_000);
    }

    #[test]
    fn preview_preprocessor_arguments_backfilled_when_empty() {
        let db = Database::open_in_memory().unwrap();

        // A row left empty (pre-backfill-era DB, or cleared via the API) is
        // refilled with the recommended tsreadex template on the next read...
        db.conn
            .execute("UPDATE preview_encoder_config SET preprocessor_arguments = '' WHERE id = 1", [])
            .unwrap();
        let (_, _, _, pre_args, _) = db.get_preview_encoder_config().unwrap();
        assert_eq!(pre_args, DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS);

        // ...while a deliberately customized value is left alone.
        db.update_preview_encoder_config(false, "-n {SID} -", 10_000).unwrap();
        let (_, _, _, pre_args, _) = db.get_preview_encoder_config().unwrap();
        assert_eq!(pre_args, "-n {SID} -");
    }

    #[test]
    fn preview_encoder_config_is_independent_from_tsreplace_config() {
        let db = Database::open_in_memory().unwrap();

        // Scribble all over the BNDP-side config...
        db.update_tsreplace_config(true, "garbage-cmd", "--garbage", 1, false, 1, "garbage-pre", "--garbage-pre")
            .unwrap();
        // ...and the preview side must be completely unaffected (it keeps its
        // own seeded defaults, not the BNDP-side garbage).
        let (enabled, cmd, pre_path, pre_args, timeout) = db.get_preview_encoder_config().unwrap();
        assert!(!enabled);
        assert_eq!(cmd, "");
        assert_eq!(pre_path, "");
        assert_eq!(pre_args, DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS);
        assert_eq!(timeout, 10_000);

        // And the reverse: preview updates never leak into tsreplace_config.
        db.set_preview_command_path("C:/enc/QSVEncC64.exe").unwrap();
        db.update_preview_encoder_config(true, "-n {SID} -", 5_000).unwrap();
        let (enabled, command_path, arguments, ..) = db.get_tsreplace_config().unwrap();
        assert!(enabled);
        assert_eq!(command_path, "garbage-cmd");
        assert_eq!(arguments, "--garbage");
    }

    #[test]
    fn preview_encoder_config_carries_over_legacy_preview_enabled() {
        let db = Database::open_in_memory().unwrap();

        // Simulate a DB from the short-lived Migration-012 era: legacy flag
        // set to 1 and no preview_encoder_config row yet.
        db.conn
            .execute("UPDATE tsreplace_config SET preview_enabled = 1 WHERE id = 1", [])
            .unwrap();
        db.conn.execute("DELETE FROM preview_encoder_config", []).unwrap();

        let (enabled, ..) = db.get_preview_encoder_config().unwrap();
        assert!(enabled, "legacy tsreplace_config.preview_enabled=1 must carry over");
    }

    /// M6 (docs/SYSTEM_REVIEW_2026-07.md Phase 14): a fresh DB ends up with
    /// `user_version == MIGRATIONS.len()`. Forcing `user_version` back to 0
    /// simulates every pre-ledger production DB (already fully migrated via
    /// the old ad-hoc code path, but starting the new ledger at position 0)
    /// — re-running `apply_migrations` must replay every body harmlessly and
    /// land back on the same `user_version`.
    #[test]
    fn migrations_replay_harmlessly_from_user_version_zero() {
        let db = Database::open_in_memory().unwrap();

        let target = Database::MIGRATIONS.len() as i64;
        let initial_version: i64 = db
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(initial_version, target, "fresh DB should be fully migrated");

        // Force user_version back to 0, as a real pre-ledger production DB
        // would have (it was migrated ad-hoc, so every column/table already
        // exists despite the ledger position being 0).
        db.connection().execute_batch("PRAGMA user_version = 0;").unwrap();

        db.apply_migrations().unwrap();

        let final_version: i64 = db
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(final_version, target);
    }
}
