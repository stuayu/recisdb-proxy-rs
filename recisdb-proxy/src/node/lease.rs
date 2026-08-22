//! Connection-independent remote tuner leases.
//!
//! An HTTP/2/Tailscale/Cloudflare path is transport, not tuner ownership.
//! Losing that connection must not immediately close a physical tuner during
//! a recording. A lease has its own expiry and RECORD sessions can resume
//! against the same lease/replay buffer over a different transport path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use recisdb_protocol::StreamClass;
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::tuner::EffectiveClaim;

use super::frame::NodeTsFrame;
use super::replay::{ReplayBudget, ReplayBuffer, ReplayError};
use super::types::{LogicalMuxId, NodeId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteLeaseId(String);

impl RemoteLeaseId {
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("OS RNG unavailable for lease id");
        let mut out = String::with_capacity(32);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(&mut out, "{b:02x}");
        }
        Self(out)
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err("invalid remote lease id");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
pub struct RemoteMuxLease {
    pub id: RemoteLeaseId,
    pub owner_node: NodeId,
    pub route_id: String,
    pub mux: LogicalMuxId,
    pub sid: Option<u16>,
    pub stream_class: StreamClass,
    pub claim: EffectiveClaim,
    pub generation: u32,
    pub replay: Arc<Mutex<ReplayBuffer>>,
    live_tx: broadcast::Sender<NodeTsFrame>,
    state: Arc<Mutex<LeaseTimes>>,
}

struct LeaseTimes {
    created_at: Instant,
    renewed_at: Instant,
    expires_at: Instant,
}

impl RemoteMuxLease {
    fn new(
        owner_node: NodeId,
        route_id: String,
        mux: LogicalMuxId,
        sid: Option<u16>,
        stream_class: StreamClass,
        claim: EffectiveClaim,
        generation: u32,
        ttl: Duration,
        replay_budget: ReplayBudget,
    ) -> Self {
        let now = Instant::now();
        let (live_tx, _) = broadcast::channel(256);
        Self {
            id: RemoteLeaseId::random(),
            owner_node,
            route_id,
            mux,
            sid,
            stream_class,
            claim,
            generation,
            replay: Arc::new(Mutex::new(ReplayBuffer::new_for_generation(
                replay_budget,
                generation,
            ))),
            live_tx,
            state: Arc::new(Mutex::new(LeaseTimes {
                created_at: now,
                renewed_at: now,
                expires_at: now + ttl,
            })),
        }
    }

    /// Publish one source frame into both the replay history and live fanout.
    /// A source sequence gap is fatal before anything is broadcast.
    pub async fn publish(&self, frame: NodeTsFrame) -> Result<(), ReplayError> {
        self.replay.lock().await.push(frame.clone())?;
        let _ = self.live_tx.send(frame);
        Ok(())
    }

    pub fn subscribe_live(&self) -> broadcast::Receiver<NodeTsFrame> {
        self.live_tx.subscribe()
    }

    pub async fn renew(&self, ttl: Duration) {
        let now = Instant::now();
        let mut state = self.state.lock().await;
        state.renewed_at = now;
        state.expires_at = now + ttl;
    }

    pub async fn is_expired(&self, now: Instant) -> bool {
        self.state.lock().await.expires_at <= now
    }

    pub async fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.state.lock().await.created_at)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LeasePolicy {
    pub view_ttl: Duration,
    pub preview_ttl: Duration,
    /// Recording lease grace after a transport connection disappears.
    pub record_ttl: Duration,
    pub replay_budget: ReplayBudget,
}

impl Default for LeasePolicy {
    fn default() -> Self {
        Self {
            view_ttl: Duration::from_secs(8),
            preview_ttl: Duration::from_secs(10),
            record_ttl: Duration::from_secs(25),
            replay_budget: ReplayBudget::default(),
        }
    }
}

impl LeasePolicy {
    pub fn ttl(self, class: StreamClass) -> Duration {
        match class {
            StreamClass::View => self.view_ttl,
            StreamClass::Preview => self.preview_ttl,
            StreamClass::Record => self.record_ttl,
        }
    }
}

pub struct RemoteLeaseManager {
    leases: RwLock<HashMap<RemoteLeaseId, Arc<RemoteMuxLease>>>,
    policy: LeasePolicy,
}

impl RemoteLeaseManager {
    pub fn new(policy: LeasePolicy) -> Self {
        Self {
            leases: RwLock::new(HashMap::new()),
            policy,
        }
    }

    pub async fn create(
        &self,
        owner_node: NodeId,
        route_id: String,
        mux: LogicalMuxId,
        sid: Option<u16>,
        stream_class: StreamClass,
        claim: EffectiveClaim,
        generation: u32,
    ) -> Arc<RemoteMuxLease> {
        let lease = Arc::new(RemoteMuxLease::new(
            owner_node,
            route_id,
            mux,
            sid,
            stream_class,
            claim,
            generation,
            self.policy.ttl(stream_class),
            self.policy.replay_budget,
        ));
        self.leases.write().await.insert(lease.id.clone(), Arc::clone(&lease));
        lease
    }

    pub fn policy(&self) -> LeasePolicy {
        self.policy
    }

    pub async fn get(&self, id: &RemoteLeaseId) -> Option<Arc<RemoteMuxLease>> {
        let lease = self.leases.read().await.get(id).cloned()?;
        if lease.is_expired(Instant::now()).await {
            self.leases.write().await.remove(id);
            return None;
        }
        Some(lease)
    }

    pub async fn renew(&self, id: &RemoteLeaseId) -> bool {
        let Some(lease) = self.get(id).await else {
            return false;
        };
        lease.renew(self.policy.ttl(lease.stream_class)).await;
        true
    }

    pub async fn release(&self, id: &RemoteLeaseId) -> Option<Arc<RemoteMuxLease>> {
        self.leases.write().await.remove(id)
    }

    pub async fn reap_expired(&self) -> Vec<Arc<RemoteMuxLease>> {
        let now = Instant::now();
        let snapshot: Vec<_> = self.leases.read().await.values().cloned().collect();
        let mut expired_ids = Vec::new();
        for lease in snapshot {
            if lease.is_expired(now).await {
                expired_ids.push(lease.id.clone());
            }
        }
        let mut leases = self.leases.write().await;
        expired_ids
            .into_iter()
            .filter_map(|id| leases.remove(&id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crate::node::{FrameFlags, NodeTsFrame};

    #[tokio::test]
    async fn lease_lifetime_is_not_bound_to_transport_connection() {
        let manager = RemoteLeaseManager::new(LeasePolicy {
            view_ttl: Duration::from_secs(1),
            preview_ttl: Duration::from_secs(1),
            record_ttl: Duration::from_secs(30),
            replay_budget: ReplayBudget::default(),
        });
        let lease = manager
            .create(
                NodeId::new("fukushima").unwrap(),
                "gunma-route".into(),
                LogicalMuxId { nid: 1, tsid: 1 },
                Some(101),
                StreamClass::Record,
                EffectiveClaim::new(2, false),
                1,
            )
            .await;

        // No connection handle is stored in the lease. Looking it up again
        // represents reconnecting through a different transport path.
        let resumed = manager.get(&lease.id).await.unwrap();
        assert_eq!(resumed.id, lease.id);
        assert_eq!(resumed.claim, EffectiveClaim::new(2, false));
        assert!(resumed.replay.lock().await.replay_from(1, 0).unwrap().is_empty());
    }

    #[tokio::test]
    async fn publish_populates_live_and_replay_from_same_sequence() {
        let manager = RemoteLeaseManager::new(LeasePolicy::default());
        let lease = manager
            .create(
                NodeId::new("fukushima").unwrap(),
                "gunma-route".into(),
                LogicalMuxId { nid: 1, tsid: 1 },
                None,
                StreamClass::Record,
                EffectiveClaim::new(2, false),
                7,
            )
            .await;
        let mut live = lease.subscribe_live();
        lease
            .publish(NodeTsFrame {
                generation: 7,
                sequence: 100,
                source_monotonic_ms: 0,
                flags: FrameFlags::default(),
                payload: Bytes::from(vec![0x47; 188]),
            })
            .await
            .unwrap();
        assert_eq!(live.recv().await.unwrap().sequence, 100);
        assert_eq!(lease.replay.lock().await.replay_from(7, 100).unwrap()[0].sequence, 100);
    }
}
