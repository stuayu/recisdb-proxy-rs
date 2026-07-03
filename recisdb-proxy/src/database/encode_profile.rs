//! Encode profile CRUD (STREAMING_DESIGN.md §5.3, §9 P5).
//!
//! `encode_profiles` replaces the old single-row `tsreplace_config` mindset
//! with a small catalogue: each row supplies codec/bitrate/extra-argument
//! choices for a `purpose` ('record' / 'preview' / 'view'). `command_path`
//! deliberately stays out of this table and out of every request type in
//! this module — it remains governed solely by `tsreplace_config.command_path`
//! (TOML-only, REVIEW S1); see `Database::set_tsreplace_command_path`.

use super::{Database, EncodeProfileRecord, Result};
use rusqlite::{params, OptionalExtension};

impl Database {
    fn row_to_encode_profile_record(row: &rusqlite::Row) -> rusqlite::Result<EncodeProfileRecord> {
        Ok(EncodeProfileRecord {
            id: row.get("id")?,
            name: row.get("name")?,
            purpose: row.get("purpose")?,
            codec: row.get("codec")?,
            container: row.get::<_, Option<String>>("container")?.unwrap_or_else(|| "mpegts".to_string()),
            target_bitrate: row.get("target_bitrate")?,
            extra_args: row.get("extra_args")?,
            is_enabled: row.get::<_, i64>("is_enabled")? != 0,
            created_at: row.get("created_at")?,
        })
    }

    /// All encode profiles, ordered by purpose then id.
    pub fn get_all_encode_profiles(&self) -> Result<Vec<EncodeProfileRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM encode_profiles ORDER BY purpose, id")?;
        let rows = stmt
            .query_map([], Self::row_to_encode_profile_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get a single encode profile by id.
    pub fn get_encode_profile(&self, id: i64) -> Result<Option<EncodeProfileRecord>> {
        let mut stmt = self.conn.prepare("SELECT * FROM encode_profiles WHERE id = ?1")?;
        Ok(stmt
            .query_row([id], Self::row_to_encode_profile_record)
            .optional()?)
    }

    /// First *enabled* profile for `purpose`, ordered by id ascending (so the
    /// seeded default profile wins unless an admin inserts one with a lower
    /// id — in practice the seed always has the lowest id for its purpose).
    ///
    /// Used by the HTTP preview streaming endpoint (STREAMING_DESIGN.md §6.3)
    /// to pick the encode profile for `?profile=preview`.
    pub fn get_encode_profile_by_purpose(&self, purpose: &str) -> Result<Option<EncodeProfileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM encode_profiles WHERE purpose = ?1 AND is_enabled = 1 ORDER BY id ASC LIMIT 1",
        )?;
        Ok(stmt
            .query_row(params![purpose], Self::row_to_encode_profile_record)
            .optional()?)
    }

    /// Insert a new encode profile. Returns the new row id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_encode_profile(
        &self,
        name: &str,
        purpose: &str,
        codec: &str,
        container: &str,
        target_bitrate: Option<i64>,
        extra_args: Option<&str>,
        is_enabled: bool,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO encode_profiles (name, purpose, codec, container, target_bitrate, extra_args, is_enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                purpose,
                codec,
                container,
                target_bitrate,
                extra_args,
                is_enabled as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Partially update an encode profile. Only `Some(..)` fields are
    /// changed; `target_bitrate`/`extra_args` use the `Option<Option<..>>`
    /// convention from `update_channel_full` (outer `None` = leave alone,
    /// `Some(None)` = clear to NULL, `Some(Some(x))` = set).
    #[allow(clippy::too_many_arguments)]
    pub fn update_encode_profile(
        &self,
        id: i64,
        name: Option<&str>,
        purpose: Option<&str>,
        codec: Option<&str>,
        container: Option<&str>,
        target_bitrate: Option<Option<i64>>,
        extra_args: Option<Option<&str>>,
        is_enabled: Option<bool>,
    ) -> Result<()> {
        let mut updates: Vec<&str> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(v) = name {
            updates.push("name = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = purpose {
            updates.push("purpose = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = codec {
            updates.push("codec = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = container {
            updates.push("container = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = target_bitrate {
            updates.push("target_bitrate = ?");
            values.push(Box::new(v));
        }
        if let Some(v) = extra_args {
            updates.push("extra_args = ?");
            values.push(Box::new(v.map(|s| s.to_string())));
        }
        if let Some(v) = is_enabled {
            updates.push("is_enabled = ?");
            values.push(Box::new(v as i64));
        }

        if updates.is_empty() {
            return Ok(());
        }

        values.push(Box::new(id));
        let sql = format!("UPDATE encode_profiles SET {} WHERE id = ?", updates.join(", "));
        let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&sql, params.as_slice())?;
        Ok(())
    }

    /// Delete an encode profile.
    pub fn delete_encode_profile(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM encode_profiles WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Seed the default `preview-h264` profile if no row with that name
    /// exists yet. Idempotent — safe to call on every startup.
    ///
    /// STREAMING_DESIGN.md §6.2/§12-2: preview is H.264 fixed (compatibility
    /// over quality — MSE/mpegts.js in mainstream browsers requires H.264 or
    /// HEVC-with-limited-support; H.264 is the safe default). The bitrate and
    /// `extra_args` below are a conservative QSVEncC template mirroring the
    /// existing `tsreplace_config` default (same command shape, different
    /// codec/bitrate); sites without QSVEncC will need to edit this via the
    /// dashboard/API before `?profile=preview` produces playable output.
    pub(crate) fn seed_default_encode_profiles(&self) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM encode_profiles WHERE name = ?1)",
            params!["preview-h264"],
            |row| row.get(0),
        )?;
        if !exists {
            self.insert_encode_profile(
                "preview-h264",
                "preview",
                "h264",
                "mpegts",
                Some(2_000_000),
                Some(
                    "-i - -o - --preserve-other-services -e QSVEncC64.exe -i - \
                     --input-format mpegts --tff --vpp-deinterlace normal \
                     -c h264 --vbr 2000 --max-bitrate 3000 --gop-len 60 \
                     --output-format mpegts -o -",
                ),
                true,
            )?;
            log::info!("Seeded default encode profile 'preview-h264' (H.264, ~2Mbps, purpose=preview)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::database::Database;

    #[test]
    fn seed_creates_default_preview_profile() {
        let db = Database::open_in_memory().unwrap();
        let profile = db
            .get_encode_profile_by_purpose("preview")
            .unwrap()
            .expect("seeded preview profile should exist");
        assert_eq!(profile.name, "preview-h264");
        assert_eq!(profile.codec, "h264");
        assert_eq!(profile.purpose, "preview");
        assert!(profile.is_enabled);
    }

    #[test]
    fn seed_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        db.seed_default_encode_profiles().unwrap(); // called again on top of open()'s own seed
        let all = db.get_all_encode_profiles().unwrap();
        assert_eq!(all.iter().filter(|p| p.name == "preview-h264").count(), 1);
    }

    #[test]
    fn crud_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .insert_encode_profile("record-hevc", "record", "hevc", "mpegts", Some(8_000_000), Some("--foo"), true)
            .unwrap();

        let fetched = db.get_encode_profile(id).unwrap().expect("just inserted");
        assert_eq!(fetched.name, "record-hevc");
        assert_eq!(fetched.target_bitrate, Some(8_000_000));

        db.update_encode_profile(id, Some("record-hevc-v2"), None, None, None, Some(Some(10_000_000)), None, Some(false))
            .unwrap();
        let updated = db.get_encode_profile(id).unwrap().unwrap();
        assert_eq!(updated.name, "record-hevc-v2");
        assert_eq!(updated.target_bitrate, Some(10_000_000));
        assert!(!updated.is_enabled);

        db.delete_encode_profile(id).unwrap();
        assert!(db.get_encode_profile(id).unwrap().is_none());
    }

    #[test]
    fn get_by_purpose_ignores_disabled_rows() {
        let db = Database::open_in_memory().unwrap();
        // Disable the seeded default and insert a second, disabled-by-default
        // candidate — get_by_purpose must return None, not the disabled row.
        let seeded = db.get_encode_profile_by_purpose("preview").unwrap().unwrap();
        db.update_encode_profile(seeded.id, None, None, None, None, None, None, Some(false))
            .unwrap();
        assert!(db.get_encode_profile_by_purpose("preview").unwrap().is_none());
    }
}
