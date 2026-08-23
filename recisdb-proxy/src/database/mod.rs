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
pub use encode_profile::{preview_encode_args_ffmpeg, preview_extra_args_is_auto_generated, video_encoder_tuning};

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
        ("017_card_reader_name", Database::migration_017_card_reader_name),
        ("018_ts_queue_duration_columns", Database::migration_018_ts_queue_duration_columns),
        ("019_bon_driver_disable_b25", Database::migration_019_bon_driver_disable_b25),
        ("020_bon_driver_stream_format", Database::migration_020_bon_driver_stream_format),
        ("021_github_token", Database::migration_021_github_token),
        ("022_log_config", Database::migration_022_log_config),
        ("023_tuner_livelock_config", Database::migration_023_tuner_livelock_config),
        (
            "024_driver_runtime_health",
            Database::migration_024_driver_runtime_health,
        ),
    ];

    /// Migration 024: BonDriver runtime health (startup latency, stalls,
    /// failures). Previously created lazily on first access, which worked but
    /// put a schema definition outside this ledger; CLAUDE.md requires every
    /// schema change to be an entry here so a pre-ledger database replays it
    /// from `user_version = 0`. Idempotent, like every other entry.
    fn migration_024_driver_runtime_health(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS driver_runtime_health (
                bon_driver_id INTEGER PRIMARY KEY,
                samples INTEGER NOT NULL DEFAULT 0,
                open_latency_ewma_ms REAL,
                tune_latency_ewma_ms REAL,
                first_ts_latency_ewma_ms REAL,
                stall_count INTEGER NOT NULL DEFAULT 0,
                open_failures INTEGER NOT NULL DEFAULT 0,
                tune_failures INTEGER NOT NULL DEFAULT 0,
                first_ts_timeouts INTEGER NOT NULL DEFAULT 0,
                worker_restarts INTEGER NOT NULL DEFAULT 0,
                runtime_score REAL NOT NULL DEFAULT 1.0,
                last_updated INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                FOREIGN KEY(bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
            );",
        )?;
        Ok(())
    }

    /// Migration 022: log level/retention, moved from the TOML `[logging]`
    /// section into the database so the Web dashboard can change them
    /// without a restart (level, via `logging::LogLevelHandle::set_level`)
    /// or a redeploy (retention_days). Single-row table (`id = 1`), same
    /// pattern as `tuner_config`/`scan_scheduler_config`.
    fn migration_022_log_config(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS log_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                level TEXT NOT NULL DEFAULT 'info',
                retention_days INTEGER NOT NULL DEFAULT 7
            );
            INSERT OR IGNORE INTO log_config (id, level, retention_days) VALUES (1, 'info', 7);",
        )?;
        Ok(())
    }

    /// Livelock protection settings. Kept as one idempotent migration so old
    /// databases receive all three controls together.
    fn migration_023_tuner_livelock_config(&self) -> Result<()> {
        self.add_column_if_not_exists("tuner_config", "min_hold_secs", "INTEGER NOT NULL DEFAULT 10")?;
        self.add_column_if_not_exists("tuner_config", "reject_cooldown_ms", "INTEGER NOT NULL DEFAULT 2000")?;
        self.add_column_if_not_exists("tuner_config", "no_data_timeout_secs", "INTEGER NOT NULL DEFAULT 30")?;
        Ok(())
    }

    /// Log level/retention configuration (`[logging]` moved from TOML to the
    /// DB). Seeds the default row if missing, same as [`Self::get_tuner_config`].
    pub fn get_log_config(&self) -> Result<(String, u64)> {
        let result = self.conn.query_row(
            "SELECT level, retention_days FROM log_config WHERE id = 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        );

        match result {
            Ok(config) => Ok(config),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO log_config (id, level, retention_days) VALUES (1, 'info', 7)",
                    [],
                )?;
                Ok(("info".to_string(), 7))
            }
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Update log level/retention configuration.
    pub fn update_log_config(&self, level: &str, retention_days: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO log_config (id, level, retention_days) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET level = ?1, retention_days = ?2",
            rusqlite::params![level, retention_days],
        )?;
        Ok(())
    }

    /// Migration 021: GitHub token for development-build updates.
    ///
    /// GitHub's artifact download endpoint requires authentication **even for
    /// public repositories** (a `repo`-scoped token), unlike release assets
    /// which are anonymous. Updating to a CI build is therefore impossible
    /// without one, so it is stored next to the Web API token rather than in
    /// the config file: it is a credential the operator pastes once from the
    /// dashboard, not a deployment setting.
    fn migration_021_github_token(&self) -> Result<()> {
        self.add_column_if_not_exists("web_auth_config", "github_token", "TEXT")
    }

    /// GitHub token used to download CI artifacts, if configured.
    pub fn get_github_token(&self) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT github_token FROM web_auth_config WHERE id = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        );
        match result {
            Ok(token) => Ok(token.filter(|t| !t.trim().is_empty())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DatabaseError::Sqlite(e)),
        }
    }

    /// Store (or, with an empty string, clear) the GitHub token.
    pub fn set_github_token(&self, token: &str) -> Result<()> {
        let trimmed = token.trim();
        let value = if trimmed.is_empty() { None } else { Some(trimmed) };
        self.conn.execute(
            "INSERT INTO web_auth_config (id, github_token, updated_at)
             VALUES (1, ?1, strftime('%s','now'))
             ON CONFLICT(id) DO UPDATE SET github_token = ?1, updated_at = strftime('%s','now')",
            rusqlite::params![value],
        )?;
        Ok(())
    }

    /// Migration 020: what the driver actually hands back.
    ///
    /// `'ts'` (default) = MPEG-2 TS, the only thing the rest of the pipeline
    /// understands. `'mmttlv'` = raw MMT/TLV, i.e. a 4K tuner, which has to go
    /// through the external converter first (`tuner/mmt_pipe.rs`).
    ///
    /// This cannot be derived from a scan the way `band_type` is: scanning
    /// parses TS, so a raw 4K tuner produces nothing to classify until the
    /// converter is already in the path. It is a property of the driver, set
    /// when the driver is registered.
    fn migration_020_bon_driver_stream_format(&self) -> Result<()> {
        self.add_column_if_not_exists("bon_drivers", "stream_format", "TEXT DEFAULT 'ts'")
    }

    /// Stream format the driver delivers. Unknown drivers and unrecognised
    /// values answer `Ts`: assuming TS keeps existing setups working, while
    /// wrongly assuming MMT/TLV would insert a converter into a stream that
    /// does not need one.
    pub fn driver_stream_format(&self, dll_path: &str) -> StreamFormat {
        self.conn
            .query_row(
                "SELECT COALESCE(stream_format, 'ts') FROM bon_drivers WHERE dll_path = ?1",
                rusqlite::params![dll_path],
                |row| row.get::<_, String>(0),
            )
            .map(|s| StreamFormat::from_db_value(&s))
            .unwrap_or(StreamFormat::Ts)
    }

    /// Set the stream format for a driver.
    pub fn set_driver_stream_format(&self, dll_path: &str, format: StreamFormat) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET stream_format = ?2, updated_at = strftime('%s','now')
             WHERE dll_path = ?1",
            rusqlite::params![dll_path, format.as_db_value()],
        )?;
        Ok(())
    }

    /// Migration 019: per-driver "this source is already descrambled" flag.
    ///
    /// A BonDriver that hands back an already-descrambled stream must not be
    /// run through libaribb25. The case that forced this is 4K: a converter
    /// such as `BonDriver_dantto4k.dll` descrambles ACAS itself and remuxes
    /// MMT/TLV to TS, but leaves the CA descriptor in the PMT — and it
    /// advertises `CA_system_id` 0x0005, the exact id our B-CAS shim reports
    /// (`b25-sys/src/bindings/ffi.rs`), so libaribb25 latches onto the
    /// declared ECM PID even though no ECM packets ever arrive.
    ///
    /// `0` = decide automatically (4K is switched off by band), `1` = never
    /// run B25 on this driver. The manual setting exists for descrambled
    /// sources that are not 4K, where nothing in the stream gives the band
    /// away.
    fn migration_019_bon_driver_disable_b25(&self) -> Result<()> {
        self.add_column_if_not_exists("bon_drivers", "disable_b25", "INTEGER DEFAULT 0")
    }

    /// Whether this driver is configured to never run B25.
    ///
    /// Unknown drivers and read errors answer `false`: refusing to descramble
    /// when we should have is a blank screen, while descrambling when we did
    /// not need to is (with the ECM PID dead) just wasted work.
    pub fn driver_disables_b25(&self, dll_path: &str) -> bool {
        self.conn
            .query_row(
                "SELECT COALESCE(disable_b25, 0) FROM bon_drivers WHERE dll_path = ?1",
                rusqlite::params![dll_path],
                |row| row.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    /// Band of the channel a driver's (space, channel) pair tunes, as recorded
    /// by the last scan. `None` when it has not been scanned yet.
    pub fn band_type_for_bon_channel(
        &self,
        dll_path: &str,
        space: u32,
        channel: u32,
    ) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT c.band_type
                 FROM channels c
                 JOIN bon_drivers d ON d.id = c.bon_driver_id
                 WHERE d.dll_path = ?1 AND c.bon_space = ?2 AND c.bon_channel = ?3
                   AND c.band_type IS NOT NULL
                 LIMIT 1",
                rusqlite::params![dll_path, space, channel],
                |row| row.get::<_, i64>(0),
            )
            .ok()
    }

    /// Set the per-driver B25 override.
    pub fn set_driver_disable_b25(&self, dll_path: &str, disable: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET disable_b25 = ?2, updated_at = strftime('%s','now')
             WHERE dll_path = ?1",
            rusqlite::params![dll_path, if disable { 1 } else { 0 }],
        )?;
        Ok(())
    }

    /// Migration 018: per-class TS write queue durations
    /// (STREAMING_DESIGN.md §3.2).
    ///
    /// The queue used to be bounded by a frame count, which meant the amount
    /// of slack depended on how large the upstream driver's chunks happened to
    /// be. These express it as a duration instead; the byte budget is derived
    /// at runtime from the measured bitrate.
    ///
    /// Defaults are chosen to be no tighter than the previous 256-frame
    /// behaviour on a typical stream, and RECORD gets the most because it is
    /// the class that must not drop.
    fn migration_018_ts_queue_duration_columns(&self) -> Result<()> {
        self.add_column_if_not_exists("tuner_config", "ts_queue_view_ms", "INTEGER DEFAULT 8000")?;
        self.add_column_if_not_exists("tuner_config", "ts_queue_preview_ms", "INTEGER DEFAULT 12000")?;
        self.add_column_if_not_exists("tuner_config", "ts_queue_record_ms", "INTEGER DEFAULT 15000")?;
        Ok(())
    }

    /// Per-class TS write queue durations, in milliseconds.
    ///
    /// Falls back to the migration defaults when the row or the columns are
    /// missing, so a caller never has to deal with a partially-migrated DB.
    pub fn get_ts_queue_config(&self) -> Result<(u64, u64, u64)> {
        let mut stmt = self.conn.prepare(
            "SELECT ts_queue_view_ms, ts_queue_preview_ms, ts_queue_record_ms
             FROM tuner_config WHERE id = 1",
        )?;

        match stmt.query_row([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
            ))
        }) {
            Ok(config) => Ok(config),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((8000, 12000, 15000)),
            Err(e) => Err(e.into()),
        }
    }

    /// Update the per-class TS write queue durations.
    ///
    /// Kept separate from `update_tuner_config` so the already unwieldy
    /// positional tuple there does not grow further.
    pub fn update_ts_queue_config(
        &self,
        view_ms: u64,
        preview_ms: u64,
        record_ms: u64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tuner_config
             SET ts_queue_view_ms = ?1, ts_queue_preview_ms = ?2, ts_queue_record_ms = ?3
             WHERE id = 1",
            rusqlite::params![view_ms, preview_ms, record_ms],
        )?;
        Ok(())
    }

    /// Migration 017: which PC/SC card reader libaribb25 should talk to.
    ///
    /// Empty string = 未選択 (libaribb25 が全リーダーを順に試す従来動作)。
    /// B-CAS 以外のリーダー (EMV 等) が挿さっていると、その1台あたり十数秒
    /// 待たされたうえ先に応答した方が採用されてしまうため、選べるようにする。
    fn migration_017_card_reader_name(&self) -> Result<()> {
        self.add_column_if_not_exists("tuner_config", "card_reader_name", "TEXT DEFAULT ''")
    }

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
    pub fn get_tuner_livelock_config(&self) -> Result<(u64, u64, u64)> {
        self.conn.query_row(
            "SELECT min_hold_secs, reject_cooldown_ms, no_data_timeout_secs FROM tuner_config WHERE id = 1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                self.conn.execute("INSERT OR IGNORE INTO tuner_config (id, keep_alive_secs, prewarm_enabled, prewarm_timeout_secs, min_hold_secs, reject_cooldown_ms, no_data_timeout_secs) VALUES (1, 60, 1, 30, 10, 2000, 30)", [])?;
                Ok((10, 2000, 30))
            } else { Err(e) }
        }).map_err(DatabaseError::Sqlite)
    }

    pub fn update_tuner_livelock_config(&self, min_hold_secs: u64, reject_cooldown_ms: u64, no_data_timeout_secs: u64) -> Result<()> {
        self.conn.execute("UPDATE tuner_config SET min_hold_secs=?1, reject_cooldown_ms=?2, no_data_timeout_secs=?3, updated_at=strftime('%s','now') WHERE id=1", rusqlite::params![min_hold_secs, reject_cooldown_ms, no_data_timeout_secs])?;
        Ok(())
    }
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

    /// 選択中の PC/SC カードリーダー名。空文字列 = 未選択。
    ///
    /// 未選択のとき libaribb25 は見つかったリーダーへ順に接続を試み、最初に
    /// 応答したものを使う。B-CAS 以外のリーダーが挿さっていると 1 台あたり
    /// 十数秒待たされ、しかも間違った方が採用されうる (macOS 実機で確認)。
    fn ensure_card_reader_column(&self) -> Result<()> {
        self.add_column_if_not_exists("tuner_config", "card_reader_name", "TEXT DEFAULT ''")?;
        // tuner_config の1行目は `get_tuner_config` が初回アクセス時に既定値で
        // 作る。それより先にここへ来ると UPDATE が0行に当たって黙って捨てられる
        // ため、行の存在を先に確かめる。
        self.get_tuner_config()?;
        Ok(())
    }

    pub fn get_card_reader_name(&self) -> Result<String> {
        self.ensure_card_reader_column()?;
        let mut stmt = self
            .conn
            .prepare("SELECT COALESCE(card_reader_name, '') FROM tuner_config WHERE id = 1")?;
        // tuner_config は1行固定だが、初期化直後などで行が無い場合に
        // エラーにせず「未選択」として扱う。
        let name = stmt
            .query_row([], |row| row.get::<_, String>(0))
            .unwrap_or_default();
        Ok(name)
    }

    pub fn set_card_reader_name(&self, name: &str) -> Result<()> {
        self.ensure_card_reader_column()?;
        self.conn.execute(
            "UPDATE tuner_config SET card_reader_name = ?1 WHERE id = 1",
            rusqlite::params![name],
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

    /// The GitHub token is what makes CI-artifact updates possible at all, so
    /// it must survive a round trip and be clearable.
    #[test]
    fn github_token_roundtrip_and_clear() {
        let db = Database::open_in_memory().unwrap();
        db.migration_021_github_token().unwrap();
        db.migration_021_github_token().unwrap();

        assert_eq!(db.get_github_token().unwrap(), None);

        db.set_github_token("ghp_example").unwrap();
        assert_eq!(db.get_github_token().unwrap(), Some("ghp_example".to_string()));

        // Whitespace-only is the same as unset, so a cleared field does not
        // turn into an Authorization header of spaces.
        db.set_github_token("   ").unwrap();
        assert_eq!(db.get_github_token().unwrap(), None);

        // Storing does not disturb the Web API token in the same row.
        db.set_web_auth_token("web-token").unwrap();
        db.set_github_token("ghp_second").unwrap();
        assert_eq!(db.get_web_auth_token().unwrap(), Some("web-token".to_string()));
        assert_eq!(db.get_github_token().unwrap(), Some("ghp_second".to_string()));
    }

    /// `get_log_config` on a freshly-opened DB must seed and return the
    /// documented defaults (`info` / 7 days) — same seed-on-read pattern as
    /// `get_tuner_config`.
    #[test]
    fn log_config_defaults_are_seeded() {
        let db = Database::open_in_memory().unwrap();
        let (level, retention_days) = db.get_log_config().unwrap();
        assert_eq!(level, "info");
        assert_eq!(retention_days, 7);
    }

    /// A saved level/retention must round-trip exactly, and migration 022
    /// must be idempotent (replaying it must not clobber an already-updated
    /// row back to defaults).
    #[test]
    fn log_config_roundtrips_and_migration_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        db.update_log_config("debug", 14).unwrap();

        // Re-running the migration body (as would happen on a pre-ledger DB
        // replaying from user_version=0) must not reset the row.
        db.migration_022_log_config().unwrap();

        let (level, retention_days) = db.get_log_config().unwrap();
        assert_eq!(level, "debug");
        assert_eq!(retention_days, 14);
    }

    /// Migration 020 must be idempotent and default to TS, so registering a
    /// driver keeps meaning "MPEG-2 TS" unless it is explicitly said otherwise.
    #[test]
    fn migration_020_adds_stream_format_idempotently() {
        let db = Database::open_in_memory().unwrap();
        db.migration_020_bon_driver_stream_format().unwrap();
        db.migration_020_bon_driver_stream_format().unwrap();

        let path = "BonDriver_BDA_CATV4K_1.dll";
        db.get_or_create_bon_driver(path).unwrap();
        assert_eq!(db.driver_stream_format(path), StreamFormat::Ts);

        db.set_driver_stream_format(path, StreamFormat::MmtTlv).unwrap();
        assert_eq!(db.driver_stream_format(path), StreamFormat::MmtTlv);
        assert!(db.driver_stream_format(path).is_mmt_tlv());

        db.set_driver_stream_format(path, StreamFormat::Ts).unwrap();
        assert_eq!(db.driver_stream_format(path), StreamFormat::Ts);
    }

    /// An unknown driver must read as TS. Guessing MMT/TLV would put a
    /// converter in front of a stream that does not need one.
    #[test]
    fn unknown_driver_reads_as_ts() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(
            db.driver_stream_format("BonDriver_NotRegistered.dll"),
            StreamFormat::Ts
        );
    }

    /// A typo in the stored value must not turn a TS driver into a 4K one.
    #[test]
    fn an_unrecognised_stream_format_falls_back_to_ts() {
        assert_eq!(StreamFormat::from_db_value("ts"), StreamFormat::Ts);
        assert_eq!(StreamFormat::from_db_value("mmttlv"), StreamFormat::MmtTlv);
        assert_eq!(StreamFormat::from_db_value("MMTTLV"), StreamFormat::MmtTlv);
        assert_eq!(StreamFormat::from_db_value(" mmt/tlv "), StreamFormat::MmtTlv);
        assert_eq!(StreamFormat::from_db_value("mmt"), StreamFormat::Ts);
        assert_eq!(StreamFormat::from_db_value(""), StreamFormat::Ts);
    }

    /// Migration 019 must be idempotent (the ledger replays from
    /// user_version=0 on pre-ledger databases) and default to "decide
    /// automatically".
    #[test]
    fn migration_019_adds_disable_b25_idempotently() {
        let db = Database::open_in_memory().unwrap();
        db.migration_019_bon_driver_disable_b25().unwrap();
        db.migration_019_bon_driver_disable_b25().unwrap();

        let path = "BonDriver_dantto4k.dll";
        db.get_or_create_bon_driver(path).unwrap();
        assert!(!db.driver_disables_b25(path), "default is automatic");

        db.set_driver_disable_b25(path, true).unwrap();
        assert!(db.driver_disables_b25(path));
        db.set_driver_disable_b25(path, false).unwrap();
        assert!(!db.driver_disables_b25(path));
    }

    /// An unknown driver must not be reported as "B25 disabled": failing to
    /// descramble is a black screen, while descrambling needlessly is cheap.
    #[test]
    fn unknown_driver_does_not_disable_b25() {
        let db = Database::open_in_memory().unwrap();
        assert!(!db.driver_disables_b25("BonDriver_NotRegistered.dll"));
    }

    /// The band lookup is what switches B25 off for 4K automatically, so it
    /// has to find the row by the driver's own (space, channel) pair.
    #[test]
    fn band_lookup_finds_the_scanned_band_for_a_bon_channel() {
        let db = Database::open_in_memory().unwrap();
        let path = "BonDriver_dantto4k.dll";
        let driver_id = db.get_or_create_bon_driver(path).unwrap();

        // Real capture: BS朝日4K.
        db.connection()
            .execute(
                "INSERT INTO channels
                   (bon_driver_id, nid, sid, tsid, bon_space, bon_channel, band_type)
                 VALUES (?1, 0x000B, 0x97, 0xB070, 3, 0, ?2)",
                rusqlite::params![driver_id, recisdb_protocol::BandType::FourK as i64],
            )
            .unwrap();

        assert_eq!(
            db.band_type_for_bon_channel(path, 3, 0),
            Some(recisdb_protocol::BandType::FourK as i64)
        );
        // Not scanned yet.
        assert_eq!(db.band_type_for_bon_channel(path, 9, 9), None);
        assert_eq!(db.band_type_for_bon_channel("BonDriver_Other.dll", 3, 0), None);
    }

    /// Migration 018 must be idempotent (the whole ledger replays from
    /// user_version=0 on pre-ledger databases) and must leave a usable config
    /// behind for both fresh and pre-existing rows.
    #[test]
    fn migration_018_adds_ts_queue_columns_idempotently() {
        let db = Database::open_in_memory().unwrap();

        // Already applied once by `apply_migrations` during open; running it
        // again must not error on the duplicate columns.
        db.migration_018_ts_queue_duration_columns().unwrap();
        db.migration_018_ts_queue_duration_columns().unwrap();

        let (view_ms, preview_ms, record_ms) = db.get_ts_queue_config().unwrap();
        assert_eq!((view_ms, preview_ms, record_ms), (8000, 12000, 15000));
    }

    /// The durations are what operators tune for a slow link, so they must
    /// survive a write and come back out of `get_ts_queue_config`.
    #[test]
    fn ts_queue_durations_are_configurable() {
        let db = Database::open_in_memory().unwrap();
        // Ensure row 1 exists (get_tuner_config seeds it when missing).
        let _ = db.get_tuner_config().unwrap();

        db.connection()
            .execute(
                "UPDATE tuner_config SET ts_queue_view_ms = 3000,
                        ts_queue_preview_ms = 4000, ts_queue_record_ms = 30000
                 WHERE id = 1",
                [],
            )
            .unwrap();

        assert_eq!(db.get_ts_queue_config().unwrap(), (3000, 4000, 30000));
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

#[cfg(test)]
mod card_reader_tests {
    use super::Database;

    #[test]
    fn card_reader_name_defaults_to_unselected_and_roundtrips() {
        let db = Database::open_in_memory().unwrap();

        // 既定は「自動」。ここが空でないと、まだ何も選んでいない利用者の環境で
        // 存在しないリーダーを名指ししてしまう。
        assert_eq!(db.get_card_reader_name().unwrap(), "");

        db.set_card_reader_name("SCM Microsystems Inc. SCR3310").unwrap();
        assert_eq!(db.get_card_reader_name().unwrap(), "SCM Microsystems Inc. SCR3310");

        // 空文字列で「自動」に戻せること。
        db.set_card_reader_name("").unwrap();
        assert_eq!(db.get_card_reader_name().unwrap(), "");
    }
}
