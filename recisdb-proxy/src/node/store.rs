//! Persistent node/fabric configuration in the existing SQLite database.
//!
//! Tables are created idempotently when the fabric is enabled. This keeps the
//! first implementation isolated from the historical migration ledger while
//! still using the same database file and transaction semantics.

use crate::database::{Database, DatabaseError};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::identity::{NodeCredential, NodeIdentity};
use super::types::{NodeEndpoint, NodeId};

pub type Result<T> = std::result::Result<T, DatabaseError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredNode {
    pub node_id: NodeId,
    pub display_name: String,
    pub site_name: Option<String>,
    pub enabled: bool,
    pub allow_transit: bool,
    pub auto_connect: bool,
    pub last_seen_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteGroup {
    pub id: i64,
    pub name: String,
}

pub struct NodeStore<'a> {
    db: &'a Database,
}

impl<'a> NodeStore<'a> {
    pub fn new(db: &'a Database) -> Result<Self> {
        let store = Self { db };
        store.ensure_schema()?;
        Ok(store)
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.db.connection().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS node_local_identity (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                node_id TEXT NOT NULL,
                display_name TEXT NOT NULL,
                node_listen_addr TEXT,
                auto_discovery INTEGER NOT NULL DEFAULT 1,
                updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000)
            );

            CREATE TABLE IF NOT EXISTS receive_sites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                region TEXT,
                tags_json TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000)
            );

            CREATE TABLE IF NOT EXISTS remote_nodes (
                node_id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                site_name TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                allow_transit INTEGER NOT NULL DEFAULT 0,
                auto_connect INTEGER NOT NULL DEFAULT 1,
                credential TEXT,
                last_seen_unix_ms INTEGER,
                updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000)
            );

            CREATE TABLE IF NOT EXISTS node_endpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id TEXT NOT NULL,
                endpoint_json TEXT NOT NULL,
                UNIQUE(node_id, endpoint_json),
                FOREIGN KEY(node_id) REFERENCES remote_nodes(node_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS route_groups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000)
            );

            CREATE TABLE IF NOT EXISTS route_group_members (
                group_id INTEGER NOT NULL,
                node_id TEXT NOT NULL,
                weight INTEGER NOT NULL DEFAULT 100,
                PRIMARY KEY(group_id, node_id),
                FOREIGN KEY(group_id) REFERENCES route_groups(id) ON DELETE CASCADE,
                FOREIGN KEY(node_id) REFERENCES remote_nodes(node_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS logical_muxes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                nid INTEGER NOT NULL,
                tsid INTEGER NOT NULL,
                logical_broadcast TEXT NOT NULL,
                network_name TEXT,
                UNIQUE(nid, tsid)
            );

            CREATE TABLE IF NOT EXISTS reception_routes (
                route_id TEXT PRIMARY KEY,
                mux_id INTEGER NOT NULL,
                node_id TEXT,
                local_bon_driver_id INTEGER,
                bon_space INTEGER,
                bon_channel INTEGER,
                physical_ch INTEGER,
                frequency_hz INTEGER,
                ingress_delivery TEXT NOT NULL,
                ultimate_delivery TEXT NOT NULL,
                tsmf_relative_ts INTEGER,
                routing_state TEXT NOT NULL DEFAULT 'discovered',
                configured_priority INTEGER NOT NULL DEFAULT 0,
                source_quality REAL NOT NULL DEFAULT 0.0,
                confidence REAL NOT NULL DEFAULT 0.0,
                last_seen_unix_ms INTEGER,
                last_qualified_unix_ms INTEGER,
                FOREIGN KEY(mux_id) REFERENCES logical_muxes(id) ON DELETE CASCADE,
                FOREIGN KEY(node_id) REFERENCES remote_nodes(node_id) ON DELETE CASCADE,
                FOREIGN KEY(local_bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS route_observations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                route_id TEXT NOT NULL,
                observed_at_unix_ms INTEGER NOT NULL,
                signal_raw REAL,
                signal_normalized REAL,
                tune_ms INTEGER,
                first_ts_ms INTEGER,
                sample_bytes INTEGER NOT NULL,
                bitrate_bps INTEGER NOT NULL,
                tei_rate REAL NOT NULL,
                cc_error_rate REAL NOT NULL,
                sync_error_rate REAL NOT NULL,
                scramble_rate REAL NOT NULL,
                pat_ok INTEGER NOT NULL,
                sdt_ok INTEGER NOT NULL,
                nit_ok INTEGER NOT NULL,
                qualification_result TEXT NOT NULL,
                reasons_json TEXT NOT NULL DEFAULT '[]',
                FOREIGN KEY(route_id) REFERENCES reception_routes(route_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS node_path_health (
                node_id TEXT NOT NULL,
                endpoint_id INTEGER NOT NULL,
                state TEXT NOT NULL,
                rtt_p50_ms REAL,
                rtt_p95_ms REAL,
                throughput_down_p10_bps INTEGER,
                throughput_down_ewma_bps INTEGER,
                jitter_ms REAL,
                stall_rate REAL,
                reconnect_rate REAL,
                confidence REAL,
                tailscale_path TEXT,
                measured_at_unix_ms INTEGER,
                PRIMARY KEY(node_id, endpoint_id),
                FOREIGN KEY(node_id) REFERENCES remote_nodes(node_id) ON DELETE CASCADE,
                FOREIGN KEY(endpoint_id) REFERENCES node_endpoints(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_reception_routes_mux ON reception_routes(mux_id, routing_state);
            CREATE INDEX IF NOT EXISTS idx_route_observations_route_time ON route_observations(route_id, observed_at_unix_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_node_endpoints_node ON node_endpoints(node_id);
            "#,
        )?;
        Ok(())
    }

    pub fn local_identity(&self) -> Result<NodeIdentity> {
        let existing = self
            .db
            .connection()
            .query_row(
                "SELECT node_id, display_name FROM node_local_identity WHERE id = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((node_id, display_name)) = existing {
            let node_id = NodeId::new(node_id).map_err(|e| DatabaseError::MigrationFailed(e.into()))?;
            return Ok(NodeIdentity { node_id, display_name });
        }

        let identity = NodeIdentity {
            node_id: NodeId::random(),
            display_name: "recisdb-proxy".into(),
        };
        self.db.connection().execute(
            "INSERT INTO node_local_identity (id, node_id, display_name) VALUES (1, ?1, ?2)",
            params![identity.node_id.as_str(), identity.display_name],
        )?;
        Ok(identity)
    }

    pub fn update_local_identity(&self, identity: &NodeIdentity, listen_addr: Option<&str>) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO node_local_identity (id, node_id, display_name, node_listen_addr, updated_at)
             VALUES (1, ?1, ?2, ?3, strftime('%s','now') * 1000)
             ON CONFLICT(id) DO UPDATE SET
               node_id=excluded.node_id,
               display_name=excluded.display_name,
               node_listen_addr=excluded.node_listen_addr,
               updated_at=excluded.updated_at",
            params![identity.node_id.as_str(), identity.display_name, listen_addr],
        )?;
        Ok(())
    }

    pub fn upsert_node(&self, node: &StoredNode, credential: Option<&NodeCredential>) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO remote_nodes
             (node_id, display_name, site_name, enabled, allow_transit, auto_connect, credential, last_seen_unix_ms, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%s','now') * 1000)
             ON CONFLICT(node_id) DO UPDATE SET
               display_name=excluded.display_name,
               site_name=excluded.site_name,
               enabled=excluded.enabled,
               allow_transit=excluded.allow_transit,
               auto_connect=excluded.auto_connect,
               credential=COALESCE(excluded.credential, remote_nodes.credential),
               last_seen_unix_ms=COALESCE(excluded.last_seen_unix_ms, remote_nodes.last_seen_unix_ms),
               updated_at=excluded.updated_at",
            params![
                node.node_id.as_str(),
                node.display_name,
                node.site_name,
                node.enabled as i32,
                node.allow_transit as i32,
                node.auto_connect as i32,
                credential.map(NodeCredential::expose),
                node.last_seen_unix_ms,
            ],
        )?;
        Ok(())
    }

    pub fn list_nodes(&self) -> Result<Vec<StoredNode>> {
        let mut stmt = self.db.connection().prepare(
            "SELECT node_id, display_name, site_name, enabled, allow_transit, auto_connect, last_seen_unix_ms
             FROM remote_nodes ORDER BY display_name, node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let node_id: String = row.get(0)?;
            Ok((
                node_id,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(4)? != 0,
                row.get::<_, i64>(5)? != 0,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (node_id, display_name, site_name, enabled, allow_transit, auto_connect, last_seen_unix_ms) = row?;
            let node_id = NodeId::new(node_id).map_err(|e| DatabaseError::MigrationFailed(e.into()))?;
            out.push(StoredNode {
                node_id,
                display_name,
                site_name,
                enabled,
                allow_transit,
                auto_connect,
                last_seen_unix_ms,
            });
        }
        Ok(out)
    }

    pub fn credential_for(&self, node_id: &NodeId) -> Result<Option<NodeCredential>> {
        let value: Option<String> = self
            .db
            .connection()
            .query_row(
                "SELECT credential FROM remote_nodes WHERE node_id = ?1",
                params![node_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        value
            .map(|value| NodeCredential::parse(value).map_err(|e| DatabaseError::MigrationFailed(e.into())))
            .transpose()
    }

    pub fn replace_endpoints(&self, node_id: &NodeId, endpoints: &[NodeEndpoint]) -> Result<()> {
        let conn = self.db.connection();
        conn.execute("DELETE FROM node_endpoints WHERE node_id = ?1", params![node_id.as_str()])?;
        for endpoint in endpoints {
            let json = serde_json::to_string(endpoint)
                .map_err(|e| DatabaseError::MigrationFailed(e.to_string()))?;
            conn.execute(
                "INSERT INTO node_endpoints (node_id, endpoint_json) VALUES (?1, ?2)",
                params![node_id.as_str(), json],
            )?;
        }
        Ok(())
    }

    pub fn endpoints(&self, node_id: &NodeId) -> Result<Vec<NodeEndpoint>> {
        let mut stmt = self.db.connection().prepare(
            "SELECT endpoint_json FROM node_endpoints WHERE node_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![node_id.as_str()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            let endpoint = serde_json::from_str(&json)
                .map_err(|e| DatabaseError::MigrationFailed(e.to_string()))?;
            out.push(endpoint);
        }
        Ok(out)
    }

    pub fn ensure_route_group(&self, name: &str) -> Result<i64> {
        self.db.connection().execute(
            "INSERT OR IGNORE INTO route_groups (name) VALUES (?1)",
            params![name],
        )?;
        self.db.connection().query_row(
            "SELECT id FROM route_groups WHERE name = ?1",
            params![name],
            |row| row.get(0),
        ).map_err(DatabaseError::from)
    }

    pub fn set_group_member(&self, group_id: i64, node_id: &NodeId, weight: i32) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO route_group_members (group_id, node_id, weight) VALUES (?1, ?2, ?3)
             ON CONFLICT(group_id, node_id) DO UPDATE SET weight=excluded.weight",
            params![group_id, node_id.as_str(), weight],
        )?;
        Ok(())
    }

    pub fn list_route_groups(&self) -> Result<Vec<RouteGroup>> {
        let mut stmt = self.db.connection().prepare("SELECT id, name FROM route_groups ORDER BY name")?;
        let rows = stmt.query_map([], |row| Ok(RouteGroup { id: row.get(0)?, name: row.get(1)? }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::types::EndpointKind;

    #[test]
    fn schema_and_gui_facing_config_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let local = store.local_identity().unwrap();
        assert!(!local.node_id.as_str().is_empty());

        let node = StoredNode {
            node_id: NodeId::new("gunma").unwrap(),
            display_name: "群馬".into(),
            site_name: Some("群馬".into()),
            enabled: true,
            allow_transit: false,
            auto_connect: true,
            last_seen_unix_ms: None,
        };
        let credential = NodeCredential::random();
        store.upsert_node(&node, Some(&credential)).unwrap();
        store.replace_endpoints(
            &node.node_id,
            &[NodeEndpoint {
                kind: EndpointKind::Tailscale,
                address: "http://gunma.tailnet:4512".into(),
                enabled: true,
                record_allowed: true,
                metered: false,
                user_priority: 0,
            }],
        ).unwrap();
        assert_eq!(store.list_nodes().unwrap().len(), 1);
        assert_eq!(store.endpoints(&node.node_id).unwrap().len(), 1);
        assert_eq!(store.credential_for(&node.node_id).unwrap().unwrap(), credential);
    }
}
