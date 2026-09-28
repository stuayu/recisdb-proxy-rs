//! Consuming a remote node's tuner as if it were a local one.
//!
//! This is the "demand" half of the fabric. [`RemoteMuxStream`] opens a lease
//! on a peer, keeps it alive, and republishes the peer's TS into an ordinary
//! `broadcast::Sender<Bytes>` — the same shape `SharedTuner` hands to every
//! local consumer, so downstream code needs no remote-specific branch.
//!
//! The properties that matter (`docs/DISTRIBUTED_TUNER_FABRIC.md` §7/§8):
//!
//! - **The lease outlives the connection.** A dropped HTTP/2 stream is a
//!   transport event; it triggers a reconnect with `from_seq`, not a tuner
//!   release.
//! - **RECORD never resumes across a hole.** If the peer's replay buffer no
//!   longer covers the next sequence it answers `410 Gone`; a RECORD stream
//!   then ends with an error instead of silently continuing from live.
//!   VIEW/PREVIEW may resynchronize and carry on.
//! - **The end-to-end budget is shared.** Reconnects spend from the same
//!   `RequestContext.remaining_ms`; a hop never restarts a full timeout.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use recisdb_protocol::StreamClass;
use tokio::sync::broadcast;

use crate::server::listener::DatabaseHandle;
use crate::tuner::EffectiveClaim;

use super::frame::{FrameFlags, NodeTsFrame, NODE_TS_HEADER_LEN};
use super::identity::NodeIdentity;
use super::store::NodeStore;
use super::route::{rank_remote_routes, RemoteRouteCandidate};
use super::path::{score_path, PathHealth, PathPolicy, TransportPath};
use super::transport::{LeaseStreamError, NodeTransportClient, OpenLeaseReply, OpenLeaseRequest};
use super::types::{EndpointKind, LogicalMuxId, NodeEndpoint, RequestContext};

/// Capacity of the local republish channel. Matches the local tuner fanout
/// (`STREAMING_DESIGN.md`) so a slow consumer behaves identically whether the
/// source is a local BonDriver or a peer.
const REPUBLISH_CAPACITY: usize = 4096;

/// How much of the lease TTL to leave as headroom when scheduling renewals.
/// Renewing at half the TTL survives one lost renewal round trip.
const RENEW_FRACTION: u32 = 2;

/// Backoff between reconnect attempts. Deliberately short: the lease TTL is
/// the thing protecting the recording, and it is measured in seconds.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum ConsumeError {
    #[error("no usable transport path to the peer")]
    NoPath,
    #[error("peer refused the lease: {0}")]
    Refused(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("record stream lost data that cannot be replayed")]
    RecordGap,
    #[error("the peer released the lease")]
    LeaseGone,
}

/// A live remote mux, republished locally.
pub struct RemoteMuxStream {
    lease: OpenLeaseReply,
    base_url: String,
    tx: broadcast::Sender<Bytes>,
    /// Highest source sequence handed downstream, used for resume.
    last_sequence: Arc<AtomicU64>,
    /// Wall-clock timestamp of the last frame handed to local consumers.
    /// Used only for the BonDriver signal-level compatibility value.
    last_data_at_ms: Arc<AtomicU64>,
    /// Ends the pump/renew tasks when this handle drops.
    shutdown: Arc<tokio::sync::Notify>,
}

impl RemoteMuxStream {
    /// Open a lease on `base_url` and start republishing it locally.
    ///
    /// `context` is the shared end-to-end request context. It is passed to the
    /// peer unchanged, and the peer's post-`enter_node` view of it is returned
    /// inside the lease reply.
    pub async fn open(
        client: Arc<NodeTransportClient>,
        base_url: String,
        context: RequestContext,
        mux: LogicalMuxId,
        sid: Option<u16>,
        spent_ms: u64,
    ) -> Result<Self, ConsumeError> {
        let request = OpenLeaseRequest {
            context,
            mux,
            sid,
            spent_ms,
        };
        let lease = client
            .open_lease(&base_url, &request)
            .await
            .map_err(|e| ConsumeError::Refused(e.to_string()))?;

        let (tx, _) = broadcast::channel(REPUBLISH_CAPACITY);
        let last_sequence = Arc::new(AtomicU64::new(0));
        let last_data_at_ms = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(tokio::sync::Notify::new());

        let stream = Self {
            lease: lease.clone(),
            base_url: base_url.clone(),
            tx: tx.clone(),
            last_sequence: Arc::clone(&last_sequence),
            last_data_at_ms: Arc::clone(&last_data_at_ms),
            shutdown: Arc::clone(&shutdown),
        };

        spawn_renew_loop(
            Arc::clone(&client),
            base_url.clone(),
            lease.clone(),
            Arc::clone(&shutdown),
        );
        spawn_pump(
            client,
            base_url,
            lease,
            tx,
            last_sequence,
            last_data_at_ms,
            shutdown,
        );

        Ok(stream)
    }

    /// Find a peer that can receive `mux` and open a lease on it.
    ///
    /// Candidate peers come from the stored route advertisements
    /// (`node::sync`), which is a cache: a peer may still refuse, so this
    /// walks them in order until one succeeds.
    ///
    /// `budget_ms` is the whole end-to-end deadline for the attempt. It is
    /// spent across every peer and endpoint tried, never reset per attempt.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_best(
        database: &DatabaseHandle,
        local: &NodeIdentity,
        mux: LogicalMuxId,
        sid: Option<u16>,
        class: StreamClass,
        claim: EffectiveClaim,
        budget_ms: u64,
    ) -> Result<Self, ConsumeError> {
        let started = std::time::Instant::now();

        let peers = {
            let db = database.lock().await;
            let store = NodeStore::new(&db).map_err(|e| ConsumeError::Transport(e.to_string()))?;
            let routes = store
                .remote_routes_for(mux)
                .map_err(|e| ConsumeError::Transport(e.to_string()))?;

            let mut peer_data: HashMap<super::types::NodeId, (Vec<NodeEndpoint>, HashMap<String, PathHealth>)> = HashMap::new();
            for route in &routes {
                let Ok(Some(_credential)) = store.credential_for(&route.node_id) else {
                    continue;
                };
                let endpoints = store.endpoints(&route.node_id).unwrap_or_default();
                let health = endpoints
                    .iter()
                    .filter_map(|endpoint| {
                        store
                            .path_health(&route.node_id, endpoint)
                            .ok()
                            .flatten()
                            .map(|health| (endpoint.address.clone(), health))
                    })
                    .collect();
                peer_data.entry(route.node_id.clone()).or_insert((endpoints, health));
            }
            let mut ranked: Vec<RemoteRouteCandidate> = routes
                .into_iter()
                .filter(|route| peer_data.contains_key(&route.node_id))
                .map(|route| {
                    let (endpoints, health) = peer_data.get(&route.node_id).expect("peer data");
                    RemoteRouteCandidate {
                        transport_quality: best_transport_quality(endpoints, health, class),
                        route,
                    }
                })
                .collect();
            rank_remote_routes(&mut ranked, claim);
            let mut seen = HashSet::new();
            ranked
                .into_iter()
                .filter_map(|candidate| {
                    seen.insert(candidate.route.node_id.clone()).then(|| {
                        let (endpoints, health) = peer_data
                            .get(&candidate.route.node_id)
                            .expect("peer data");
                        (candidate.route.node_id, endpoints.clone(), health.clone())
                    })
                })
                .collect::<Vec<_>>()
        };
        if peers.is_empty() {
            return Err(ConsumeError::NoPath);
        }

        let mut last_error = None;
        for (node_id, endpoints, health) in peers {
            let credential = {
                let db = database.lock().await;
                let store =
                    NodeStore::new(&db).map_err(|e| ConsumeError::Transport(e.to_string()))?;
                match store.credential_for(&node_id) {
                    Ok(Some(credential)) => credential,
                    // Unpaired between the listing above and now.
                    _ => continue,
                }
            };
            let client = match NodeTransportClient::new(local.node_id.clone(), credential) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    last_error = Some(ConsumeError::Transport(e.to_string()));
                    continue;
                }
            };

            for endpoint in usable_endpoints_with_health(&endpoints, &health, class) {
                let spent_ms = started.elapsed().as_millis() as u64;
                if spent_ms >= budget_ms {
                    return Err(last_error.unwrap_or(ConsumeError::NoPath));
                }
                let context = RequestContext {
                    request_id: format!("remote-{}-{}", mux.nid, mux.tsid),
                    trace_id: format!("{}-{}", local.node_id, started.elapsed().as_nanos()),
                    stream_class: class,
                    claim,
                    remaining_ms: budget_ms,
                    origin_node: local.node_id.clone(),
                    // This node is recorded as visited so a peer can never
                    // route the request back to us.
                    visited_nodes: vec![local.node_id.clone()],
                    hop_count: 0,
                    max_hops: 3,
                };

                // Only the overall search budget bounds an attempt. A lease
                // open includes the peer's tune and reader start (seconds on a
                // cold tuner, far longer for 4K), so a short per-attempt cap
                // failed every cold start and orphaned the peer's lease. Fast
                // refusals (409) return at once, and an unreachable peer is
                // bounded by the client's connect timeout, so ordering (not a
                // per-attempt cap) is what keeps the search fast.
                let attempt_budget =
                    Duration::from_millis(budget_ms.saturating_sub(spent_ms));
                let open = tokio::time::timeout(
                    attempt_budget,
                    Self::open(
                        Arc::clone(&client),
                        endpoint.address.clone(),
                        context,
                        mux,
                        sid,
                        spent_ms,
                    ),
                )
                .await;
                let result = match open {
                    Ok(result) => result,
                    Err(_) => Err(ConsumeError::Transport("remote lease attempt timed out".into())),
                };
                match result {
                    Ok(stream) => {
                        log::info!(
                            "[node] opened remote {:?} lease for NID=0x{:04X} TSID=0x{:04X} on {} via {}",
                            class,
                            mux.nid,
                            mux.tsid,
                            node_id,
                            endpoint.address
                        );
                        return Ok(stream);
                    }
                    Err(e) => {
                        log::debug!(
                            "[node] {} could not serve NID=0x{:04X} TSID=0x{:04X} via {}: {}",
                            node_id,
                            mux.nid,
                            mux.tsid,
                            endpoint.address,
                            e
                        );
                        last_error = Some(e);
                    }
                }
            }
        }
        Err(last_error.unwrap_or(ConsumeError::NoPath))
    }

    /// Subscribe to the republished TS, exactly like `SharedTuner::subscribe`.
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.tx.subscribe()
    }

    pub fn lease(&self) -> &OpenLeaseReply {
        &self.lease
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Highest source sequence delivered downstream so far.
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence.load(Ordering::Acquire)
    }

    /// Return the compatibility signal used by BNDP while this source is
    /// remote. There is no RF meter at the consuming node, so 20 dB is a
    /// fixed non-zero value while frames are arriving; zero means the lease
    /// has not delivered data recently and prevents a scan from accepting a
    /// dead remote channel.
    pub fn signal_level(&self) -> f32 {
        const ACTIVE_WINDOW_MS: u64 = 2_000;
        let now = unix_now_ms();
        let last = self.last_data_at_ms.load(Ordering::Acquire);
        signal_level_from_last_data(last, now, ACTIVE_WINDOW_MS)
    }
}

fn signal_level_from_last_data(last: u64, now: u64, active_window_ms: u64) -> f32 {
    if last != 0 && now.saturating_sub(last) <= active_window_ms {
        20.0
    } else {
        0.0
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Drop for RemoteMuxStream {
    fn drop(&mut self) {
        // Stops the pump and renew loops. The peer's lease then expires on its
        // own TTL even if the release request never lands.
        self.shutdown.notify_waiters();
    }
}

fn spawn_renew_loop(
    client: Arc<NodeTransportClient>,
    base_url: String,
    lease: OpenLeaseReply,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let interval = Duration::from_millis((lease.ttl_ms / RENEW_FRACTION as u64).max(500));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    // Best-effort explicit release so the peer frees its
                    // tuner immediately rather than after the TTL.
                    let _ = client.release_lease(&base_url, &lease.lease_id).await;
                    return;
                }
                _ = tokio::time::sleep(interval) => {}
            }
            match client.renew_lease(&base_url, &lease.lease_id).await {
                Ok(true) => {}
                Ok(false) => {
                    log::warn!(
                        "[node] lease {} no longer exists on {}; stopping renewals",
                        lease.lease_id,
                        base_url
                    );
                    return;
                }
                Err(e) => log::warn!(
                    "[node] lease {} renew failed against {}: {}",
                    lease.lease_id,
                    base_url,
                    e
                ),
            }
        }
    });
}

fn spawn_pump(
    client: Arc<NodeTransportClient>,
    base_url: String,
    lease: OpenLeaseReply,
    tx: broadcast::Sender<Bytes>,
    last_sequence: Arc<AtomicU64>,
    last_data_at_ms: Arc<AtomicU64>,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let is_record = lease.stream_class == StreamClass::Record;
    tokio::spawn(async move {
        loop {
            let resume_from = match last_sequence.load(Ordering::Acquire) {
                0 => None,
                seq => Some(seq + 1),
            };

            let outcome = tokio::select! {
                _ = shutdown.notified() => return,
                outcome = pump_once(
                    &client,
                    &base_url,
                    &lease,
                    &tx,
                    &last_sequence,
                    &last_data_at_ms,
                    resume_from,
                ) => outcome,
            };

            match outcome {
                // The connection ended cleanly; for a lease that is still
                // alive this is a transport event, so reconnect and resume.
                Ok(()) => {}
                Err(ConsumeError::RecordGap) | Err(ConsumeError::LeaseGone) => {
                    // Terminal. Dropping the sender closes every subscriber's
                    // receiver, which downstream reports as a failed stream
                    // rather than a silently truncated one.
                    log::error!(
                        "[node] lease {} on {} cannot continue without a gap; ending the stream",
                        lease.lease_id,
                        base_url
                    );
                    return;
                }
                Err(e) => {
                    if is_record {
                        log::warn!(
                            "[node] RECORD lease {} lost its connection to {} ({}); resuming from seq {:?}",
                            lease.lease_id,
                            base_url,
                            e,
                            resume_from
                        );
                    } else {
                        log::debug!(
                            "[node] lease {} lost its connection to {} ({}); reconnecting",
                            lease.lease_id,
                            base_url,
                            e
                        );
                    }
                }
            }

            tokio::select! {
                _ = shutdown.notified() => return,
                _ = tokio::time::sleep(RECONNECT_BACKOFF) => {}
            }
        }
    });
}

/// One connection's worth of streaming. Returns `Ok(())` when the connection
/// ended and a reconnect is appropriate.
async fn pump_once(
    client: &NodeTransportClient,
    base_url: &str,
    lease: &OpenLeaseReply,
    tx: &broadcast::Sender<Bytes>,
    last_sequence: &AtomicU64,
    last_data_at_ms: &AtomicU64,
    resume_from: Option<u64>,
) -> Result<(), ConsumeError> {
    let is_record = lease.stream_class == StreamClass::Record;
    let mut response = match client
        .open_lease_stream(
            base_url,
            &lease.lease_id,
            Some(lease.generation),
            resume_from,
        )
        .await
    {
        Ok(response) => response,
        Err(LeaseStreamError::ReplayGap) => {
            return Err(if is_record {
                ConsumeError::RecordGap
            } else {
                // A viewer can start again from live; the gap is visible as a
                // discontinuity, not as a failure.
                last_sequence.store(0, Ordering::Release);
                ConsumeError::Transport("replay gap; restarting from live".into())
            });
        }
        Err(LeaseStreamError::LeaseGone) => return Err(ConsumeError::LeaseGone),
        Err(e) => return Err(ConsumeError::Transport(e.to_string())),
    };

    let mut buffer = BytesMut::new();
    // STARTING control frames consume sequence numbers too.  Resume state
    // therefore cannot use `last_sequence` as proof that TS was received.
    // Keep the first-data deadline active until an actual payload was handed
    // downstream, including after a reconnect that followed only controls.
    let mut first_data_received = last_data_at_ms.load(Ordering::Acquire) > 0;
    loop {
        let chunk = if !first_data_received {
            match lease.first_data_grace_ms.filter(|ms| *ms > 0) {
                Some(grace_ms) => tokio::time::timeout(
                    Duration::from_millis(grace_ms),
                    response.chunk(),
                )
                .await
                .map_err(|_| {
                    ConsumeError::Transport(format!(
                        "first TS data timeout after {grace_ms}ms"
                    ))
                })?
                .map_err(|e| ConsumeError::Transport(e.to_string()))?,
                // Older peers do not send the optional deadline or startup
                // control frames. Preserve their legacy wait-until-close
                // behavior instead of inventing an incompatible timeout.
                None => response
                    .chunk()
                    .await
                    .map_err(|e| ConsumeError::Transport(e.to_string()))?,
            }
        } else {
            response
                .chunk()
                .await
                .map_err(|e| ConsumeError::Transport(e.to_string()))?
        };
        let Some(chunk) = chunk else {
            // Server closed the body. The lease may well still be alive.
            return Ok(());
        };
        buffer.extend_from_slice(&chunk);

        loop {
            if buffer.len() < NODE_TS_HEADER_LEN {
                break;
            }
            let (frame, consumed) = match NodeTsFrame::decode(&buffer) {
                Ok(decoded) => decoded,
                // Not enough bytes for the declared payload yet — HTTP/2 DATA
                // boundaries are not frame boundaries.
                Err(super::frame::FrameError::Incomplete { .. })
                | Err(super::frame::FrameError::TooShort) => break,
                Err(e) => return Err(ConsumeError::Transport(e.to_string())),
            };
            let _ = buffer.split_to(consumed);

            // Replayed frames re-deliver history the downstream consumer has
            // by definition not seen (we asked from `last_sequence + 1`), so
            // they are forwarded like any other frame; only the ordering
            // guard below matters.
            if frame.sequence <= last_sequence.load(Ordering::Acquire) && frame.sequence != 0 {
                continue;
            }
            if frame.flags.contains(FrameFlags::END) {
                return Err(ConsumeError::LeaseGone);
            }

            last_sequence.store(frame.sequence, Ordering::Release);
            if frame.flags.contains(FrameFlags::STARTING) || frame.payload.is_empty() {
                // Startup control frames reset the timeout by causing the
                // next `chunk()` deadline to start now. Empty frames from an
                // older peer are equally harmless and must not reach TS
                // consumers.
                continue;
            }
            first_data_received = true;
            last_data_at_ms.store(unix_now_ms(), Ordering::Release);
            // A closed channel means every local consumer went away; there is
            // nothing left to feed, so stop rather than keep the peer's tuner.
            if tx.send(frame.payload).is_err() && tx.receiver_count() == 0 {
                return Err(ConsumeError::LeaseGone);
            }
        }
    }
}

/// Endpoints worth trying for `class`, best first. Stored probe health breaks
/// ties between reception candidates and keeps stale/unknown paths usable as a
/// fallback.
///
/// Two rules that are not just preference:
/// - RECORD only ever uses endpoints the operator marked `record_allowed`.
///   A recording must not be silently carried over a metered or best-effort
///   relay.
/// - RECORD refuses `CloudflarePublic` outright: a general-purpose HTTP proxy
///   is a bootstrap and fallback path, not a sustained recording path
///   (`docs/DISTRIBUTED_TUNER_FABRIC.md` §4).
#[cfg(test)]
fn usable_endpoints(endpoints: &[NodeEndpoint], class: StreamClass) -> Vec<&NodeEndpoint> {
    let mut usable: Vec<&NodeEndpoint> = endpoints
        .iter()
        .filter(|e| e.enabled)
        .filter(|e| match class {
            StreamClass::Record => e.record_allowed && e.kind != EndpointKind::CloudflarePublic,
            _ => true,
        })
        .collect();
    usable.sort_by_key(|e| (kind_rank(e.kind), -e.user_priority));
    usable
}

fn usable_endpoints_with_health<'a>(
    endpoints: &'a [NodeEndpoint],
    health: &HashMap<String, PathHealth>,
    class: StreamClass,
) -> Vec<&'a NodeEndpoint> {
    let mut usable: Vec<&NodeEndpoint> = endpoints
        .iter()
        .filter(|e| e.enabled)
        .filter(|e| match class {
            StreamClass::Record => e.record_allowed && e.kind != EndpointKind::CloudflarePublic,
            _ => true,
        })
        .collect();
    usable.sort_by(|a, b| {
        let score = |endpoint: &NodeEndpoint| {
            score_path(
                &TransportPath {
                    id: endpoint.address.clone(),
                    endpoint: endpoint.clone(),
                    health: health.get(&endpoint.address).cloned().unwrap_or_default(),
                },
                class,
                0,
                PathPolicy::default(),
            )
            .score
        };
        score(b)
            .total_cmp(&score(a))
            .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
            .then_with(|| b.user_priority.cmp(&a.user_priority))
            .then_with(|| a.address.cmp(&b.address))
    });
    usable
}

fn best_transport_quality(
    endpoints: &[NodeEndpoint],
    health: &HashMap<String, PathHealth>,
    class: StreamClass,
) -> f64 {
    usable_endpoints_with_health(endpoints, health, class)
        .into_iter()
        .map(|endpoint| {
            score_path(
                &TransportPath {
                    id: endpoint.address.clone(),
                    endpoint: endpoint.clone(),
                    health: health.get(&endpoint.address).cloned().unwrap_or_default(),
                },
                class,
                0,
                PathPolicy::default(),
            )
            .score
        })
        .max_by(f64::total_cmp)
        .unwrap_or(f64::NEG_INFINITY)
}

/// Lower is preferred. LAN beats an overlay, an overlay beats the open
/// Internet, and a public HTTP proxy is last.
const fn kind_rank(kind: EndpointKind) -> u8 {
    match kind {
        EndpointKind::Lan => 0,
        EndpointKind::Tailscale => 1,
        EndpointKind::CloudflarePrivate => 2,
        EndpointKind::Static => 3,
        EndpointKind::InternetDirect => 4,
        EndpointKind::CloudflarePublic => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::NodeId;
    use super::*;

    #[test]
    fn remote_signal_is_fixed_nonzero_only_while_data_is_recent() {
        assert_eq!(signal_level_from_last_data(0, 10_000, 2_000), 0.0);
        assert_eq!(signal_level_from_last_data(8_500, 10_000, 2_000), 20.0);
        assert_eq!(signal_level_from_last_data(7_999, 10_000, 2_000), 0.0);
    }

    #[tokio::test]
    async fn dropping_remote_stream_notifies_lease_tasks_for_release() {
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let notified = shutdown.notified();
        let (tx, _) = broadcast::channel(1);
        let stream = RemoteMuxStream {
            lease: OpenLeaseReply {
                lease_id: "lease".into(),
                generation: 1,
                owner_node: NodeId::new("site-b").unwrap(),
                route_id: "route".into(),
                stream_class: StreamClass::View,
                ttl_ms: 8_000,
                first_data_grace_ms: Some(60_000),
                context: RequestContext {
                    request_id: "request".into(),
                    trace_id: "trace".into(),
                    stream_class: StreamClass::View,
                    claim: EffectiveClaim::new(0, false),
                    remaining_ms: 1_000,
                    origin_node: NodeId::new("site-a").unwrap(),
                    visited_nodes: Vec::new(),
                    hop_count: 0,
                    max_hops: 3,
                },
            },
            base_url: "http://127.0.0.1".into(),
            tx,
            last_sequence: Arc::new(AtomicU64::new(0)),
            last_data_at_ms: Arc::new(AtomicU64::new(0)),
            shutdown: Arc::clone(&shutdown),
        };
        drop(stream);
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .expect("drop must wake renew/pump tasks");
    }

    fn context(class: StreamClass) -> RequestContext {
        RequestContext {
            request_id: "r".into(),
            trace_id: "t".into(),
            stream_class: class,
            claim: EffectiveClaim::new(2, false),
            remaining_ms: 10_000,
            origin_node: NodeId::new("site-c").unwrap(),
            visited_nodes: Vec::new(),
            hop_count: 0,
            max_hops: 3,
        }
    }

    /// The claim and stream class must reach the peer untouched: the request
    /// body is the only place they are expressed, and no hop may rewrite them.
    #[test]
    fn open_lease_request_carries_the_context_verbatim() {
        let ctx = context(StreamClass::Record);
        let request = OpenLeaseRequest {
            context: ctx.clone(),
            mux: LogicalMuxId {
                nid: 0x7FE0,
                tsid: 0x7FE0,
            },
            sid: Some(1024),
            spent_ms: 120,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["context"]["claim"]["priority"], 2);
        assert_eq!(json["context"]["claim"]["exclusive"], false);
        assert_eq!(json["context"]["stream_class"], "Record");
        assert_eq!(json["context"]["remaining_ms"], 10_000);
        assert_eq!(json["spent_ms"], 120);

        let decoded: OpenLeaseRequest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.context.claim.priority, ctx.claim.priority);
        assert_eq!(decoded.context.claim.exclusive, ctx.claim.exclusive);
        assert_eq!(decoded.context.remaining_ms, ctx.remaining_ms);
    }

    /// Reconnects must not restart the budget: the deadline is end-to-end.
    #[test]
    fn entering_a_node_spends_from_the_shared_budget() {
        let mut ctx = context(StreamClass::View);
        let node = NodeId::new("site-a").unwrap();
        ctx.enter_node(&node, 3_000).unwrap();
        assert_eq!(ctx.remaining_ms, 7_000);
        assert_eq!(ctx.hop_count, 1);

        // The same node cannot be entered twice — that is a routing loop.
        let mut looping = ctx.clone();
        assert!(looping.enter_node(&node, 10).is_err());
    }

    fn endpoint(kind: EndpointKind, address: &str, record_allowed: bool) -> NodeEndpoint {
        NodeEndpoint {
            kind,
            address: address.into(),
            enabled: true,
            record_allowed,
            metered: false,
            user_priority: 0,
        }
    }

    #[test]
    fn record_never_uses_a_path_not_marked_for_it() {
        let endpoints = vec![
            endpoint(EndpointKind::Lan, "http://lan", false),
            endpoint(EndpointKind::Tailscale, "http://ts", true),
        ];
        let record = usable_endpoints(&endpoints, StreamClass::Record);
        assert_eq!(record.len(), 1);
        assert_eq!(record[0].address, "http://ts");

        // A viewer may use either.
        assert_eq!(usable_endpoints(&endpoints, StreamClass::View).len(), 2);
    }

    #[test]
    fn record_refuses_the_public_http_fallback_even_when_allowed() {
        let endpoints = vec![endpoint(
            EndpointKind::CloudflarePublic,
            "https://public",
            true,
        )];
        assert!(usable_endpoints(&endpoints, StreamClass::Record).is_empty());
        assert_eq!(usable_endpoints(&endpoints, StreamClass::Preview).len(), 1);
    }

    #[test]
    fn endpoints_are_ordered_lan_overlay_then_open_internet() {
        let endpoints = vec![
            endpoint(EndpointKind::CloudflarePublic, "https://public", true),
            endpoint(EndpointKind::InternetDirect, "https://direct", true),
            endpoint(EndpointKind::Tailscale, "http://ts", true),
            endpoint(EndpointKind::Lan, "http://lan", true),
        ];
        let ordered: Vec<&str> = usable_endpoints(&endpoints, StreamClass::View)
            .iter()
            .map(|e| e.address.as_str())
            .collect();
        assert_eq!(
            ordered,
            vec![
                "http://lan",
                "http://ts",
                "https://direct",
                "https://public"
            ]
        );
    }

    #[test]
    fn user_priority_breaks_ties_within_a_kind() {
        let mut high = endpoint(EndpointKind::Tailscale, "http://preferred", true);
        high.user_priority = 10;
        let endpoints = vec![
            endpoint(EndpointKind::Tailscale, "http://other", true),
            high,
        ];
        let ordered = usable_endpoints(&endpoints, StreamClass::View);
        assert_eq!(ordered[0].address, "http://preferred");
    }
}
