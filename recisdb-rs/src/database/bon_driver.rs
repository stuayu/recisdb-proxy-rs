//! BonDriver CRUD operations.

use super::{BonDriverRecord, Database, DatabaseError, NewBonDriver, Result};
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
            "INSERT INTO bon_drivers (dll_path, driver_name, version) VALUES (?1, ?2, ?3)",
            params![driver.dll_path, driver.driver_name, driver.version],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get BonDriver by ID.
    pub fn get_bon_driver(&self, id: i64) -> Result<Option<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    created_at, updated_at
             FROM bon_drivers WHERE id = ?1",
        )?;

        let result = stmt.query_row([id], |row| {
            Ok(BonDriverRecord {
                id: row.get(0)?,
                dll_path: row.get(1)?,
                driver_name: row.get(2)?,
                version: row.get(3)?,
                auto_scan_enabled: row.get::<_, i32>(4)? != 0,
                scan_interval_hours: row.get(5)?,
                scan_priority: row.get(6)?,
                last_scan: row.get(7)?,
                next_scan_at: row.get(8)?,
                passive_scan_enabled: row.get::<_, i32>(9)? != 0,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
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
            "SELECT id, dll_path, driver_name, version, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    created_at, updated_at
             FROM bon_drivers WHERE dll_path = ?1",
        )?;

        let result = stmt.query_row([dll_path], |row| {
            Ok(BonDriverRecord {
                id: row.get(0)?,
                dll_path: row.get(1)?,
                driver_name: row.get(2)?,
                version: row.get(3)?,
                auto_scan_enabled: row.get::<_, i32>(4)? != 0,
                scan_interval_hours: row.get(5)?,
                scan_priority: row.get(6)?,
                last_scan: row.get(7)?,
                next_scan_at: row.get(8)?,
                passive_scan_enabled: row.get::<_, i32>(9)? != 0,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all BonDrivers.
    pub fn get_all_bon_drivers(&self) -> Result<Vec<BonDriverRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    created_at, updated_at
             FROM bon_drivers ORDER BY scan_priority DESC, dll_path ASC",
        )?;

        let records = stmt
            .query_map([], |row| {
                Ok(BonDriverRecord {
                    id: row.get(0)?,
                    dll_path: row.get(1)?,
                    driver_name: row.get(2)?,
                    version: row.get(3)?,
                    auto_scan_enabled: row.get::<_, i32>(4)? != 0,
                    scan_interval_hours: row.get(5)?,
                    scan_priority: row.get(6)?,
                    last_scan: row.get(7)?,
                    next_scan_at: row.get(8)?,
                    passive_scan_enabled: row.get::<_, i32>(9)? != 0,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get BonDrivers that are due for scanning.
    pub fn get_due_bon_drivers(&self) -> Result<Vec<BonDriverRecord>> {
        let now = chrono::Utc::now().timestamp();

        let mut stmt = self.conn.prepare(
            "SELECT id, dll_path, driver_name, version, auto_scan_enabled, scan_interval_hours,
                    scan_priority, last_scan, next_scan_at, passive_scan_enabled,
                    created_at, updated_at
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
                    auto_scan_enabled: row.get::<_, i32>(4)? != 0,
                    scan_interval_hours: row.get(5)?,
                    scan_priority: row.get(6)?,
                    last_scan: row.get(7)?,
                    next_scan_at: row.get(8)?,
                    passive_scan_enabled: row.get::<_, i32>(9)? != 0,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
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

    /// Delete a BonDriver (cascades to channels and scan history).
    pub fn delete_bon_driver(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM bon_drivers WHERE id = ?1", [id])?;
        Ok(())
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
