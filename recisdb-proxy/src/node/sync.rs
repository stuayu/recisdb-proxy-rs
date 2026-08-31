//! Periodic exchange of reception-route advertisements between paired nodes.
//!
//! Two directions, both on the same tick:
//!
//! - **Outbound**: refresh what `GET /node/v3/routes` serves, so a peer's view
//!   of this node's free slots is at most one interval stale.
//! - **Inbound**: ask every paired, enabled peer what it can receive and store
//!   the answer in `reception_routes`.
//!
//! Storing the inbound picture is what makes remote routes survive a restart
//! and lets candidate discovery work without a round trip per request. The
//! advertisement is a *cache*, not the authority: a peer can still refuse a
//! lease, and `available_slots` is a hint that may already be wrong by the
//! time it is read.

use std::sync::Arc;
use std::time::Duration;

use crate::server::listener::DatabaseHandle;
use crate::tuner::TunerPool;

use super::advertise::{build_local_advertisements, store_peer_advertisements};
use super::store::NodeStore;
use super::transport::{NodeTransportClient, NodeTransportState};

/// Default refresh interval. Slot occupancy changes far faster than this, so
/// it is deliberately *not* the thing a lease decision relies on — it only
/// keeps the dashboard and candidate ordering roughly current.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(60);

pub struct RouteSync {
    state: Arc<NodeTransportState>,
    database: DatabaseHandle,
    tuner_pool: Arc<TunerPool>,
    interval: Duration,
}

impl RouteSync {
    pub fn new(
        state: Arc<NodeTransportState>,
        database: DatabaseHandle,
        tuner_pool: Arc<TunerPool>,
    ) -> Self {
        Self {
            state,
            database,
            tuner_pool,
            interval: DEFAULT_SYNC_INTERVAL,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn spawn(self) {
        tokio::spawn(async move {
            loop {
                self.refresh_local().await;
                self.pull_peers().await;
                tokio::time::sleep(self.interval).await;
            }
        });
    }

    /// Republish what this node can receive.
    async fn refresh_local(&self) {
        match build_local_advertisements(
            &self.database,
            &self.tuner_pool,
            &self.state.identity.node_id,
        )
        .await
        {
            Ok(advertisements) => {
                let count = advertisements.len();
                *self.state.routes.write().await = advertisements;
                log::debug!("[node] advertising {count} local reception route(s)");
            }
            Err(e) => log::warn!("[node] failed to build local route advertisements: {e}"),
        }
    }

    /// Ask every paired peer what it can receive.
    async fn pull_peers(&self) {
        let peers = {
            let db = self.database.lock().await;
            let store = match NodeStore::new(&db) {
                Ok(store) => store,
                Err(e) => {
                    log::warn!("[node] route sync: node store unavailable: {e}");
                    return;
                }
            };
            let nodes = match store.list_nodes() {
                Ok(nodes) => nodes,
                Err(e) => {
                    log::warn!("[node] route sync: cannot list nodes: {e}");
                    return;
                }
            };
            let mut peers = Vec::new();
            for node in nodes {
                if !node.enabled || !node.auto_connect {
                    continue;
                }
                let Ok(Some(credential)) = store.credential_for(&node.node_id) else {
                    // Not paired yet; nothing to authenticate with.
                    continue;
                };
                let endpoints = store.endpoints(&node.node_id).unwrap_or_default();
                peers.push((node.node_id, credential, endpoints));
            }
            peers
        };

        for (node_id, credential, endpoints) in peers {
            let client =
                match NodeTransportClient::new(self.state.identity.node_id.clone(), credential) {
                    Ok(client) => client,
                    Err(e) => {
                        log::warn!("[node] route sync: cannot build client for {node_id}: {e}");
                        continue;
                    }
                };

            // First endpoint that answers wins. Ranking paths by measured
            // health is `node::path`'s job and belongs to the lease request,
            // not to this metadata refresh.
            let mut advertisements = None;
            for endpoint in endpoints.iter().filter(|e| e.enabled) {
                match client.routes(&endpoint.address).await {
                    Ok(routes) => {
                        advertisements = Some(routes);
                        break;
                    }
                    Err(e) => log::debug!(
                        "[node] route sync: {} did not answer on {}: {}",
                        node_id,
                        endpoint.address,
                        e
                    ),
                }
            }

            let Some(advertisements) = advertisements else {
                log::debug!("[node] route sync: no endpoint of {node_id} answered");
                continue;
            };

            let db = self.database.lock().await;
            match store_peer_advertisements(
                &db,
                &self.state.identity.node_id,
                &node_id,
                &advertisements,
            ) {
                Ok(stored) => log::debug!(
                    "[node] route sync: stored {stored} route(s) from {node_id} ({} advertised)",
                    advertisements.len()
                ),
                Err(e) => log::warn!("[node] route sync: cannot store routes from {node_id}: {e}"),
            }
        }
    }
}
