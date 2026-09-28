//! Serving a [`RemoteMuxLease`] from this node's own tuners.
//!
//! This is the "supply" half of the fabric: a peer asks for a logical mux,
//! and this node turns that into an ordinary local tuner acquisition plus a
//! lease that outlives any single transport connection.
//!
//! Two rules shape the whole module:
//!
//! 1. **The lease request is an ordinary tune request.** It goes through
//!    `tuner::acquire::acquire` like every other path, carrying the
//!    requester's [`EffectiveClaim`] verbatim. A remote recording therefore
//!    contends with local viewers under exactly the same policy — priority is
//!    never reinterpreted per hop (`docs/DISTRIBUTED_TUNER_FABRIC.md` §5).
//! 2. **The lease owns the tuner subscription, not the HTTP connection.**
//!    The pump task holds a `TunerSubscription` for as long as the lease
//!    exists in the [`RemoteLeaseManager`]. A dropped connection therefore
//!    does not close the tuner mid-recording; only lease expiry or explicit
//!    release does (§7).

use std::sync::Arc;
use std::time::Duration;

use recisdb_protocol::StreamClass;
use serde::{Deserialize, Serialize};

use crate::database::{EpgScanStatus, EpgSource, ProgramUpsert};
use crate::scheduler::epg_dwell::{
    evaluate_dwell, mux_reached_target, DwellObservation, DwellVerdict, EpgDwellConfig,
};
use crate::server::listener::DatabaseHandle;
use crate::tuner::acquire::{acquire, AcquireError, AcquireRequest};
use crate::tuner::channel_key::ChannelKeySpec;
use crate::tuner::epg_collector::EpgCollector;
use crate::tuner::shared::TunerUsage;
use crate::tuner::{ChannelKey, TunerPool};

use super::frame::{FrameFlags, NodeTsFrame, MAX_NODE_TS_PAYLOAD};
use super::identity::NodeIdentity;
use super::lease::{MuxLeaseManager, RemoteLeaseManager, RemoteMuxLease};
use super::transport::RemoteEpgDwell;
use super::types::{HopError, LogicalMuxId, RequestContext};

/// TS packets per node frame. 188 * 1000 ≈ 188 KB, comfortably under
/// [`MAX_NODE_TS_PAYLOAD`] while keeping per-frame overhead negligible.
const TS_PACKET_SIZE: usize = 188;
const MAX_PACKETS_PER_FRAME: usize = 1000;

/// First generation of a freshly opened lease. It increments only when the
/// *source* changes underneath a live lease, which is what lets a resuming
/// client tell "reconnect" apart from "different tuner, history is gone".
const INITIAL_GENERATION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("request context rejected: {0}")]
    Hop(#[from] HopError),
    #[error("no local reception route for NID=0x{:04X} TSID=0x{:04X}", .0.nid, .0.tsid)]
    NoRoute(LogicalMuxId),
    #[error("EPG mux is already leased: NID=0x{:04X} TSID=0x{:04X}", .0.nid, .0.tsid)]
    MuxLeaseUnavailable(LogicalMuxId),
    /// Rendered rather than wrapped: `AcquireError` is crate-private, and a
    /// peer only needs to know that no local tuner could be given.
    #[error("local tuner unavailable: {0}")]
    Unavailable(String),
    #[error("database error: {0}")]
    Database(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEpgMetadataReply {
    pub programs: Vec<ProgramUpsertWire>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub elapsed_secs: Option<i64>,
    #[serde(default)]
    pub sections_seen: Option<u64>,
    #[serde(default)]
    pub services_total: Option<usize>,
    #[serde(default)]
    pub services_complete: Option<usize>,
    #[serde(default)]
    pub coverage_until: Option<i64>,
}

pub struct RemoteEpgMetadataOutcome {
    pub programs: Vec<ProgramUpsert>,
    pub status: EpgScanStatus,
    pub elapsed_secs: i64,
    pub sections_seen: u64,
    pub services_total: usize,
    pub services_complete: usize,
    pub coverage_until: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramUpsertWire {
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub event_id: u16,
    pub start_at: i64,
    pub duration_secs: i64,
    pub free_ca_mode: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub extended: Option<String>,
    pub genre: Option<i64>,
    pub updated_at: i64,
    #[serde(default)]
    pub source: u8,
    pub basic_updated_at: Option<i64>,
    pub extended_updated_at: Option<i64>,
}

impl From<ProgramUpsert> for ProgramUpsertWire {
    fn from(value: ProgramUpsert) -> Self {
        Self {
            nid: value.nid,
            sid: value.sid,
            tsid: value.tsid,
            event_id: value.event_id,
            start_at: value.start_at,
            duration_secs: value.duration_secs,
            free_ca_mode: value.free_ca_mode,
            name: value.name,
            description: value.description,
            extended: value.extended,
            genre: value.genre,
            updated_at: value.updated_at,
            source: value.source as u8,
            basic_updated_at: value.basic_updated_at,
            extended_updated_at: value.extended_updated_at,
        }
    }
}

impl From<ProgramUpsertWire> for ProgramUpsert {
    fn from(value: ProgramUpsertWire) -> Self {
        let basic_updated_at = value.basic_updated_at.or_else(|| {
            (value.name.is_some() || value.description.is_some() || value.genre.is_some())
                .then_some(value.updated_at)
        });
        let extended_updated_at = value
            .extended_updated_at
            .or_else(|| value.extended.as_ref().map(|_| value.updated_at));
        Self {
            nid: value.nid,
            sid: value.sid,
            tsid: value.tsid,
            event_id: value.event_id,
            start_at: value.start_at,
            duration_secs: value.duration_secs,
            free_ca_mode: value.free_ca_mode,
            name: value.name,
            description: value.description,
            extended: value.extended,
            genre: value.genre,
            updated_at: value.updated_at,
            source: if value.source == EpgSource::Schedule as u8 {
                EpgSource::Schedule
            } else {
                EpgSource::PresentFollowing
            },
            basic_updated_at,
            extended_updated_at,
        }
    }
}

/// Turns peer lease requests into local tuner acquisitions.
pub struct LocalMuxServer {
    identity: NodeIdentity,
    tuner_pool: Arc<TunerPool>,
    database: DatabaseHandle,
    leases: Arc<RemoteLeaseManager>,
    mux_leases: Arc<MuxLeaseManager>,
}

impl LocalMuxServer {
    pub fn new(
        identity: NodeIdentity,
        tuner_pool: Arc<TunerPool>,
        database: DatabaseHandle,
        leases: Arc<RemoteLeaseManager>,
        mux_leases: Arc<MuxLeaseManager>,
    ) -> Self {
        Self {
            identity,
            tuner_pool,
            database,
            leases,
            mux_leases,
        }
    }

    pub fn leases(&self) -> &Arc<RemoteLeaseManager> {
        &self.leases
    }

    /// Physical routes this node can currently offer for `mux`, as
    /// `ChannelKey`s ready to hand to `acquire`.
    ///
    /// Discovery only: which one wins is decided by `tuner::policy::decide`
    /// from the complete list, never here (CLAUDE.md「選局」).
    async fn local_candidates(
        &self,
        mux: LogicalMuxId,
        sid: Option<u16>,
    ) -> Result<Vec<ChannelKey>, ServeError> {
        let db = self.database.lock().await;
        let rows = db
            .get_channels_by_nid_tsid_ordered(mux.nid, mux.tsid, sid)
            .map_err(|e| ServeError::Database(e.to_string()))?;

        let mut candidates: Vec<ChannelKey> = Vec::new();
        for row in &rows {
            let key = ChannelKey::space_channel(
                &row.bon_driver_path,
                row.channel.bon_space.unwrap_or(0),
                row.channel.bon_channel.unwrap_or(0),
            );
            if !candidates.contains(&key) {
                candidates.push(key);
            }
        }
        Ok(candidates)
    }

    /// Open a lease for `mux`, acquiring a local tuner for it.
    ///
    /// `context` is mutated in place: entering this node consumes one hop,
    /// records this node in `visited_nodes` (loop detection) and subtracts
    /// `spent_ms` from the shared end-to-end budget. The caller must have
    /// already subtracted whatever it spent getting here — a hop never
    /// restarts a full timeout.
    pub async fn open_lease(
        &self,
        context: &mut RequestContext,
        mux: LogicalMuxId,
        sid: Option<u16>,
        spent_ms: u64,
    ) -> Result<Arc<RemoteMuxLease>, ServeError> {
        // Supply-side invariant: this endpoint is the terminal hop.  It may
        // acquire only this node's physical candidates; calling the
        // requester-side remote fallback here would create a multi-node loop.
        context.enter_node(&self.identity.node_id, spent_ms)?;

        let Some(_mux_lease) = self.mux_leases.try_acquire(mux) else {
            return Err(ServeError::MuxLeaseUnavailable(mux));
        };

        let candidates = self.local_candidates(mux, sid).await?;
        if candidates.is_empty() {
            return Err(ServeError::NoRoute(mux));
        }

        let request = AcquireRequest {
            candidates,
            // Verbatim. The requester's rank is the rank used here.
            priority: context.claim.priority,
            exclusive: context.claim.exclusive,
            bondriver_version: 2,
            carried_permit: None,
            warm: None,
            own_key: None,
            own_claim_id: None,
            own_key_will_free_slot: false,
            client_host: format!("node:{}", context.origin_node),
        };

        let outcome = acquire(&self.tuner_pool, &self.database, request)
            .await
            .map_err(|e: AcquireError| ServeError::Unavailable(e.to_string()))?;
        // A lease request never carries a permit or warm handle, so there is
        // nothing to hand back; assert that assumption rather than leaking.
        debug_assert!(outcome.unused_permit.is_none());
        debug_assert!(outcome.unused_warm.is_none());

        self.tuner_pool.cancel_idle_close(&outcome.key).await;

        let lease = self
            .leases
            .create_with_mux_lease(
                self.identity.node_id.clone(),
                route_id_for(&outcome.key),
                mux,
                sid,
                context.stream_class,
                context.claim,
                INITIAL_GENERATION,
                Some(_mux_lease),
            )
            .await;

        // The subscription is created here, before the pump task is spawned,
        // so the tuner's subscriber count reflects this lease the moment
        // `open_lease` returns. Otherwise the reader looks idle in the window
        // between acquire and the task being scheduled, and the idle-close /
        // reclaim paths could take it away from the peer that just asked.
        let usage = match context.stream_class {
            StreamClass::Record => TunerUsage::Record,
            StreamClass::Preview => TunerUsage::Preview,
            StreamClass::View => TunerUsage::View,
        };
        let subscription = outcome.tuner.subscribe_with_claim_class(
            context.claim.priority,
            context.claim.exclusive,
            usage,
        );

        let pump = LeasePump {
            lease: Arc::clone(&lease),
            subscription,
            leases: Arc::clone(&self.leases),
            tuner_pool: Arc::clone(&self.tuner_pool),
        };
        tokio::spawn(pump.run());

        log::info!(
            "[node] lease {} opened for {} (NID=0x{:04X} TSID=0x{:04X}, {:?}) on {:?}",
            lease.id.as_str(),
            context.origin_node,
            mux.nid,
            mux.tsid,
            context.stream_class,
            outcome.key
        );
        Ok(lease)
    }

    /// Parse EIT on this node and return only program rows. TS never crosses
    /// the node boundary on this endpoint.
    pub async fn collect_epg_metadata(
        &self,
        context: &mut RequestContext,
        mux: LogicalMuxId,
        sid: Option<u16>,
        spent_ms: u64,
        dwell_secs: u64,
        remote_dwell: Option<RemoteEpgDwell>,
    ) -> Result<RemoteEpgMetadataOutcome, ServeError> {
        context.enter_node(&self.identity.node_id, spent_ms)?;
        let Some(_mux_lease) = self.mux_leases.try_acquire(mux) else {
            return Err(ServeError::MuxLeaseUnavailable(mux));
        };
        let candidates = self.local_candidates(mux, sid).await?;
        if candidates.is_empty() {
            return Err(ServeError::NoRoute(mux));
        }
        let outcome = acquire(
            &self.tuner_pool,
            &self.database,
            AcquireRequest {
                candidates,
                priority: context.claim.priority,
                exclusive: context.claim.exclusive,
                bondriver_version: 2,
                carried_permit: None,
                warm: None,
                own_key: None,
                own_claim_id: None,
                own_key_will_free_slot: false,
                client_host: format!("node-epg:{}", context.origin_node),
            },
        )
        .await
        .map_err(|e: AcquireError| ServeError::Unavailable(e.to_string()))?;
        let mut subscription = outcome.tuner.subscribe_with_claim_class(
            context.claim.priority,
            context.claim.exclusive,
            TunerUsage::EpgActiveScan,
        );
        let mut collector = EpgCollector::new_metadata();
        let progress = collector.progress().clone();
        let started = tokio::time::Instant::now();
        let dwell = remote_dwell.map(EpgDwellConfig::from);
        let legacy_deadline = started + remote_metadata_dwell_duration(remote_dwell, dwell_secs);
        let cpu_hard_limit_percent = self
            .database
            .lock()
            .await
            .get_epg_global_settings()
            .ok()
            .map(|settings| settings.cpu_hard_limit_percent);
        let mut last_progress = progress.progress_counter();
        let mut last_progress_at = started;
        let status = loop {
            let now = chrono::Utc::now().timestamp();
            let completion = progress.mux_completion(mux.nid, mux.tsid);
            let counter = progress.progress_counter();
            if counter != last_progress {
                last_progress = counter;
                last_progress_at = tokio::time::Instant::now();
            }
            if let Some(dwell) = dwell {
                let observation = DwellObservation {
                    elapsed: started.elapsed(),
                    since_progress: last_progress_at.elapsed(),
                    any_sections: completion.sections_seen > 0,
                    reached_target: mux_reached_target(
                        &completion,
                        now,
                        dwell.target_future_coverage_hours,
                    ),
                    any_service_complete: completion.services_schedule_basic_complete > 0,
                    stream_closed: false,
                    cpu_hard_limit: cpu_hard_limit_percent.is_some_and(|limit| {
                        crate::scheduler::epg_scheduler::cpu_percent() as i64 >= limit
                    }),
                };
                if let DwellVerdict::Stop(status) = evaluate_dwell(&dwell, &observation) {
                    break status;
                }
            } else if tokio::time::Instant::now() >= legacy_deadline {
                break if completion.sections_seen > 0 {
                    EpgScanStatus::Partial
                } else {
                    EpgScanStatus::NoData
                };
            }
            let remaining = if dwell.is_some() {
                Duration::from_secs(1)
            } else {
                legacy_deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(Duration::from_secs(2))
            };
            match tokio::time::timeout(remaining, subscription.recv()).await {
                Ok(Ok(chunk)) => collector.process_ts_chunk(&chunk),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    if let Some(dwell) = dwell {
                        let completion = progress.mux_completion(mux.nid, mux.tsid);
                        let observation = DwellObservation {
                            elapsed: started.elapsed(),
                            since_progress: last_progress_at.elapsed(),
                            any_sections: completion.sections_seen > 0,
                            reached_target: mux_reached_target(
                                &completion,
                                chrono::Utc::now().timestamp(),
                                dwell.target_future_coverage_hours,
                            ),
                            any_service_complete: completion.services_schedule_basic_complete > 0,
                            stream_closed: true,
                            cpu_hard_limit: cpu_hard_limit_percent.is_some_and(|limit| {
                                crate::scheduler::epg_scheduler::cpu_percent() as i64 >= limit
                            }),
                        };
                        if let DwellVerdict::Stop(status) = evaluate_dwell(&dwell, &observation) {
                            break status;
                        }
                    } else {
                        break if progress.progress_counter() > 0 {
                            EpgScanStatus::Partial
                        } else {
                            EpgScanStatus::NoData
                        };
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(_) => {}
            }
        };
        let completion = progress.mux_completion(mux.nid, mux.tsid);
        Ok(RemoteEpgMetadataOutcome {
            programs: collector.drain_metadata_records(),
            status,
            elapsed_secs: started.elapsed().as_secs() as i64,
            sections_seen: completion.sections_seen,
            services_total: completion.services_total,
            services_complete: completion.services_schedule_basic_complete,
            coverage_until: completion.coverage_until,
        })
    }
}

fn remote_metadata_dwell_duration(dwell: Option<RemoteEpgDwell>, dwell_secs: u64) -> Duration {
    dwell.map_or(Duration::from_secs(dwell_secs.clamp(1, 300)), |dwell| {
        Duration::from_secs(dwell.max_dwell_secs.max(1))
    })
}

#[cfg(test)]
mod remote_metadata_tests {
    use super::*;

    #[test]
    fn remote_dwell_falls_back_to_dwell_secs_when_absent() {
        assert_eq!(
            remote_metadata_dwell_duration(None, 0),
            Duration::from_secs(1)
        );
        assert_eq!(
            remote_metadata_dwell_duration(None, 301),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn remote_epg_metadata_reply_without_status_deserializes() {
        let reply: RemoteEpgMetadataReply = serde_json::from_value(serde_json::json!({
            "programs": []
        }))
        .unwrap();
        assert!(reply.status.is_none());
        assert!(reply.elapsed_secs.is_none());
        assert!(reply.sections_seen.is_none());
        assert!(reply.services_total.is_none());
        assert!(reply.services_complete.is_none());
        assert!(reply.coverage_until.is_none());
    }
}

/// Stable identifier for the physical route a lease ended up on. Peers use it
/// only for diagnostics and for noticing that a re-lease landed elsewhere.
fn route_id_for(key: &ChannelKey) -> String {
    match &key.channel {
        ChannelKeySpec::SpaceChannel { space, channel } => {
            format!("{}#{}:{}", key.tuner_path, space, channel)
        }
        ChannelKeySpec::Simple(c) => format!("{}#{}", key.tuner_path, c),
    }
}

/// Moves TS from a local tuner into a lease's replay buffer and live fanout.
struct LeasePump {
    lease: Arc<RemoteMuxLease>,
    subscription: crate::tuner::shared::TunerSubscription,
    leases: Arc<RemoteLeaseManager>,
    tuner_pool: Arc<TunerPool>,
}

impl LeasePump {
    async fn run(mut self) {
        let lease_id = self.lease.id.clone();
        let class = self.lease.stream_class;
        let mut sequence: u64 = 0;
        let mut carry: Vec<u8> = Vec::new();
        let mut discontinuity_pending = false;
        let started = std::time::Instant::now();

        let reason = loop {
            // The lease, not the connection, decides how long the tuner is
            // held. When it expires or is released the pump stops and the
            // subscription drops with it.
            if self.leases.get(&lease_id).await.is_none() {
                break "released";
            }

            let received =
                tokio::time::timeout(Duration::from_secs(1), self.subscription.recv()).await;

            let data = match received {
                // Timeout is not an error: it is the periodic chance to
                // notice that the lease went away while the source was quiet.
                Err(_elapsed) => continue,
                Ok(Ok(data)) => data,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break "source_closed",
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                    if class == StreamClass::Record {
                        // CLAUDE.md / STREAMING_DESIGN.md §2: a recording must
                        // never lose data silently. There is no way to
                        // reconstruct the skipped chunks, so the lease fails
                        // loudly instead of producing a corrupt file at the
                        // far end.
                        log::warn!(
                            "[node] lease {} RECORD source lagged by {} chunk(s); closing the lease",
                            lease_id.as_str(),
                            skipped
                        );
                        break "record_broadcast_lag";
                    }
                    log::debug!(
                        "[node] lease {} source lagged by {} chunk(s); resynchronizing",
                        lease_id.as_str(),
                        skipped
                    );
                    // The carry buffer may hold a partial packet from before
                    // the gap; whatever follows is no longer its continuation.
                    carry.clear();
                    discontinuity_pending = true;
                    continue;
                }
            };

            carry.extend_from_slice(&data);
            while let Some(payload) = take_aligned_chunk(&mut carry) {
                sequence += 1;
                let flags = if discontinuity_pending {
                    discontinuity_pending = false;
                    FrameFlags::new(FrameFlags::DISCONTINUITY)
                } else {
                    FrameFlags::default()
                };
                let frame = NodeTsFrame {
                    generation: self.lease.generation,
                    sequence,
                    source_monotonic_ms: started.elapsed().as_millis() as u64,
                    flags,
                    payload,
                };
                if let Err(e) = self.lease.publish(frame).await {
                    // `publish` only fails on a replay-history sequence gap,
                    // which would make a RECORD resume silently lossy.
                    log::error!(
                        "[node] lease {} replay history broke: {}; closing the lease",
                        lease_id.as_str(),
                        e
                    );
                    self.finish(&lease_id, "replay_gap").await;
                    return;
                }
            }
        };

        self.finish(&lease_id, reason).await;
    }

    /// Drop the lease and the tuner subscription together, then let the pool's
    /// ordinary keep-alive rules decide the tuner's fate.
    async fn finish(self, lease_id: &super::lease::RemoteLeaseId, reason: &str) {
        log::info!("[node] lease {} closed: {}", lease_id.as_str(), reason);
        self.leases.release(lease_id).await;

        let tuner = Arc::clone(self.subscription.tuner());
        // Explicit drop so the subscriber count has already decremented
        // before `schedule_idle_close` checks it.
        drop(self.subscription);
        if !tuner.has_subscribers() {
            self.tuner_pool
                .schedule_idle_close(tuner.key.clone(), tuner)
                .await;
        }
    }
}

/// Split off as many whole 188-byte packets as fit in one frame, leaving any
/// partial trailing packet in `carry` for the next chunk.
///
/// Node frames must be packet-aligned ([`NodeTsFrame::encode`] rejects
/// anything else) but a broadcast chunk is whatever the reader happened to
/// hand over — the same problem `web/stream.rs::TsAligner` solves for HTTP.
fn take_aligned_chunk(carry: &mut Vec<u8>) -> Option<bytes::Bytes> {
    let whole_packets = carry.len() / TS_PACKET_SIZE;
    if whole_packets == 0 {
        return None;
    }
    let take_packets = whole_packets.min(MAX_PACKETS_PER_FRAME);
    let take = take_packets * TS_PACKET_SIZE;
    debug_assert!(take <= MAX_NODE_TS_PAYLOAD);
    let payload = bytes::Bytes::copy_from_slice(&carry[..take]);
    carry.drain(..take);
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_chunks_never_split_a_packet() {
        let mut carry = vec![0u8; TS_PACKET_SIZE * 2 + 30];
        let chunk = take_aligned_chunk(&mut carry).unwrap();
        assert_eq!(chunk.len(), TS_PACKET_SIZE * 2);
        assert_eq!(
            carry.len(),
            30,
            "the partial packet must stay for next time"
        );
        assert!(take_aligned_chunk(&mut carry).is_none());
    }

    #[test]
    fn a_frame_never_exceeds_the_payload_cap() {
        let mut carry = vec![0u8; TS_PACKET_SIZE * (MAX_PACKETS_PER_FRAME + 5)];
        let chunk = take_aligned_chunk(&mut carry).unwrap();
        assert_eq!(chunk.len(), TS_PACKET_SIZE * MAX_PACKETS_PER_FRAME);
        assert!(chunk.len() <= MAX_NODE_TS_PAYLOAD);
        assert_eq!(carry.len(), TS_PACKET_SIZE * 5);
    }

    #[test]
    fn route_id_is_stable_and_names_the_physical_target() {
        let key = ChannelKey::space_channel("/dev/px4video0", 0, 27);
        assert_eq!(route_id_for(&key), "/dev/px4video0#0:27");
        assert_eq!(route_id_for(&key), route_id_for(&key.clone()));
    }

    #[tokio::test]
    async fn open_lease_never_falls_back_when_no_local_route_exists() {
        let database = Arc::new(tokio::sync::Mutex::new(
            crate::database::Database::open_in_memory().unwrap(),
        ));
        let server = LocalMuxServer::new(
            NodeIdentity {
                node_id: super::super::types::NodeId::new("supply-node").unwrap(),
                display_name: "Supply node".to_owned(),
            },
            Arc::new(TunerPool::new(1)),
            database,
            Arc::new(RemoteLeaseManager::new(Default::default())),
            MuxLeaseManager::new(Duration::from_secs(60)),
        );
        let mut context = RequestContext {
            request_id: "test-request".to_owned(),
            trace_id: "test-trace".to_owned(),
            stream_class: StreamClass::View,
            claim: crate::tuner::EffectiveClaim::new(1, false),
            remaining_ms: 10_000,
            origin_node: super::super::types::NodeId::new("requester-node").unwrap(),
            visited_nodes: Vec::new(),
            hop_count: 0,
            max_hops: 3,
        };

        let error = match server
            .open_lease(
                &mut context,
                LogicalMuxId {
                    nid: 0x1001,
                    tsid: 0x2001,
                },
                None,
                0,
            )
            .await
        {
            Ok(_) => panic!("a supply node must not invent a remote route"),
            Err(error) => error,
        };
        assert!(matches!(error, ServeError::NoRoute(_)));
    }
}
