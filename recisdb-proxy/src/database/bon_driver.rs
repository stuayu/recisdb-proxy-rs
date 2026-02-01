//! BonDriver CRUD operations.

use super::{BonDriverRecord, Database, NewBonDriver, Result};
use rusqlite::params;

impl Database {
    /// Get or create a BonDriver record by DLL path.
    pub fn get_or_create_bon_driver(&self, dll_path: &str) -> Result<i64> {
        // Try to get existing
        if let Some(record) = self.get_bon_driver_by_path(dll_path)? {
            return Ok(record.id);
        }

        // Create new
        self.insert_bon_driver(&NewBonDriver::new(dll_path))
    }

    /// Insert a new BonDriver record.
    pub fn insert_bon_driver(&self, driver: &NewBonDriver) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO bon_drivers (dll_path, driver_name, version, max_instances) VALUES (?1, ?2, ?3, ?4)",
            params![driver.dll_path, driver.driver_name, driver.version, driver.max_instances.unwrap_or(1)],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get BonDriver by ID.
    pub fn get_bon_driver(&self, id: i64) -> Result<Option<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, group_name, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    max_instances, created_at, updated_at
             FROM bon_drivers WHERE id = ?1",
        )?;

        let result = stmt.query_row([id], |row| {
            Ok(BonDriverRecord {
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
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get BonDriver by display name.
    pub fn get_bon_driver_by_display_name(&self, display_name: &str) -> Result<Option<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, group_name, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    max_instances, created_at, updated_at
             FROM bon_drivers WHERE driver_name = ?1",
        )?;

        let result = stmt.query_row([display_name], |row| {
            Ok(BonDriverRecord {
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
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get BonDriver by DLL path.
    pub fn get_bon_driver_by_path(&self, dll_path: &str) -> Result<Option<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, group_name, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    max_instances, created_at, updated_at
             FROM bon_drivers WHERE dll_path = ?1",
        )?;

        let result = stmt.query_row([dll_path], |row| {
            Ok(BonDriverRecord {
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
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get max instances for a BonDriver by path.
    pub fn get_max_instances_for_path(&self, dll_path: &str) -> Result<i32> {
        let mut stmt = self.conn.prepare(
            "SELECT max_instances FROM bon_drivers WHERE dll_path = ?1",
        )?;

        let result = stmt.query_row([dll_path], |row| row.get(0));

        match result {
            Ok(max_instances) => Ok(max_instances),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(1), // Default to 1 if not found
            Err(e) => Err(e.into()),
        }
    }

    /// Get all BonDrivers.
    pub fn get_all_bon_drivers(&self) -> Result<Vec<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, group_name, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    max_instances, created_at, updated_at
             FROM bon_drivers ORDER BY scan_priority DESC, dll_path ASC",
        )?;

        let records = stmt
            .query_map([], |row| {
                Ok(BonDriverRecord {
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get BonDrivers that are due for scanning.
    pub fn get_due_bon_drivers(&self) -> Result<Vec<BonDriverRecord>> {
        let now = chrono::Utc::now().timestamp();

        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, group_name, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    max_instances, created_at, updated_at
             FROM bon_drivers
             WHERE auto_scan_enabled = 1
               AND scan_interval_hours > 0
               AND (next_scan_at IS NULL OR next_scan_at <= ?1)
             ORDER BY scan_priority DESC, next_scan_at ASC",
        )?;

        let records = stmt
            .query_map([now], |row| {
                Ok(BonDriverRecord {
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Update scan configuration for a BonDriver.
    pub fn update_scan_config(
        &self,
        id: i64,
        auto_scan_enabled: Option<bool>,
        scan_interval_hours: Option<i32>,
        scan_priority: Option<i32>,
        passive_scan_enabled: Option<bool>,
    ) -> Result<()> {
        let mut updates = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(v) = auto_scan_enabled {
            updates.push("auto_scan_enabled = ?");
            values.push(Box::new(v as i32));
        }
        if let Some(v) = scan_interval_hours {
            updates.push("scan_interval_hours = ?");
            values.push(Box::new(v));
        }
        if let Some(v) = scan_priority {
            updates.push("scan_priority = ?");
            values.push(Box::new(v));
        }
        if let Some(v) = passive_scan_enabled {
            updates.push("passive_scan_enabled = ?");
            values.push(Box::new(v as i32));
        }

        if updates.is_empty() {
            return Ok(());
        }

        values.push(Box::new(id));
        let sql = format!(
            "UPDATE bon_drivers SET {} WHERE id = ?",
            updates.join(", ")
        );

        let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&sql, params.as_slice())?;

        Ok(())
    }

    /// Update next scan time after a successful scan.
    pub fn update_next_scan(&self, id: i64, next_scan_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET next_scan_at = ?1, last_scan = strftime('%s', 'now') WHERE id = ?2",
            params![next_scan_at, id],
        )?;
        Ok(())
    }

    /// Enable scanning for a BonDriver and schedule immediate scan.
    /// This sets auto_scan_enabled = 1, scan_interval_hours = 24, and next_scan_at = 0.
    pub fn enable_immediate_scan(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET auto_scan_enabled = 1, scan_interval_hours = 24, next_scan_at = 0 WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Delete a BonDriver (cascades to channels and scan history).
    pub fn delete_bon_driver(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM bon_drivers WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Update max instances for a BonDriver.
    pub fn update_max_instances(&self, id: i64, max_instances: i32) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET max_instances = ?1 WHERE id = ?2",
            params![max_instances, id],
        )?;
        Ok(())
    }

    /// Update max instances for a BonDriver by ID.
    pub fn update_bon_driver_max_instances(&self, id: i64, max_instances: i32) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET max_instances = ?1 WHERE id = ?2",
            params![max_instances, id],
        )?;
        Ok(())
    }

    /// Update display name for a BonDriver by ID.
    pub fn update_bon_driver_display_name(&self, id: i64, display_name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET driver_name = ?1 WHERE id = ?2",
            params![display_name, id],
        )?;
        Ok(())
    }

    /// Get all BonDrivers in a group by group_name.
    pub fn get_group_drivers(&self, group_name: &str) -> Result<Vec<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, group_name, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    max_instances, created_at, updated_at
             FROM bon_drivers WHERE group_name = ?1 ORDER BY dll_path",
        )?;

        let records = stmt
            .query_map([group_name], |row| {
                Ok(BonDriverRecord {
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Set group_name for a BonDriver by ID.
    pub fn set_group_name(&self, id: i64, group_name: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE bon_drivers SET group_name = ?1, updated_at = strftime('%s', 'now') WHERE id = ?2",
            params![group_name, id],
        )?;
        Ok(())
    }

    /// Infer group_name from DLL filename.
    /// Examples:
    ///   "BonDriver_MLT1.dll" -> "PX-MLT"
    ///   "BonDriver_MLT2.dll" -> "PX-MLT"
    ///   "BonDriver_PX-Q1UD.dll" -> "PX-Q1UD"
    ///   "BonDriver_PX4-S.dll" -> "PX4-S"
    pub fn infer_group_name(dll_path: &str) -> Option<String> {
        let filename = std::path::Path::new(dll_path)
            .file_stem()
            .and_then(|s| s.to_str())?;

        // Remove "BonDriver_" prefix
        let name = filename.strip_prefix("BonDriver_")?;

        // Group by model (remove version number if present)
        // MLT1, MLT2, MLT3 -> PX-MLT
        // PX-Q1UD -> PX-Q1UD
        // PX4-S -> PX4-S
        let group = if let Some(model) = name.chars().position(|c| c.is_numeric()) {
            let base = &name[..model];
            format!("PX-{}", base)
        } else {
            // Already in full form (e.g., "PX-Q1UD", "PX4-S")
            if name.contains("PX") {
                name.to_string()
            } else {
                format!("PX-{}", name)
            }
        };

        Some(group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bon_driver_crud() {
        let db = Database::open_in_memory().unwrap();

        // Insert
        let id = db
            .insert_bon_driver(&NewBonDriver::new("BonDriver_Test.dll").with_name("Test Driver"))
            .unwrap();
        assert!(id > 0);

        // Get by ID
        let record = db.get_bon_driver(id).unwrap().unwrap();
        assert_eq!(record.dll_path, "BonDriver_Test.dll");
        assert_eq!(record.driver_name, Some("Test Driver".to_string()));
        assert!(record.auto_scan_enabled);

        // Get by path
        let record2 = db
            .get_bon_driver_by_path("BonDriver_Test.dll")
            .unwrap()
            .unwrap();
        assert_eq!(record2.id, id);

        // Get or create (existing)
        let id2 = db.get_or_create_bon_driver("BonDriver_Test.dll").unwrap();
        assert_eq!(id, id2);

        // Get or create (new)
        let id3 = db.get_or_create_bon_driver("BonDriver_New.dll").unwrap();
        assert_ne!(id, id3);

        // Update config
        db.update_scan_config(id, Some(false), Some(48), None, None)
            .unwrap();
        let updated = db.get_bon_driver(id).unwrap().unwrap();
        assert!(!updated.auto_scan_enabled);
        assert_eq!(updated.scan_interval_hours, 48);

        // Delete
        db.delete_bon_driver(id).unwrap();
        assert!(db.get_bon_driver(id).unwrap().is_none());
    }

    #[test]
    fn test_get_all_bon_drivers() {
        let db = Database::open_in_memory().unwrap();

        db.insert_bon_driver(&NewBonDriver::new("Driver1.dll"))
            .unwrap();
        db.insert_bon_driver(&NewBonDriver::new("Driver2.dll"))
            .unwrap();

        let all = db.get_all_bon_drivers().unwrap();
        assert_eq!(all.len(), 2);
    }
}
