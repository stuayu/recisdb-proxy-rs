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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use recisdb_protocol::StreamClass;
use tokio::sync::broadcast;

use super::frame::{FrameFlags, NodeTsFrame, NODE_TS_HEADER_LEN};
use super::transport::{LeaseStreamError, NodeTransportClient, OpenLeaseReply, OpenLeaseRequest};
use super::types::{LogicalMuxId, RequestContext};

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
        let shutdown = Arc::new(tokio::sync::Notify::new());

        let stream = Self {
            lease: lease.clone(),
            base_url: base_url.clone(),
            tx: tx.clone(),
            last_sequence: Arc::clone(&last_sequence),
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
            shutdown,
        );

        Ok(stream)
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
            })
        }
        Err(LeaseStreamError::LeaseGone) => return Err(ConsumeError::LeaseGone),
        Err(e) => return Err(ConsumeError::Transport(e.to_string())),
    };

    let mut buffer = BytesMut::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| ConsumeError::Transport(e.to_string()))?;
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
            // A closed channel means every local consumer went away; there is
            // nothing left to feed, so stop rather than keep the peer's tuner.
            if tx.send(frame.payload).is_err() && tx.receiver_count() == 0 {
                return Err(ConsumeError::LeaseGone);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuner::EffectiveClaim;
    use super::super::types::NodeId;

    fn context(class: StreamClass) -> RequestContext {
        RequestContext {
            request_id: "r".into(),
            trace_id: "t".into(),
            stream_class: class,
            claim: EffectiveClaim::new(2, false),
            remaining_ms: 10_000,
            origin_node: NodeId::new("tokyo").unwrap(),
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
            mux: LogicalMuxId { nid: 0x7FE0, tsid: 0x7FE0 },
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
        let node = NodeId::new("fukushima").unwrap();
        ctx.enter_node(&node, 3_000).unwrap();
        assert_eq!(ctx.remaining_ms, 7_000);
        assert_eq!(ctx.hop_count, 1);

        // The same node cannot be entered twice — that is a routing loop.
        let mut looping = ctx.clone();
        assert!(looping.enter_node(&node, 10).is_err());
    }
}
