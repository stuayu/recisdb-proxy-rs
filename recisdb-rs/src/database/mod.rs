//! Database module for channel information storage.
//!
//! This module provides SQLite-based persistent storage for:
//! - BonDriver registration and scan configuration
//! - Channel information (NID/SID/TSID-based identification)
//! - Scan history and statistics

#[cfg(feature = "database")]
mod bon_driver;
#[cfg(feature = "database")]
mod channel;
#[cfg(feature = "database")]
mod models;
#[cfg(feature = "database")]
mod schema;

#[cfg(feature = "database")]
pub use bon_driver::*;
#[cfg(feature = "database")]
pub use channel::*;
#[cfg(feature = "database")]
pub use models::*;

#[cfg(feature = "database")]
use rusqlite::{Connection, Result as SqliteResult};
#[cfg(feature = "database")]
use std::path::Path;
#[cfg(feature = "database")]
use thiserror::Error;

/// Database error types.
#[cfg(feature = "database")]
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

#[cfg(feature = "database")]
pub type Result<T> = std::result::Result<T, DatabaseError>;

/// Main database connection wrapper.
#[cfg(feature = "database")]
pub struct Database {
    conn: Connection,
}

#[cfg(feature = "database")]
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

#[cfg(feature = "database")]
impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[cfg(feature = "database")]
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
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('bon_drivers', 'channels', 'scan_history')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 3);
    }
}
