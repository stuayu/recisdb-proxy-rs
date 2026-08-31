//! Persistent node/fabric configuration in the existing SQLite database.
//!
//! Tables are created idempotently when the fabric is enabled. This keeps the
//! first implementation isolated from the historical migration ledger while
//! still using the same database file and transaction semantics.

use crate::database::{Database, DatabaseError};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::identity::{NodeCredential, NodeIdentity, PairingCode};
use super::types::{
    DeliveryType, LogicalBroadcastType, LogicalMuxId, NodeEndpoint, NodeId,
    ReceptionRouteAdvertisement, ReceptionRouteState,
};

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

/// One remote physical route, as last advertised by its owning node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRemoteRoute {
    pub route_id: String,
    pub node_id: NodeId,
    pub mux: LogicalMuxId,
    pub logical_broadcast: LogicalBroadcastType,
    pub ingress_delivery: DeliveryType,
    pub ultimate_delivery: DeliveryType,
    pub state: ReceptionRouteState,
    pub source_quality: f64,
    pub confidence: f64,
    pub last_seen_unix_ms: Option<i64>,
}

/// An outstanding pairing code, as far as the dashboard may know about it.
/// The code itself is never recoverable — only its digest was stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPairing {
    pub label: Option<String>,
    pub expires_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
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

            -- One-time pairing codes issued by this node. Only the SHA-256 of
            -- the normalized code is stored: the plaintext exists in the
            -- issuing HTTP response and nowhere else, so a database dump
            -- cannot be replayed to become a trusted peer.
            CREATE TABLE IF NOT EXISTS node_pending_pairings (
                code_hash TEXT PRIMARY KEY,
                label TEXT,
                expires_at_unix_ms INTEGER NOT NULL,
                created_at_unix_ms INTEGER NOT NULL
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
            let node_id =
                NodeId::new(node_id).map_err(|e| DatabaseError::MigrationFailed(e.into()))?;
            return Ok(NodeIdentity {
                node_id,
                display_name,
            });
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

    pub fn update_local_identity(
        &self,
        identity: &NodeIdentity,
        listen_addr: Option<&str>,
    ) -> Result<()> {
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

    pub fn upsert_node(
        &self,
        node: &StoredNode,
        credential: Option<&NodeCredential>,
    ) -> Result<()> {
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
            let (
                node_id,
                display_name,
                site_name,
                enabled,
                allow_transit,
                auto_connect,
                last_seen_unix_ms,
            ) = row?;
            let node_id =
                NodeId::new(node_id).map_err(|e| DatabaseError::MigrationFailed(e.into()))?;
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

    /// Enable or suspend one paired node without deleting its credentials and
    /// route history. Suspended nodes remain visible for later reactivation.
    pub fn set_node_enabled(&self, node_id: &NodeId, enabled: bool) -> Result<()> {
        self.db.connection().execute(
            "UPDATE remote_nodes SET enabled = ?2, updated_at = strftime('%s','now') * 1000
             WHERE node_id = ?1",
            params![node_id.as_str(), enabled as i32],
        )?;
        Ok(())
    }

    /// Delete a remote node and all data owned by it. Explicit child cleanup
    /// keeps this safe on databases created before foreign keys were enabled.
    pub fn delete_node(&self, node_id: &NodeId) -> Result<()> {
        let conn = self.db.connection();
        conn.execute(
            "DELETE FROM route_group_members WHERE node_id = ?1",
            params![node_id.as_str()],
        )?;
        conn.execute(
            "DELETE FROM node_path_health WHERE node_id = ?1",
            params![node_id.as_str()],
        )?;
        conn.execute(
            "DELETE FROM reception_routes WHERE node_id = ?1",
            params![node_id.as_str()],
        )?;
        conn.execute(
            "DELETE FROM node_endpoints WHERE node_id = ?1",
            params![node_id.as_str()],
        )?;
        conn.execute(
            "DELETE FROM remote_nodes WHERE node_id = ?1",
            params![node_id.as_str()],
        )?;
        Ok(())
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
            .map(|value| {
                NodeCredential::parse(value).map_err(|e| DatabaseError::MigrationFailed(e.into()))
            })
            .transpose()
    }

    /// Atomically replace one node's endpoint list.
    ///
    /// The list is de-duplicated first: `node_endpoints` has
    /// `UNIQUE(node_id, endpoint_json)`, and a UI that sends the same endpoint
    /// twice is asking for the same end state, not for an error. Letting the
    /// constraint fire here would surface as a raw SQLite 500 on save.
    pub fn replace_endpoints(&self, node_id: &NodeId, endpoints: &[NodeEndpoint]) -> Result<()> {
        let conn = self.db.connection();
        conn.execute(
            "DELETE FROM node_endpoints WHERE node_id = ?1",
            params![node_id.as_str()],
        )?;
        let mut seen = std::collections::HashSet::new();
        for endpoint in endpoints {
            let json = serde_json::to_string(endpoint)
                .map_err(|e| DatabaseError::MigrationFailed(e.to_string()))?;
            if !seen.insert(json.clone()) {
                continue;
            }
            conn.execute(
                "INSERT INTO node_endpoints (node_id, endpoint_json) VALUES (?1, ?2)",
                params![node_id.as_str(), json],
            )?;
        }
        Ok(())
    }

    pub fn endpoints(&self, node_id: &NodeId) -> Result<Vec<NodeEndpoint>> {
        let mut stmt = self
            .db
            .connection()
            .prepare("SELECT endpoint_json FROM node_endpoints WHERE node_id = ?1 ORDER BY id")?;
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
        self.db
            .connection()
            .query_row(
                "SELECT id FROM route_groups WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .map_err(DatabaseError::from)
    }

    pub fn set_group_member(&self, group_id: i64, node_id: &NodeId, weight: i32) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO route_group_members (group_id, node_id, weight) VALUES (?1, ?2, ?3)
             ON CONFLICT(group_id, node_id) DO UPDATE SET weight=excluded.weight",
            params![group_id, node_id.as_str(), weight],
        )?;
        Ok(())
    }

    /// Get-or-create the `logical_muxes` row for `mux`.
    ///
    /// The logical family is what a client asks for; how the mux physically
    /// arrived lives on the individual `reception_routes` rows. Keeping them
    /// in different tables is what stops "BS over CATV" from being recorded as
    /// something other than BS.
    pub fn upsert_logical_mux(
        &self,
        mux: LogicalMuxId,
        logical_broadcast: LogicalBroadcastType,
        network_name: Option<&str>,
    ) -> Result<i64> {
        let broadcast = serde_json::to_value(logical_broadcast)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        self.db.connection().execute(
            "INSERT INTO logical_muxes (nid, tsid, logical_broadcast, network_name)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(nid, tsid) DO UPDATE SET
                 logical_broadcast = excluded.logical_broadcast,
                 network_name = COALESCE(excluded.network_name, logical_muxes.network_name)",
            params![mux.nid as i64, mux.tsid as i64, broadcast, network_name],
        )?;
        let id = self.db.connection().query_row(
            "SELECT id FROM logical_muxes WHERE nid = ?1 AND tsid = ?2",
            params![mux.nid as i64, mux.tsid as i64],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Replace everything known about `peer`'s reception routes in one
    /// transaction.
    ///
    /// Replace rather than merge: the peer's advertisement is the complete
    /// current picture, and a route it stopped advertising must stop being a
    /// candidate here. Local routes (`node_id IS NULL`) are untouched.
    pub fn replace_remote_routes(
        &self,
        peer: &NodeId,
        advertisements: &[ReceptionRouteAdvertisement],
    ) -> Result<()> {
        let conn = self.db.connection();
        conn.execute("BEGIN IMMEDIATE", [])?;
        let result = (|| -> Result<()> {
            conn.execute(
                "DELETE FROM reception_routes WHERE node_id = ?1",
                params![peer.as_str()],
            )?;
            for advertisement in advertisements {
                let mux_id = self.upsert_logical_mux(
                    advertisement.mux,
                    advertisement.logical_broadcast,
                    None,
                )?;
                conn.execute(
                    "INSERT OR REPLACE INTO reception_routes (
                        route_id, mux_id, node_id, ingress_delivery, ultimate_delivery,
                        routing_state, source_quality, confidence, last_seen_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        // Namespaced by peer: two nodes can legitimately use
                        // the same local route id (same DLL path/tuning).
                        format!("{}::{}", peer.as_str(), advertisement.route_id),
                        mux_id,
                        peer.as_str(),
                        enum_str(advertisement.ingress_delivery),
                        enum_str(advertisement.ultimate_delivery),
                        enum_str(advertisement.state),
                        advertisement.source_quality,
                        advertisement.confidence,
                        advertisement.observed_at_unix_ms,
                    ],
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute("COMMIT", [])?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    /// Remote routes currently known for `mux`, most trustworthy first.
    ///
    /// Only routable states are returned: `Discovered`/`Quarantined`/
    /// `Disabled` routes stay in the table so they can be re-probed later
    /// rather than being deleted and rediscovered forever.
    pub fn remote_routes_for(&self, mux: LogicalMuxId) -> Result<Vec<StoredRemoteRoute>> {
        let conn = self.db.connection();
        let mut stmt = conn.prepare(
            "SELECT r.route_id, r.node_id, r.ingress_delivery, r.ultimate_delivery,
                    r.routing_state, r.source_quality, r.confidence, r.last_seen_unix_ms,
                    m.logical_broadcast
             FROM reception_routes r
             JOIN logical_muxes m ON r.mux_id = m.id
             JOIN remote_nodes n ON r.node_id = n.node_id
             WHERE m.nid = ?1 AND m.tsid = ?2 AND r.node_id IS NOT NULL AND n.enabled = 1
             ORDER BY r.configured_priority DESC, r.confidence DESC, r.source_quality DESC",
        )?;
        let rows = stmt
            .query_map(params![mux.nid as i64, mux.tsid as i64], |row| {
                let node_id: String = row.get(1)?;
                Ok(StoredRemoteRoute {
                    route_id: row.get(0)?,
                    node_id: NodeId::new(node_id).unwrap_or_else(|_| NodeId::random()),
                    mux,
                    logical_broadcast: parse_enum(&row.get::<_, String>(8)?)
                        .unwrap_or(LogicalBroadcastType::Unknown),
                    ingress_delivery: parse_enum(&row.get::<_, String>(2)?)
                        .unwrap_or(DeliveryType::Unknown),
                    ultimate_delivery: parse_enum(&row.get::<_, String>(3)?)
                        .unwrap_or(DeliveryType::Unknown),
                    state: parse_enum(&row.get::<_, String>(4)?)
                        .unwrap_or(ReceptionRouteState::Discovered),
                    source_quality: row.get(5)?,
                    confidence: row.get(6)?,
                    last_seen_unix_ms: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().filter(|r| r.state.routable()).collect())
    }

    /// How many muxes this peer currently advertises, split into routable and
    /// total. The dashboard needs both: "12 routes, 0 usable" is a very
    /// different situation from "no routes at all".
    pub fn remote_route_counts(&self, node_id: &NodeId) -> Result<(i64, i64)> {
        let conn = self.db.connection();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM reception_routes WHERE node_id = ?1",
            params![node_id.as_str()],
            |row| row.get(0),
        )?;
        let routable: i64 = conn.query_row(
            "SELECT COUNT(*) FROM reception_routes
             WHERE node_id = ?1 AND routing_state IN ('usable', 'preferred', 'degraded')",
            params![node_id.as_str()],
            |row| row.get(0),
        )?;
        Ok((routable, total))
    }

    /// Record a pairing code this node just issued. Only its digest is kept.
    pub fn create_pending_pairing(
        &self,
        code: &PairingCode,
        label: Option<&str>,
        expires_at_unix_ms: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        // Housekeeping here rather than on a timer: pairing is rare and this
        // is the only place that grows the table.
        self.db.connection().execute(
            "DELETE FROM node_pending_pairings WHERE expires_at_unix_ms <= ?1",
            params![now],
        )?;
        self.db.connection().execute(
            "INSERT OR REPLACE INTO node_pending_pairings (code_hash, label, expires_at_unix_ms, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4)",
            params![code.digest(), label, expires_at_unix_ms, now],
        )?;
        Ok(())
    }

    /// Redeem a pairing code exactly once.
    ///
    /// The `DELETE ... RETURNING`-equivalent below is a single statement, so
    /// two concurrent redemptions of the same code cannot both succeed: only
    /// the one that actually removed the row sees a non-zero row count. An
    /// expired row is removed and reported as *not* redeemable.
    pub fn consume_pending_pairing(&self, code: &PairingCode) -> Result<bool> {
        let now = chrono::Utc::now().timestamp_millis();
        let digest = code.digest();
        let redeemed = self.db.connection().execute(
            "DELETE FROM node_pending_pairings WHERE code_hash = ?1 AND expires_at_unix_ms > ?2",
            params![digest, now],
        )?;
        if redeemed == 0 {
            // Clean up a matching-but-expired row so a stale code cannot sit
            // around being probed forever.
            self.db.connection().execute(
                "DELETE FROM node_pending_pairings WHERE code_hash = ?1",
                params![digest],
            )?;
        }
        Ok(redeemed > 0)
    }

    /// Expiry timestamps of the pairing codes still outstanding. The codes
    /// themselves are unrecoverable by design, so the dashboard can only show
    /// that one is pending and until when.
    pub fn pending_pairings(&self) -> Result<Vec<PendingPairing>> {
        let conn = self.db.connection();
        let mut stmt = conn.prepare(
            "SELECT label, expires_at_unix_ms, created_at_unix_ms FROM node_pending_pairings WHERE expires_at_unix_ms > ?1 ORDER BY created_at_unix_ms DESC",
        )?;
        let rows = stmt
            .query_map(params![chrono::Utc::now().timestamp_millis()], |row| {
                Ok(PendingPairing {
                    label: row.get(0)?,
                    expires_at_unix_ms: row.get(1)?,
                    created_at_unix_ms: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_route_groups(&self) -> Result<Vec<RouteGroup>> {
        let mut stmt = self
            .db
            .connection()
            .prepare("SELECT id, name FROM route_groups ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(RouteGroup {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn route_group_members(&self, group_id: i64) -> Result<Vec<(NodeId, i32)>> {
        let mut stmt = self.db.connection().prepare(
            "SELECT node_id, weight FROM route_group_members WHERE group_id = ?1 ORDER BY node_id",
        )?;
        let rows = stmt.query_map(params![group_id], |row| {
            let id: String = row.get(0)?;
            Ok((id, row.get::<_, i32>(1)?))
        })?;
        rows.map(|row| {
            let (id, weight) = row?;
            let id = NodeId::new(id).map_err(|e| DatabaseError::MigrationFailed(e.into()))?;
            Ok((id, weight))
        })
        .collect()
    }

    /// Look up a route group by its (unique) name.
    ///
    /// Callers use this to turn "the name is taken" into a 409 with a human
    /// message instead of letting the UNIQUE constraint surface as a raw
    /// `SQLite error: UNIQUE constraint failed: route_groups.name` 500.
    pub fn route_group_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        self.db
            .connection()
            .query_row(
                "SELECT id FROM route_groups WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .map_err(DatabaseError::from)
    }

    pub fn route_group_exists(&self, group_id: i64) -> Result<bool> {
        self.db
            .connection()
            .query_row(
                "SELECT 1 FROM route_groups WHERE id = ?1",
                params![group_id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(DatabaseError::from)
    }

    pub fn rename_route_group(&self, group_id: i64, name: &str) -> Result<()> {
        self.db.connection().execute(
            "UPDATE route_groups SET name = ?2 WHERE id = ?1",
            params![group_id, name],
        )?;
        Ok(())
    }

    pub fn delete_route_group(&self, group_id: i64) -> Result<()> {
        self.db
            .connection()
            .execute("DELETE FROM route_groups WHERE id = ?1", params![group_id])?;
        Ok(())
    }

    pub fn remove_group_member(&self, group_id: i64, node_id: &NodeId) -> Result<()> {
        self.db.connection().execute(
            "DELETE FROM route_group_members WHERE group_id = ?1 AND node_id = ?2",
            params![group_id, node_id.as_str()],
        )?;
        Ok(())
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
        store
            .replace_endpoints(
                &node.node_id,
                &[NodeEndpoint {
                    kind: EndpointKind::Tailscale,
                    address: "http://gunma.tailnet:4512".into(),
                    enabled: true,
                    record_allowed: true,
                    metered: false,
                    user_priority: 0,
                }],
            )
            .unwrap();
        assert_eq!(store.list_nodes().unwrap().len(), 1);
        assert_eq!(store.endpoints(&node.node_id).unwrap().len(), 1);
        assert_eq!(
            store.credential_for(&node.node_id).unwrap().unwrap(),
            credential
        );
    }

    #[test]
    fn pairing_code_is_single_use() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let code = PairingCode::random();
        let expires = chrono::Utc::now().timestamp_millis() + 600_000;

        store
            .create_pending_pairing(&code, Some("東京"), expires)
            .unwrap();
        assert_eq!(store.pending_pairings().unwrap().len(), 1);

        assert!(store.consume_pending_pairing(&code).unwrap());
        // A replay of the same code must not pair a second node.
        assert!(!store.consume_pending_pairing(&code).unwrap());
        assert!(store.pending_pairings().unwrap().is_empty());
    }

    #[test]
    fn duplicate_endpoints_collapse_instead_of_violating_the_unique_index() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let node = paired_node(&store, "tokyo");
        let endpoint = NodeEndpoint {
            kind: EndpointKind::Lan,
            address: "http://192.0.2.10:20773".into(),
            enabled: true,
            record_allowed: true,
            metered: false,
            user_priority: 0,
        };
        // The dashboard can submit the same endpoint twice; that asks for one
        // endpoint, not for a UNIQUE constraint failure surfaced as a 500.
        store
            .replace_endpoints(&node, &[endpoint.clone(), endpoint])
            .unwrap();
        assert_eq!(store.endpoints(&node).unwrap().len(), 1);
    }

    #[test]
    fn route_group_lookup_reports_the_name_owner() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let kanto = store.ensure_route_group("関東").unwrap();
        let tohoku = store.ensure_route_group("東北").unwrap();

        assert_eq!(store.route_group_id_by_name("関東").unwrap(), Some(kanto));
        assert_eq!(store.route_group_id_by_name("近畿").unwrap(), None);
        assert!(store.route_group_exists(tohoku).unwrap());
        assert!(!store.route_group_exists(tohoku + 1000).unwrap());
        // ensure_route_group is idempotent: the same name keeps its id.
        assert_eq!(store.ensure_route_group("関東").unwrap(), kanto);
    }

    #[test]
    fn route_group_members_can_be_renamed_removed_and_deleted() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let node = paired_node(&store, "tokyo");
        let group = store.ensure_route_group("関東").unwrap();
        store.set_group_member(group, &node, 200).unwrap();
        assert_eq!(
            store.route_group_members(group).unwrap(),
            vec![(node.clone(), 200)]
        );
        store.rename_route_group(group, "東日本").unwrap();
        assert_eq!(store.list_route_groups().unwrap()[0].name, "東日本");
        store.remove_group_member(group, &node).unwrap();
        assert!(store.route_group_members(group).unwrap().is_empty());
        store.delete_route_group(group).unwrap();
        assert!(store.list_route_groups().unwrap().is_empty());
    }

    #[test]
    fn expired_pairing_code_is_not_redeemable_and_is_cleaned_up() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let code = PairingCode::random();
        let already_expired = chrono::Utc::now().timestamp_millis() - 1;

        store
            .create_pending_pairing(&code, None, already_expired)
            .unwrap();
        assert!(store.pending_pairings().unwrap().is_empty());
        assert!(!store.consume_pending_pairing(&code).unwrap());
    }

    #[test]
    fn an_unrelated_code_never_matches() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let issued = PairingCode::random();
        let guessed = PairingCode::random();
        store
            .create_pending_pairing(
                &issued,
                None,
                chrono::Utc::now().timestamp_millis() + 600_000,
            )
            .unwrap();

        assert!(!store.consume_pending_pairing(&guessed).unwrap());
        // The real code still works afterwards.
        assert!(store.consume_pending_pairing(&issued).unwrap());
    }

    fn advertisement(
        node: &NodeId,
        route_id: &str,
        mux: LogicalMuxId,
        state: ReceptionRouteState,
    ) -> ReceptionRouteAdvertisement {
        ReceptionRouteAdvertisement {
            route_id: route_id.into(),
            origin_node: node.clone(),
            mux,
            logical_broadcast: LogicalBroadcastType::Bs,
            // BS delivered over CATV: logically still BS, physically not.
            ingress_delivery: DeliveryType::CatvTransmodulation,
            ultimate_delivery: DeliveryType::IsdbSDirect,
            path: Vec::new(),
            state,
            available_slots: 1,
            total_slots: 2,
            predicted_ready_ms: 0,
            source_quality: 0.7,
            confidence: 0.9,
            generation: 1,
            observed_at_unix_ms: 1_700_000_000_000,
        }
    }

    fn paired_node(store: &NodeStore, id: &str) -> NodeId {
        let node_id = NodeId::new(id).unwrap();
        store
            .upsert_node(
                &StoredNode {
                    node_id: node_id.clone(),
                    display_name: id.into(),
                    site_name: None,
                    enabled: true,
                    allow_transit: false,
                    auto_connect: true,
                    last_seen_unix_ms: None,
                },
                Some(&NodeCredential::random()),
            )
            .unwrap();
        node_id
    }

    #[test]
    fn peer_routes_round_trip_and_keep_logical_and_physical_apart() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let peer = paired_node(&store, "tokyo");
        let mux = LogicalMuxId {
            nid: 0x0004,
            tsid: 0x4010,
        };

        store
            .replace_remote_routes(
                &peer,
                &[advertisement(
                    &peer,
                    "/dev/px4video0#0:0",
                    mux,
                    ReceptionRouteState::Usable,
                )],
            )
            .unwrap();

        let routes = store.remote_routes_for(mux).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].node_id, peer);
        assert_eq!(routes[0].logical_broadcast, LogicalBroadcastType::Bs);
        assert_eq!(
            routes[0].ingress_delivery,
            DeliveryType::CatvTransmodulation
        );
        assert_eq!(routes[0].ultimate_delivery, DeliveryType::IsdbSDirect);
    }

    /// A peer's advertisement is the complete current picture: a route it
    /// stopped advertising must stop being a candidate here.
    #[test]
    fn replacing_peer_routes_drops_what_is_no_longer_advertised() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let peer = paired_node(&store, "tokyo");
        let mux = LogicalMuxId {
            nid: 0x0004,
            tsid: 0x4010,
        };

        store
            .replace_remote_routes(
                &peer,
                &[
                    advertisement(&peer, "a", mux, ReceptionRouteState::Usable),
                    advertisement(&peer, "b", mux, ReceptionRouteState::Usable),
                ],
            )
            .unwrap();
        assert_eq!(store.remote_routes_for(mux).unwrap().len(), 2);

        store
            .replace_remote_routes(
                &peer,
                &[advertisement(&peer, "a", mux, ReceptionRouteState::Usable)],
            )
            .unwrap();
        let routes = store.remote_routes_for(mux).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(routes[0].route_id.ends_with("::a"));
    }

    /// Non-routable routes stay in the table (so they can be re-probed) but
    /// are never offered as candidates.
    #[test]
    fn quarantined_routes_are_kept_but_not_offered() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let peer = paired_node(&store, "tokyo");
        let mux = LogicalMuxId {
            nid: 0x0004,
            tsid: 0x4010,
        };

        store
            .replace_remote_routes(
                &peer,
                &[advertisement(
                    &peer,
                    "weak",
                    mux,
                    ReceptionRouteState::Quarantined,
                )],
            )
            .unwrap();

        assert!(store.remote_routes_for(mux).unwrap().is_empty());
        let still_there: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM reception_routes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            still_there, 1,
            "a quarantined route must remain re-probeable"
        );
    }

    /// Two nodes may legitimately use the same local route id.
    #[test]
    fn route_ids_are_namespaced_per_node() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let tokyo = paired_node(&store, "tokyo");
        let gunma = paired_node(&store, "gunma");
        let mux = LogicalMuxId {
            nid: 0x0004,
            tsid: 0x4010,
        };
        let shared_id = "/dev/px4video0#0:0";

        store
            .replace_remote_routes(
                &tokyo,
                &[advertisement(
                    &tokyo,
                    shared_id,
                    mux,
                    ReceptionRouteState::Usable,
                )],
            )
            .unwrap();
        store
            .replace_remote_routes(
                &gunma,
                &[advertisement(
                    &gunma,
                    shared_id,
                    mux,
                    ReceptionRouteState::Usable,
                )],
            )
            .unwrap();

        let routes = store.remote_routes_for(mux).unwrap();
        assert_eq!(
            routes.len(),
            2,
            "one node must not overwrite the other's route"
        );
    }

    /// The plaintext code must not be recoverable from the database.
    #[test]
    fn only_the_digest_is_persisted() {
        let db = Database::open_in_memory().unwrap();
        let store = NodeStore::new(&db).unwrap();
        let code = PairingCode::random();
        store
            .create_pending_pairing(&code, None, chrono::Utc::now().timestamp_millis() + 600_000)
            .unwrap();

        let stored: String = db
            .connection()
            .query_row("SELECT code_hash FROM node_pending_pairings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_ne!(stored, code.as_str());
        assert_eq!(stored, code.digest());
    }
}

/// Serde is the single spelling of these enums, so the database and the wire
/// can never drift apart.
fn enum_str<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn parse_enum<T: serde::de::DeserializeOwned>(value: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).ok()
}
