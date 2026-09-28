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
use super::transport::{NodeTransportClient, NodeTransportState, RouteChangedNotice};

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
        let local_changes = self.tuner_pool.route_change_notifier();
        let peer_changes = Arc::clone(&self.state.route_sync_notify);
        tokio::spawn(async move {
            self.refresh_local().await;
            self.pull_peers().await;
            loop {
                tokio::select! {
                    _ = local_changes.notified() => {
                        self.refresh_local().await;
                        self.push_local_change().await;
                    }
                    _ = peer_changes.notified() => self.pull_peers().await,
                    _ = tokio::time::sleep(self.interval) => {
                        self.refresh_local().await;
                        self.pull_peers().await;
                    }
                }
            }
        });
    }

    async fn paired_peers(
        &self,
    ) -> Vec<(
        super::types::NodeId,
        super::identity::NodeCredential,
        Vec<super::types::NodeEndpoint>,
    )> {
        let db = self.database.lock().await;
        let Ok(store) = NodeStore::new(&db) else {
            return Vec::new();
        };
        let Ok(nodes) = store.list_nodes() else {
            return Vec::new();
        };
        nodes
            .into_iter()
            .filter(|node| node.enabled && node.auto_connect)
            .filter_map(|node| {
                let credential = store.credential_for(&node.node_id).ok().flatten()?;
                let endpoints = store.endpoints(&node.node_id).unwrap_or_default();
                Some((node.node_id, credential, endpoints))
            })
            .collect()
    }

    async fn push_local_change(&self) {
        let peers = self.paired_peers().await;
        let notice = RouteChangedNotice {
            generation: chrono::Utc::now().timestamp_millis().max(0) as u64,
        };
        for (node_id, credential, endpoints) in peers {
            let Ok(client) = NodeTransportClient::new(self.state.identity.node_id.clone(), credential)
            else {
                continue;
            };
            let mut sent = false;
            for endpoint in endpoints.into_iter().filter(|endpoint| endpoint.enabled) {
                let result = tokio::time::timeout(
                    Duration::from_millis(750),
                    client.notify_routes_changed(&endpoint.address, &notice),
                )
                .await;
                if matches!(result, Ok(Ok(()))) {
                    sent = true;
                    break;
                }
            }
            if !sent {
                log::debug!("[node] route change notice did not reach {node_id}");
            }
        }
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
        let peers = self.paired_peers().await;

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
