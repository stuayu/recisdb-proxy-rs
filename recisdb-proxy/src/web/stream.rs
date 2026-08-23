//! HTTP-TS streaming endpoints (STREAMING_DESIGN.md §6.3, §7.2).
//!
//! ```text
//! GET /api/stream/service/:sid                  raw TS passthrough
//! GET /api/stream/service/:sid?profile=preview  H.264 TS via the shared
//!                                                encoder pool (mpegts.js)
//! ```
//!
//! Both routes are registered on the same `/api/*` router as every other API
//! endpoint in `web/mod.rs`, so they sit behind the same bearer-token
//! `require_auth` middleware (STREAMING_DESIGN.md §6.5 — "無認証で映像を垂れ
//! 流さない").
//!
//! Channel resolution and tuner startup are delegated to
//! `server::channel_resolve` (shared with, conceptually, the same
//! `TunerPool`/`SharedTuner` calls `server::session` uses — see that
//! module's doc comment for why a full extraction of `handle_set_channel_space`
//! itself was not attempted).
//!
//! `StreamCleanup`, `broadcast_to_body_stream`, `respond_with_stream`,
//! `error_response`/`channel_resolve_error_response` and
//! `release_tuner_subscription` are `pub(crate)` so `web/mirakurun.rs` (P6,
//! STREAMING_DESIGN.md §7.1) can build its own passthrough streams on top of
//! the exact same response-body machinery instead of re-implementing it.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::stream::{self, Stream};
use log::{debug, info, warn};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::database::Database;
use crate::server::channel_resolve::{self, ChannelResolveError};
use crate::ts_analyzer::service_filter::TsServiceFilter;
use crate::ts_analyzer::{SYNC_BYTE, TS_PACKET_SIZE};
use crate::tuner::channel_key::ChannelKeySpec;
use crate::tuner::encoder_pool::{self, EncodeKey, EncoderPoolError, EncoderRuntimeConfig, SharedEncoder};
use crate::tuner::{EncoderPool, SharedTuner, TunerPool, TunerSubscription};
use crate::web::http_session::{HttpStreamSession, HttpStreamSessionInfo};
use crate::web::state::{SessionProtocol, WebState};
use recisdb_protocol::StreamClass;

/// Query parameters for `GET /api/stream/service/:sid`.
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// `"preview"` selects the shared H.264 encoder pipeline. Absent/empty
    /// means raw passthrough.
    pub profile: Option<String>,
    /// Disambiguates `GET /api/stream/service/by-sid/:sid`, whose `:sid` is
    /// the raw broadcast service_id — a value that repeats across networks
    /// (BS and BS4K reuse the same SID space, and terrestrial SIDs repeat
    /// across regions). When present, `stream_service_by_sid` resolves via
    /// `channel_resolve::resolve_service_by_nid_sid(nid, sid)` instead of
    /// `resolve_service_by_sid(sid)`. Not consumed by `stream_service`
    /// (`:sid` there is already the unambiguous `channels.id` primary key).
    pub nid: Option<u16>,
}

/// Cleanup for a live shared-encoder subscription, released together with
/// the tuner subscription in [`StreamCleanup::drop`].
struct EncoderCleanup {
    pool: Arc<EncoderPool>,
    key: EncodeKey,
    encoder: Arc<SharedEncoder>,
}

/// The receiver actually polled for each yielded chunk of a stream body.
///
/// Raw passthrough polls the tracked [`TunerSubscription`] directly — which
/// conveniently means dropping it (when the body stream itself is dropped)
/// also releases the tracked subscription automatically, no separate cleanup
/// step needed for that half of the RAII story. The `?profile=preview`
/// pipeline instead polls the [`SharedEncoder`]'s own (non-RAII, unrelated to
/// `SharedTuner`) broadcast receiver directly; in that mode the tracked
/// tuner subscription is kept alive separately — see
/// `StreamCleanup::parked_tuner_sub` — deliberately unread, purely to hold
/// the refcount for the response's lifetime.
pub(crate) enum BodyReceiver {
    Tuner(TunerSubscription),
    Encoder(broadcast::Receiver<Bytes>),
    /// TS republished from a lease on a peer's tuner (`node::consume`). The
    /// lease handle itself lives in [`StreamCleanup`], so the receiver here
    /// closing means the remote stream ended — which for a RECORD lease is a
    /// failure, exactly as a closed local broadcast is.
    Remote(broadcast::Receiver<Bytes>),
}

impl BodyReceiver {
    pub(crate) async fn recv(&mut self) -> Result<Bytes, broadcast::error::RecvError> {
        match self {
            BodyReceiver::Tuner(sub) => sub.recv().await,
            BodyReceiver::Encoder(rx) => rx.recv().await,
            BodyReceiver::Remote(rx) => rx.recv().await,
        }
    }
}

/// RAII guard tying an HTTP response body's lifetime to a tuner subscription
/// (and, for `?profile=preview`, a shared-encoder subscription).
///
/// The body stream (see [`broadcast_to_body_stream`]) owns exactly one of
/// these per request. Whenever axum/hyper drops that stream — client
/// disconnect, the connection resetting, or (never, in practice, since these
/// are unbounded live broadcasts) the stream ending on its own — `Drop` runs
/// and releases both subscriptions, mirroring what `server::session::Session`
/// does explicitly on every exit path (dropping its `ts_receiver`
/// `TunerSubscription` + `tuner_pool.schedule_idle_close(..)`, and
/// `encoder_pool.release(..)` from `stop_tsreplace_pipeline`). There is no
/// synchronous "session loop exiting" hook to hang that cleanup off here —
/// the stream's lifetime *is* the subscription's lifetime — so `Drop` spawns
/// a short detached task to do the (necessarily `async`) `schedule_idle_close`
/// / encoder-release work. This is the one part of P5 that cannot be
/// exercised by an integration test in this environment (no real client to
/// disconnect mid-stream); see the unit test below for the closest available
/// proxy: dropping the guard decrements `SharedTuner`'s subscriber_count.
pub(crate) struct StreamCleanup {
    /// `None` when the TS comes from a peer rather than a local tuner.
    tuner: Option<Arc<SharedTuner>>,
    /// Holding this handle is what keeps the peer's lease alive; dropping it
    /// releases the lease (and the peer's tuner) — the remote equivalent of
    /// dropping a `TunerSubscription`.
    remote: Option<Arc<crate::node::RemoteMuxStream>>,
    tuner_pool: Arc<TunerPool>,
    /// Present only in `?profile=preview` mode, where the actual data comes
    /// from [`BodyReceiver::Encoder`] rather than from a `TunerSubscription`
    /// — this field is what keeps the tracked tuner subscription (and thus
    /// `subscriber_count`) alive for the whole response lifetime in that
    /// case. `None` when `BodyReceiver::Tuner` already owns the (only)
    /// tracked subscription.
    parked_tuner_sub: Option<TunerSubscription>,
    encoder: Option<EncoderCleanup>,
    /// Dashboard registration for this stream (`web/http_session.rs`).
    /// Dropping it removes the row from the client list, so it belongs to the
    /// same RAII story as the subscriptions above. `None` only in tests and
    /// on paths that could not determine a peer address.
    session: Option<HttpStreamSession>,
    /// Receiver for `POST /api/clients/{id}/disconnect`, handed to the body
    /// stream when it is built (see [`StreamCleanup::take_shutdown`]). Kept
    /// here rather than in a separate parameter so that every existing
    /// `broadcast_to_body_stream(rx, cleanup)` call site keeps working.
    shutdown_rx: Option<mpsc::Receiver<()>>,
}

impl StreamCleanup {
    /// Build a cleanup guard for a plain tuner subscription with no shared
    /// encoder involved — what every Mirakurun-compatible passthrough stream
    /// uses (`web/mirakurun.rs`, STREAMING_DESIGN.md §7.1: "passthrough
    /// (無変換) が既定"). The subscription itself is expected to live in the
    /// sibling `BodyReceiver::Tuner`, not here.
    pub(crate) fn tuner_only(tuner: Arc<SharedTuner>, tuner_pool: Arc<TunerPool>) -> Self {
        Self { tuner: Some(tuner), remote: None, tuner_pool, parked_tuner_sub: None, encoder: None, session: None, shutdown_rx: None }
    }

    /// Cleanup guard for a stream fed by a lease on a peer's tuner.
    ///
    /// There is no local reader to keep warm, so there is no idle-close to
    /// schedule: the lease's own TTL is what protects the peer's tuner across
    /// a reconnect, and dropping the handle releases it.
    pub(crate) fn remote_only(
        remote: Arc<crate::node::RemoteMuxStream>,
        tuner_pool: Arc<TunerPool>,
    ) -> Self {
        Self { tuner: None, remote: Some(remote), tuner_pool, parked_tuner_sub: None, encoder: None, session: None, shutdown_rx: None }
    }

    /// Attach the dashboard registration for this stream, together with the
    /// remote-disconnect receiver it was given.
    pub(crate) fn with_session(mut self, session: HttpStreamSession, shutdown_rx: mpsc::Receiver<()>) -> Self {
        self.session = Some(session);
        self.shutdown_rx = Some(shutdown_rx);
        self
    }

    /// Take the remote-disconnect receiver out for the body stream to poll.
    pub(crate) fn take_shutdown(&mut self) -> Option<mpsc::Receiver<()>> {
        self.shutdown_rx.take()
    }

    /// Account for a chunk handed to the client, when this stream is
    /// registered on the dashboard.
    fn record_sent(&self, len: usize) {
        if let Some(session) = self.session.as_ref() {
            session.record_sent(len);
        }
    }

    /// Record why this stream is ending, for `session_history`. No-op for
    /// unregistered streams (tests, peer-less requests).
    pub(crate) fn set_disconnect_reason(&self, reason: &str) {
        if let Some(session) = self.session.as_ref() {
            session.set_disconnect_reason(reason);
        }
    }
}

impl Drop for StreamCleanup {
    fn drop(&mut self) {
        // Drop any parked subscription synchronously, *before* spawning the
        // async cleanup task below: a type with a custom `Drop` impl runs
        // that impl's body first and only auto-drops its own fields
        // afterward, so without this explicit `take()` the `has_subscribers()`
        // check below could run (in the spawned task) before this decrement
        // actually happens.
        let _ = self.parked_tuner_sub.take();
        // Releases the peer's lease. Synchronous: there is nothing async to
        // do, and the peer frees its tuner as soon as the release lands (or
        // when the lease TTL expires, whichever comes first).
        let _ = self.remote.take();

        let tuner = self.tuner.clone();
        let tuner_pool = Arc::clone(&self.tuner_pool);
        let encoder = self.encoder.take();
        tokio::spawn(async move {
            if let Some(tuner) = tuner {
                if !tuner.has_subscribers() {
                    tuner_pool
                        .schedule_idle_close(tuner.key.clone(), Arc::clone(&tuner))
                        .await;
                }
            }
            if let Some(EncoderCleanup { pool, key, encoder }) = encoder {
                pool.release(&key, &encoder).await;
            }
        });
    }
}

/// State owned by the `stream::unfold` powering the response body: the
/// receiver being forwarded, plus the cleanup guard that must outlive every
/// yielded chunk and only run once the stream itself is dropped.
///
/// Field order matters: `rx` must be declared (and thus dropped) before
/// `_cleanup` so that, in raw-passthrough mode where `rx` is
/// `BodyReceiver::Tuner`, the tracked subscription's decrement has already
/// happened by the time `_cleanup`'s `Drop` runs its `has_subscribers()`
/// check (Rust drops a struct's fields in declaration order).
struct StreamState {
    rx: BodyReceiver,
    /// Fires when the dashboard asks this client to disconnect
    /// (`POST /api/clients/{id}/disconnect`). `None` for unregistered
    /// streams, which then simply have no remote-shutdown path.
    shutdown_rx: Option<mpsc::Receiver<()>>,
    _cleanup: StreamCleanup,
}

/// Adapt a [`BodyReceiver`] into a `Stream` suitable for
/// `axum::body::Body::from_stream`.
///
/// `Lagged` is treated the same way `server/session.rs` treats it for
/// VIEW/PREVIEW classes (STREAMING_DESIGN.md §3.2/§3.3): logged and skipped,
/// not fatal — a lagging HTTP viewer keeps receiving the *current* live
/// edge rather than being disconnected. `Closed` (source tuner reader
/// stopped, or shared encoder chain stopped) ends the HTTP response body
/// normally, which the browser/mpegts.js/ffmpeg observes as EOF.
pub(crate) fn broadcast_to_body_stream(
    rx: BodyReceiver,
    mut cleanup: StreamCleanup,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let shutdown_rx = cleanup.take_shutdown();

    stream::unfold(StreamState { rx, shutdown_rx, _cleanup: cleanup }, |mut state| async move {
        loop {
            let received = match state.shutdown_rx.as_mut() {
                Some(shutdown_rx) => {
                    tokio::select! {
                        // Dashboard-initiated disconnect: end the body, which
                        // drops the cleanup guard (tuner subscription and
                        // dashboard registration) just like a client hangup.
                        _ = shutdown_rx.recv() => {
                            debug!("[HTTP stream] disconnect requested from the dashboard");
                            return None;
                        }
                        received = state.rx.recv() => received,
                    }
                }
                None => state.rx.recv().await,
            };

            match received {
                Ok(data) => {
                    state._cleanup.record_sent(data.len());
                    return Some((Ok(data), state));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("[HTTP stream] receiver lagged, skipped {} chunks", n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// Re-aligns a stream of arbitrary byte chunks into whole 188-byte TS
/// packets.
///
/// [`TsServiceFilter::filter`] requires packet-aligned input, but a broadcast
/// chunk is whatever the tuner reader happened to hand over. The BNDP path
/// solves this with `server/session.rs`'s `ts_send_carry`; this is the same
/// idea for the HTTP paths, shared by [`service_filtered_body_stream`] and
/// `web/mirakurun_program_stream.rs`.
pub(crate) struct TsAligner {
    carry: Vec<u8>,
}

impl TsAligner {
    /// Cap on the carry buffer before it is treated as unsynchronizable
    /// garbage and dropped: a handful of TS packets is all that can
    /// legitimately be waiting for its tail. Without this, a source that
    /// never produces a sync byte would grow the buffer without bound.
    const MAX_CARRY: usize = TS_PACKET_SIZE * 16;

    pub(crate) fn new() -> Self {
        Self { carry: Vec::new() }
    }

    /// Append `data` and return whatever whole packets are now available,
    /// starting on a sync byte. `None` when there is not (yet) a full packet.
    pub(crate) fn push(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        self.carry.extend_from_slice(data);

        if !self.resync() {
            return None;
        }

        let aligned_len = self.carry.len() - (self.carry.len() % TS_PACKET_SIZE);
        if aligned_len == 0 {
            if self.carry.len() > Self::MAX_CARRY {
                debug!("[HTTP stream] dropping {} unsynchronized bytes", self.carry.len());
                self.carry.clear();
            }
            return None;
        }

        Some(self.carry.drain(..aligned_len).collect())
    }

    /// Called when chunks were lost (`RecvError::Lagged`): whatever is held is
    /// the head of a packet whose tail will never arrive, so the boundary has
    /// to be found again from the next chunk.
    pub(crate) fn on_gap(&mut self) {
        self.carry.clear();
    }

    /// Drop leading bytes until the buffer starts on a TS sync byte. `false`
    /// if no sync byte is present at all (the buffer is then cleared, since
    /// none of it can start a packet).
    fn resync(&mut self) -> bool {
        if self.carry.first() == Some(&SYNC_BYTE) {
            return true;
        }
        match self.carry.iter().position(|b| *b == SYNC_BYTE) {
            Some(pos) => {
                self.carry.drain(..pos);
                true
            }
            None => {
                self.carry.clear();
                false
            }
        }
    }
}

/// What a stream does when the broadcast channel reports `Lagged`.
///
/// STREAMING_DESIGN.md §2 / CLAUDE.md: a recording must never lose data
/// silently. A viewer would rather resynchronize than be disconnected, but a
/// recording that skips bytes produces a file whose corruption is only
/// discovered on playback, so the recording is failed loudly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LossPolicy {
    /// VIEW/PREVIEW: drop the torn carry buffer, resynchronize, keep going.
    Recover,
    /// RECORD: terminate the response with an error and record the reason in
    /// `session_history`.
    Fatal,
}

/// State for [`service_filtered_body_stream`]. Field order matters for the
/// same reason [`StreamState`]'s does: `rx` must drop before `_cleanup`.
struct FilteredStreamState {
    rx: BodyReceiver,
    /// See [`StreamState::shutdown_rx`].
    shutdown_rx: Option<mpsc::Receiver<()>>,
    filter: TsServiceFilter,
    aligner: TsAligner,
    loss_policy: LossPolicy,
    /// Set once [`LossPolicy::Fatal`] has yielded its error, so the stream
    /// ends instead of being polled again.
    fatal: bool,
    _cleanup: StreamCleanup,
}

/// Like [`broadcast_to_body_stream`], but filters the multiplex down to the
/// single service `target_sid` (rewritten PAT + that service's PMT/ES PIDs +
/// the SI tables every client needs), using the same [`TsServiceFilter`] the
/// BNDP session path uses.
///
/// This is what makes `GET /mirakurun/api/services/:id/stream` behave like
/// real Mirakurun, whose per-service stream carries one service — measured
/// against the reference Mirakurun on the same reception setup, the unfiltered
/// output carried 34 distinct PIDs where Mirakurun's carried 21 (the
/// sub-channel, one-seg and data-broadcast services riding the same
/// multiplex). EPGStation records straight from this endpoint, so the
/// difference is recording size and what its drop check counts.
///
/// Two behaviours differ from the plain passthrough, both forced by the
/// filter's "input must be 188-byte aligned" contract:
/// - Chunks are re-aligned through `carry` (broadcast chunk boundaries are
///   whatever the reader handed over, exactly as `server/session.rs`'s
///   `ts_send_carry` handles for the BNDP path).
/// - On `Lagged` the carry buffer is dropped and re-synchronized, but the
///   filter itself is **not** reset: its PID whitelist stays valid across a
///   gap, and resetting would blank the stream until the next PAT/PMT pair.
///   Sections torn by the gap fail their CRC and are simply re-collected.
///
/// Until the first PAT+PMT pair is parsed the filter emits only the
/// always-pass SI PIDs, so a client may see a brief lead-in with no video —
/// the same warm-up real Mirakurun has.
pub(crate) fn service_filtered_body_stream(
    rx: BodyReceiver,
    mut cleanup: StreamCleanup,
    target_sid: u16,
    loss_policy: LossPolicy,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let state = FilteredStreamState {
        rx,
        shutdown_rx: cleanup.take_shutdown(),
        filter: TsServiceFilter::new(target_sid),
        aligner: TsAligner::new(),
        loss_policy,
        fatal: false,
        _cleanup: cleanup,
    };

    stream::unfold(state, |mut state| async move {
        if state.fatal {
            return None;
        }
        loop {
            let received = match state.shutdown_rx.as_mut() {
                Some(shutdown_rx) => {
                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            debug!("[HTTP stream] disconnect requested from the dashboard");
                            return None;
                        }
                        received = state.rx.recv() => received,
                    }
                }
                None => state.rx.recv().await,
            };

            match received {
                Ok(data) => {
                    let Some(chunk) = state.aligner.push(&data) else { continue };
                    let filtered = state.filter.filter(&chunk);
                    if filtered.is_empty() {
                        continue;
                    }
                    state._cleanup.record_sent(filtered.len());
                    return Some((Ok(Bytes::from(filtered)), state));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    state.aligner.on_gap();
                    match state.loss_policy {
                        LossPolicy::Recover => {
                            debug!("[HTTP stream] filtered receiver lagged, skipped {} chunks", n);
                            continue;
                        }
                        LossPolicy::Fatal => {
                            warn!(
                                "[HTTP stream] RECORD receiver lagged by {} chunks; terminating rather than silently corrupting the recording",
                                n
                            );
                            state._cleanup.set_disconnect_reason("record_broadcast_lag");
                            state.fatal = true;
                            let error = std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("record stream lost {n} broadcast chunk(s)"),
                            );
                            return Some((Err(error), state));
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => match state.loss_policy {
                    // The source ended. For a viewer that is an ordinary EOF.
                    LossPolicy::Recover => return None,
                    // For a recording it is not: the response has already been
                    // sent as `200 OK`, so ending the body normally would look
                    // to EPGStation like a complete recording that just
                    // happens to stop early. Fail the body instead, and leave
                    // the reason in `session_history`.
                    LossPolicy::Fatal => {
                        warn!(
                            "[HTTP stream] RECORD source closed mid-stream; failing the response rather than reporting a truncated recording as complete"
                        );
                        state._cleanup.set_disconnect_reason("record_source_closed");
                        state.fatal = true;
                        let error = std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "record stream source closed",
                        );
                        return Some((Err(error), state));
                    }
                },
            }
        }
    })
}

pub(crate) fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "success": false, "error": message.into() }))).into_response()
}

/// `context` is only used for the log line (e.g. the `channels.id` or the
/// Mirakurun service id being resolved) — the message sent to the client
/// always comes from `e`'s own `Display` impl, which already names whichever
/// key (`channels.id` or `(nid, sid)`) the caller resolved by.
pub(crate) fn channel_resolve_error_response(
    context: impl std::fmt::Display,
    e: &ChannelResolveError,
) -> Response {
    let status = match e {
        ChannelResolveError::NotFound(_)
        | ChannelResolveError::NotFoundNidSid(_, _)
        | ChannelResolveError::Disabled(_)
        | ChannelResolveError::NoDriver(_)
        | ChannelResolveError::NoPhysicalChannel(_) => StatusCode::NOT_FOUND,
        ChannelResolveError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ChannelResolveError::Pool(_)
        | ChannelResolveError::ReaderStart(_)
        | ChannelResolveError::Busy { .. } => StatusCode::SERVICE_UNAVAILABLE,
    };
    warn!("[HTTP stream] service {} unavailable: {}", context, e);
    error_response(status, e.to_string())
}

/// Release a tracked tuner subscription taken speculatively, before any
/// response stream was built (i.e. on an error path after `subscribe()` but
/// before handing the subscription to [`broadcast_to_body_stream`]). Takes
/// the subscription by value and drops it explicitly so the
/// `has_subscribers()` check below observes the post-release count.
pub(crate) async fn release_tuner_subscription(tuner_pool: &Arc<TunerPool>, tuner_sub: TunerSubscription) {
    let tuner = Arc::clone(tuner_sub.tuner());
    drop(tuner_sub);
    if !tuner.has_subscribers() {
        tuner_pool
            .schedule_idle_close(tuner.key.clone(), tuner)
            .await;
    }
}

/// Which preview encode template a channel needs. BS4K differs enough from
/// every other band (progressive 2160p H.265 rather than 1080i H.264) that it
/// cannot share one — see `DEFAULT_PREVIEW_4K_ENCODE_ARGS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewBand {
    FourK,
    Other,
}

impl PreviewBand {
    /// Classified from the NID, the same way every other 4K decision in the
    /// codebase is (`tuner/acquire.rs::b25_enabled_for`, the channel
    /// enumeration): `band_type` is only populated by a scan and may be
    /// missing on hand-inserted rows, while the NID always identifies the
    /// network.
    fn of_nid(nid: u16) -> Self {
        match recisdb_protocol::broadcast_region::classify_nid(nid).0 {
            recisdb_protocol::BroadcastType::FourK => Self::FourK,
            _ => Self::Other,
        }
    }
}

/// Load the runtime encoder settings for `?profile=preview`: codec/bitrate/
/// arguments from the `encode_profiles` row with `purpose='preview'`
/// (STREAMING_DESIGN.md §5.3), everything else from `preview_encoder_config`
/// — the browser-preview pipeline's own table, fully separate from the BNDP
/// (TVTest) `tsreplace_config` which is never consulted here. Both
/// executable paths are TOML-only (`[preview]` section, REVIEW S1).
fn load_preview_encoder_config(
    db: &Database,
    band: PreviewBand,
) -> Result<(EncoderRuntimeConfig, u64), String> {
    // BS4K is progressive 2160p H.265: the ordinary preview template
    // deinterlaces it and encodes at full resolution, which no realtime
    // encoder keeps up with — the picture breaks up. Prefer the 4K template
    // when the channel is 4K, and fall back to the ordinary one if the admin
    // deleted or disabled it (a stuttering preview beats no preview).
    let profile = match band {
        PreviewBand::FourK => db
            .get_encode_profile_by_purpose("preview4k")
            .map_err(|e| e.to_string())?,
        PreviewBand::Other => None,
    };
    let profile = match profile {
        Some(p) => p,
        None => db
            .get_encode_profile_by_purpose("preview")
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                "no enabled encode profile with purpose='preview' is configured".to_string()
            })?,
    };

    let (enabled, command_path, preprocessor_path, preprocessor_arguments, read_timeout_ms) =
        db.get_preview_encoder_config().map_err(|e| e.to_string())?;

    if !enabled {
        return Err(
            "preview_encoder_config.enabled is false; ?profile=preview requires the browser \
             preview pipeline to be enabled (see the dashboard's browser preview settings)"
                .to_string(),
        );
    }
    if command_path.trim().is_empty() {
        return Err(
            "preview_encoder_config.command_path is not set; configure [preview] command_path \
             in recisdb-proxy.toml"
                .to_string(),
        );
    }

    let arguments = profile.extra_args.clone().unwrap_or_default();
    let cfg = EncoderRuntimeConfig {
        command_path,
        arguments,
        read_timeout_ms,
        preprocessor_path,
        preprocessor_arguments,
    };
    let generation = encoder_pool::config_generation(&cfg);

    Ok((cfg, generation))
}

/// `GET /api/stream/service/:sid[?profile=preview]`.
/// NOTE: `:sid` here is historically the `channels.id` primary key, not the
/// broadcast service_id — the dashboard UI uses `stream_service_by_sid`.
pub async fn stream_service(
    State(web_state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Path(sid): Path<i64>,
    Query(query): Query<StreamQuery>,
) -> Response {
    let resolved = {
        let db = web_state.database.lock().await;
        channel_resolve::resolve_service(&db, sid)
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => return channel_resolve_error_response(sid, &e),
    };
    stream_resolved(web_state, resolved, query, sid, peer).await
}

/// `GET /api/stream/service/by-sid/:sid[?profile=preview][&nid=N]` — same
/// streaming behaviour but `:sid` is the real broadcast service_id (what the
/// UI shows as SID everywhere).
///
/// A bare SID is not a unique service identity: it repeats across networks
/// (BS/BS4K share one SID space, and terrestrial SIDs repeat across
/// regions), so resolving by SID alone can silently land on the wrong
/// network's service. When the caller supplies `nid`, that ambiguity can't
/// arise and resolution goes straight through
/// `channel_resolve::resolve_service_by_nid_sid`. When `nid` is omitted,
/// this checks whether `sid` is actually ambiguous (spans more than one
/// distinct NID among its enabled rows) before falling back to the old
/// `resolve_service_by_sid` behaviour, and reports 409 instead of silently
/// picking one network if it is.
pub async fn stream_service_by_sid(
    State(web_state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Path(sid): Path<u16>,
    Query(query): Query<StreamQuery>,
) -> Response {
    let resolved = {
        let db = web_state.database.lock().await;
        match query.nid {
            Some(nid) => channel_resolve::resolve_service_by_nid_sid(&db, nid, sid),
            None => match ambiguous_sid_response(&db, sid) {
                Some(resp) => return resp,
                None => channel_resolve::resolve_service_by_sid(&db, sid),
            },
        }
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => return channel_resolve_error_response(sid as i64, &e),
    };
    stream_resolved(web_state, resolved, query, sid as i64, peer).await
}

/// When `sid` (looked up without a `nid` hint) has enabled rows on more than
/// one distinct NID, resolving it unambiguously is impossible — some other
/// network's service could be picked instead of the one the caller meant.
/// Returns `Some(409 response)` naming every candidate NID in that case, or
/// `None` when it's safe to fall through to
/// `channel_resolve::resolve_service_by_sid` (0 or 1 distinct NID among the
/// enabled rows — `resolve_service_by_sid` itself reports `NotFound`/
/// `Disabled` for the 0 case, so this only ever short-circuits the
/// genuinely-ambiguous case).
fn ambiguous_sid_response(db: &Database, sid: u16) -> Option<Response> {
    let rows = db.get_channels_by_sid(sid).ok()?;
    let mut nids: Vec<u16> = rows.iter().filter(|c| c.is_enabled).map(|c| c.nid).collect();
    nids.sort_unstable();
    nids.dedup();
    if nids.len() < 2 {
        return None;
    }
    let nid_list = nids.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    warn!("[HTTP stream] SID {} is ambiguous across NID {} without a nid= hint", sid, nid_list);
    Some(error_response(
        StatusCode::CONFLICT,
        format!(
            "SID {sid} は NID {nid_list} に存在するため一意に解決できません。\
             ?nid= で対象のNIDを指定してください。"
        ),
    ))
}

/// Build the dashboard registration payload for a resolved channel.
///
/// `channel_info` is formatted the same way the BNDP path formats it
/// (`server/session.rs::apply_channel_metadata`) so both kinds of row read
/// alike in the client list.
///
/// `tuner` is the [`SharedTuner`] `channel_resolve::start_tuner_for_service`
/// actually returned, NOT necessarily `resolved.primary()`: `resolved` may
/// carry several physical candidates (same service scanned into more than
/// one BonDriver — `server/channel_resolve.rs`'s module doc comment), and
/// `tuner::acquire::acquire` is free to settle on any of them (e.g. joining
/// one that's already running elsewhere). `tuner.key` is the only source of
/// truth for which driver/space/channel this session actually landed on;
/// using `resolved`'s metadata here would show the wrong driver whenever the
/// chosen candidate isn't the first one.
pub(crate) fn session_info_for(
    protocol: SessionProtocol,
    peer: Option<ConnectInfo<SocketAddr>>,
    resolved: &channel_resolve::ResolvedService,
    tuner: &SharedTuner,
    stream_class: StreamClass,
) -> HttpStreamSessionInfo {
    session_info_for_source(
        protocol,
        peer,
        resolved,
        Some(tuner),
        None,
        stream_class,
    )
}

/// Same, for a stream that may be fed by a peer instead of a local tuner.
///
/// The dashboard must always show *why* a tuner is busy (CLAUDE.md, Web
/// ダッシュボード). For a remote stream the honest answer is which node and
/// lease are holding it, so that is what goes in the tuner column.
pub(crate) fn session_info_for_source(
    protocol: SessionProtocol,
    peer: Option<ConnectInfo<SocketAddr>>,
    resolved: &channel_resolve::ResolvedService,
    tuner: Option<&SharedTuner>,
    remote: Option<&crate::node::RemoteMuxStream>,
    stream_class: StreamClass,
) -> HttpStreamSessionInfo {
    let channel = &resolved.channel;
    let (tuner_path, channel_info) = match (tuner, remote) {
        (Some(tuner), _) => (
            Some(tuner.key.tuner_path.clone()),
            match &tuner.key.channel {
                ChannelKeySpec::SpaceChannel { space, channel } => {
                    Some(format!("Space {}, Ch {}", space, channel))
                }
                ChannelKeySpec::Simple(ch) => Some(format!("Ch {}", ch)),
            },
        ),
        (None, Some(remote)) => (
            Some(format!(
                "node:{} ({})",
                remote.lease().owner_node,
                remote.base_url()
            )),
            Some(format!("remote route {}", remote.lease().route_id)),
        ),
        (None, None) => (None, None),
    };

    HttpStreamSessionInfo {
        protocol,
        // `ConnectInfo` is only present when the app is served via
        // `into_make_service_with_connect_info` (it is, in `web/mod.rs`) —
        // the fallback keeps this an optional extractor so a request can
        // never fail with 500 just because the peer address is unavailable,
        // the same treatment the access log gives it.
        addr: peer.map(|ConnectInfo(addr)| addr),
        tuner_path,
        channel_name: channel.channel_name.clone().or_else(|| channel.raw_name.clone()),
        channel_info,
        nid: Some(channel.nid),
        sid: Some(channel.sid),
        stream_class,
    }
}

/// Shared body of the two `stream_service*` handlers once the channel row
/// has been resolved. `sid` is only used for log labels.
async fn stream_resolved(
    web_state: Arc<WebState>,
    resolved: channel_resolve::ResolvedService,
    query: StreamQuery,
    sid: i64,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> Response {
    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &web_state.database, &resolved).await {
        Ok(t) => t,
        Err(e) => return channel_resolve_error_response(sid, &e),
    };

    // Show up in the dashboard's client list for as long as the body lives
    // (`web/http_session.rs`).
    let (session, shutdown_rx) = HttpStreamSession::register(
        Arc::clone(&web_state.session_registry),
        Arc::clone(&web_state.database),
        session_info_for(SessionProtocol::Http, peer, &resolved, &tuner, StreamClass::View),
    )
    .await;

    // Tracked subscription: keeps the tuner counted as "in use" for as long
    // as this HTTP response body is alive, exactly like a BNDP session's
    // `ts_receiver` (STREAMING_DESIGN.md §6.3: "切断で参照カウント減"). This
    // holds even in `?profile=preview` mode, where the *data* actually
    // forwarded to the client comes from the shared encoder's own broadcast
    // instead — see `EncoderPool`'s doc comment on `subscribe_untracked`:
    // the encoder's own subscription to the tuner deliberately does NOT
    // count, so *something* downstream must hold a tracked subscription or
    // the tuner could be idle-closed out from under an active preview
    // viewer. We intentionally never call `.recv()` on this receiver in
    // preview mode (see below) — tokio broadcast receivers that are never
    // polled do not block the sender or other receivers, they simply lag
    // and get skipped, so this is safe and cheap.
    let tuner_rx = tuner.subscribe();

    let profile = query.profile.as_deref().unwrap_or("");

    if profile.is_empty() {
        info!("[HTTP stream] service {} -> raw passthrough (tuner={:?})", sid, tuner.key);
        let cleanup = StreamCleanup::tuner_only(Arc::clone(&tuner), Arc::clone(&web_state.tuner_pool))
            .with_session(session, shutdown_rx);
        return respond_with_stream(broadcast_to_body_stream(BodyReceiver::Tuner(tuner_rx), cleanup));
    }

    if profile != "preview" {
        release_tuner_subscription(&web_state.tuner_pool, tuner_rx).await;
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("unknown profile '{}': only 'preview' is supported", profile),
        );
    }

    let (encoder_cfg, generation) = {
        let db = web_state.database.lock().await;
        match load_preview_encoder_config(&db, PreviewBand::of_nid(resolved.channel.nid)) {
            Ok(v) => v,
            Err(msg) => {
                release_tuner_subscription(&web_state.tuner_pool, tuner_rx).await;
                warn!("[HTTP stream] service {} preview unavailable: {}", sid, msg);
                return error_response(StatusCode::SERVICE_UNAVAILABLE, msg);
            }
        }
    };

    // Single-service stream: encode only this service's SID, unless the
    // profile's own arguments already specify `--service` (same convention
    // `server::session::start_tsreplace_pipeline` uses).
    let sids = if encoder_pool::args_contain_service_option(&encoder_cfg.arguments) {
        Vec::new()
    } else {
        vec![resolved.channel.sid]
    };
    let key = EncodeKey::new(tuner.key.clone(), sids, generation);

    match web_state
        .encoder_pool
        .get_or_create(key.clone(), Arc::clone(&tuner), encoder_cfg)
        .await
    {
        Ok(encoder) => {
            info!(
                "[HTTP stream] service {} -> preview encoder {:?} (subscribers={})",
                sid, key, encoder.subscriber_count()
            );
            let enc_rx = encoder.subscribe();
            let cleanup = StreamCleanup {
                tuner: Some(Arc::clone(&tuner)),
                remote: None,
                tuner_pool: Arc::clone(&web_state.tuner_pool),
                // The actual data comes from `enc_rx` below; this is only
                // here to keep `tuner_rx`'s refcount alive for the response's
                // lifetime (see `StreamCleanup::parked_tuner_sub`'s doc
                // comment) — deliberately never read.
                parked_tuner_sub: Some(tuner_rx),
                encoder: Some(EncoderCleanup {
                    pool: Arc::clone(&web_state.encoder_pool),
                    key,
                    encoder,
                }),
                session: None,
                shutdown_rx: None,
            }
            .with_session(session, shutdown_rx);
            respond_with_stream(broadcast_to_body_stream(BodyReceiver::Encoder(enc_rx), cleanup))
        }
        Err(EncoderPoolError::Saturated) => {
            release_tuner_subscription(&web_state.tuner_pool, tuner_rx).await;
            warn!(
                "[HTTP stream] service {} preview unavailable: encoder pool saturated \
                 (max_concurrent_encoders reached)",
                sid
            );
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "shared encoder pool saturated (max_concurrent_encoders reached); try again later",
            )
        }
        Err(EncoderPoolError::SpawnFailed(e)) => {
            release_tuner_subscription(&web_state.tuner_pool, tuner_rx).await;
            warn!("[HTTP stream] service {} preview encoder spawn failed: {}", sid, e);
            error_response(StatusCode::SERVICE_UNAVAILABLE, e)
        }
    }
}

pub(crate) fn respond_with_stream<S>(body_stream: S) -> Response
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(body_stream))
        .expect("static headers/streaming body are always a valid response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, NewBonDriver};
    use crate::tuner::TunerPool;
    use crate::web::auth::AuthConfig;
    use axum::http::Request;
    use tower::ServiceExt;

    // ------------------------------------------------------------------
    // TsAligner
    // ------------------------------------------------------------------

    /// `n` synthetic TS packets, each starting with the sync byte and
    /// carrying its own index so reordering/truncation is visible.
    fn ts_packets(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n * TS_PACKET_SIZE);
        for i in 0..n {
            out.push(SYNC_BYTE);
            out.extend(std::iter::repeat(i as u8).take(TS_PACKET_SIZE - 1));
        }
        out
    }

    #[test]
    fn aligner_passes_whole_packets_through_unchanged() {
        let mut aligner = TsAligner::new();
        let data = ts_packets(3);
        assert_eq!(aligner.push(&data), Some(data));
    }

    #[test]
    fn aligner_holds_a_partial_packet_until_its_tail_arrives() {
        let data = ts_packets(2);
        let (head, tail) = data.split_at(TS_PACKET_SIZE + 50);

        let mut aligner = TsAligner::new();
        // First chunk: one whole packet plus 50 bytes of the next.
        assert_eq!(aligner.push(head), Some(data[..TS_PACKET_SIZE].to_vec()));
        // Second chunk completes the held packet.
        assert_eq!(aligner.push(tail), Some(data[TS_PACKET_SIZE..].to_vec()));
    }

    #[test]
    fn aligner_returns_nothing_until_a_full_packet_is_available() {
        let mut aligner = TsAligner::new();
        assert_eq!(aligner.push(&ts_packets(1)[..100]), None);
    }

    #[test]
    fn aligner_skips_leading_garbage_to_the_first_sync_byte() {
        let data = ts_packets(1);
        let mut with_garbage = vec![0x00, 0x11, 0x22];
        with_garbage.extend_from_slice(&data);

        let mut aligner = TsAligner::new();
        assert_eq!(aligner.push(&with_garbage), Some(data));
    }

    #[test]
    fn aligner_never_grows_without_bound_on_garbage() {
        let mut aligner = TsAligner::new();
        for _ in 0..100 {
            assert_eq!(aligner.push(&[0x00; 1024]), None);
        }
        assert!(aligner.carry.len() <= TsAligner::MAX_CARRY + 1024);
    }

    /// After a gap the held bytes are the head of a packet whose tail was
    /// lost — keeping them would splice two halves of different packets
    /// together and desynchronize everything downstream.
    #[test]
    fn aligner_drops_the_partial_packet_after_a_gap() {
        let data = ts_packets(2);
        let mut aligner = TsAligner::new();

        assert_eq!(aligner.push(&data[..TS_PACKET_SIZE + 50]), Some(data[..TS_PACKET_SIZE].to_vec()));
        aligner.on_gap();

        let next = ts_packets(1);
        assert_eq!(aligner.push(&next), Some(next));
    }

    fn test_web_state() -> Arc<WebState> {
        let database = Arc::new(tokio::sync::Mutex::new(Database::open_in_memory().unwrap()));
        let tuner_pool = Arc::new(TunerPool::new(4));
        let encoder_pool = Arc::new(EncoderPool::default());
        let session_registry = Arc::new(crate::web::SessionRegistry::new());
        let log_buffer = crate::logging::LogBuffer::new(crate::logging::LOG_BUFFER_CAPACITY);
        let log_level = crate::logging::test_handle();
        let (epg_events_tx, _epg_events_rx) = broadcast::channel(16);
        Arc::new(WebState::new(
            database,
            tuner_pool,
            encoder_pool,
            session_registry,
            AuthConfig { enabled: false, token: String::new() },
            log_buffer,
            std::path::PathBuf::from("logs"),
            log_level,
            epg_events_tx,
        ))
    }

    #[tokio::test]
    async fn dropping_stream_cleanup_releases_tuner_subscription() {
        let tuner = crate::tuner::SharedTuner::new(crate::tuner::ChannelKey::simple("/dev/test", 1), 2);
        let tuner_pool = Arc::new(TunerPool::new(4));
        let sub = tuner.subscribe();
        assert!(tuner.has_subscribers());

        // Exercises the `parked_tuner_sub` path (the `?profile=preview`
        // shape): the subscription lives in `StreamCleanup`, not in a
        // sibling `BodyReceiver::Tuner`.
        let cleanup = StreamCleanup {
            tuner: Some(Arc::clone(&tuner)),
            remote: None,
            tuner_pool: Arc::clone(&tuner_pool),
            parked_tuner_sub: Some(sub),
            encoder: None,
            session: None,
            shutdown_rx: None,
        };
        drop(cleanup);

        // The parked subscription is dropped synchronously inside
        // `StreamCleanup::drop`, so this is already true with no yield
        // needed — asserted immediately to prove that.
        assert!(!tuner.has_subscribers(), "dropping StreamCleanup must release the tracked subscription");
    }

    /// Build a receiver that has already fallen behind, so the next `recv`
    /// returns `Lagged` — the shape a slow client produces in production.
    fn lagged_receiver(packets: usize) -> (broadcast::Sender<Bytes>, broadcast::Receiver<Bytes>) {
        // Capacity 2 so a handful of sends is guaranteed to overrun it.
        let (tx, rx) = broadcast::channel::<Bytes>(2);
        let packet = {
            let mut p = vec![0u8; 188];
            p[0] = 0x47;
            Bytes::from(p)
        };
        for _ in 0..packets {
            let _ = tx.send(packet.clone());
        }
        (tx, rx)
    }

    fn test_cleanup() -> StreamCleanup {
        StreamCleanup::tuner_only(
            crate::tuner::SharedTuner::new(crate::tuner::ChannelKey::simple("/dev/test", 1), 2),
            Arc::new(TunerPool::new(4)),
        )
    }

    /// CLAUDE.md / STREAMING_DESIGN.md §2: a recording must never lose data
    /// silently. A lagging RECORD consumer has to see an error, because a
    /// file with an unannounced hole is only discovered on playback.
    #[tokio::test]
    async fn a_lagging_record_stream_fails_instead_of_skipping_bytes() {
        use futures::StreamExt;

        let (_tx, rx) = lagged_receiver(16);
        let stream = service_filtered_body_stream(
            BodyReceiver::Remote(rx),
            test_cleanup(),
            1024,
            LossPolicy::Fatal,
        );
        futures::pin_mut!(stream);

        let first = stream.next().await.expect("a fatal loss must be reported, not swallowed");
        let err = first.expect_err("the RECORD stream must terminate with an error");
        assert!(
            err.to_string().contains("record stream lost"),
            "the reason must say what happened: {err}"
        );
        assert!(
            stream.next().await.is_none(),
            "the stream must end after the fatal error rather than resuming mid-recording"
        );
    }

    /// A remote node going away (its lease stream ending, the peer being
    /// killed) closes the receiver. For a recording that is a failure, not an
    /// EOF: the response has already been sent as `200 OK`, so ending the
    /// body normally would report a truncated recording as a complete one.
    #[tokio::test]
    async fn a_record_stream_fails_when_the_remote_source_closes() {
        use futures::StreamExt;

        let (tx, rx) = broadcast::channel::<Bytes>(4);
        let cleanup = test_cleanup();
        let stream = service_filtered_body_stream(
            BodyReceiver::Remote(rx),
            cleanup,
            1024,
            LossPolicy::Fatal,
        );
        futures::pin_mut!(stream);

        // The peer disappears without releasing its lease first.
        drop(tx);

        let first = stream.next().await.expect("a closed RECORD source must be reported");
        let err = first.expect_err("the RECORD stream must terminate with an error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("source closed"),
            "the reason must say what happened: {err}"
        );
        assert!(
            stream.next().await.is_none(),
            "the stream must end after the fatal error"
        );
    }

    /// A viewer would rather resynchronize than be disconnected, so the same
    /// event must *not* be fatal for VIEW/PREVIEW.
    #[tokio::test]
    async fn a_lagging_view_stream_resynchronizes_and_keeps_going() {
        use futures::StreamExt;

        let (tx, rx) = lagged_receiver(16);
        let stream = service_filtered_body_stream(
            BodyReceiver::Remote(rx),
            test_cleanup(),
            1024,
            LossPolicy::Recover,
        );
        futures::pin_mut!(stream);

        // Nothing is yielded for the gap itself; the stream simply waits for
        // the next chunk. Dropping the sender ends it cleanly, which proves
        // the `Lagged` did not terminate it with an error.
        drop(tx);
        assert!(
            stream.next().await.is_none(),
            "VIEW must end only when the source closes, never with a lag error"
        );
    }

    #[tokio::test]
    async fn stream_service_returns_404_for_unknown_sid() {
        let state = test_web_state();
        let app = axum::Router::new()
            .route("/api/stream/service/:sid", axum::routing::get(stream_service))
            .with_state(state);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stream/service/999999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stream_service_returns_404_for_unscanned_channel() {
        let state = test_web_state();
        let ch_id = {
            let db = state.database.lock().await;
            let driver_id = db.insert_bon_driver(&NewBonDriver::new("/dev/test-tuner")).unwrap();
            let info = recisdb_protocol::ChannelInfo {
                nid: 1,
                sid: 100,
                tsid: 200,
                manual_sheet: None,
                raw_name: None,
                channel_name: Some("Test".to_string()),
                physical_ch: None,
                remote_control_key: None,
                service_type: None,
                network_name: None,
                bon_space: None, // not yet scanned -> no physical assignment
                bon_channel: None,
                band_type: None,
                terrestrial_region: None,
            };
            db.insert_channel(driver_id, &info).unwrap()
        };

        let app = axum::Router::new()
            .route("/api/stream/service/:sid", axum::routing::get(stream_service))
            .with_state(state);

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/stream/service/{}", ch_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// The preview path must follow `preview_encoder_config` ONLY — the BNDP
    /// `tsreplace_config` is never consulted. Exercised directly on
    /// `load_preview_encoder_config`: the full `stream_service` handler
    /// can't reach this gate in a unit test because tuner startup (which
    /// needs a real BonDriver DLL) happens first. The `preview-h264` encode
    /// profile is seeded by `Database::open_in_memory()`, so the profile
    /// lookup preceding the gate succeeds.
    #[test]
    fn preview_path_reads_preview_encoder_config_only() {
        let db = Database::open_in_memory().unwrap();

        // Scribble garbage into the BNDP-side tsreplace_config (enabled=true
        // with a bogus command) — it must have zero influence on preview.
        db.update_tsreplace_config(true, "bndp-garbage-cmd", "--bndp", 1, true, 2, "bndp-pre", "--bndp-pre")
            .unwrap();

        // preview_encoder_config disabled (default) -> rejected, message
        // names the actual gate.
        let err = load_preview_encoder_config(&db, PreviewBand::Other).unwrap_err();
        assert!(
            err.contains("preview_encoder_config.enabled"),
            "error should name preview_encoder_config.enabled, got: {err}"
        );

        // Enabled but command_path unset -> rejected with the TOML hint.
        db.update_preview_encoder_config(true, "-x 18 -n {SID} -", 7_000).unwrap();
        let err = load_preview_encoder_config(&db, PreviewBand::Other).unwrap_err();
        assert!(
            err.contains("command_path"),
            "error should name the missing command_path, got: {err}"
        );

        // Fully configured -> allowed, and every value comes from the
        // preview table, none from the (garbage) tsreplace_config.
        db.set_preview_command_path("preview-enc").unwrap();
        db.set_preview_preprocessor_path("preview-pre").unwrap();
        let (cfg, _generation) =
            load_preview_encoder_config(&db, PreviewBand::Other).expect("configured preview should pass the gate");
        assert_eq!(cfg.command_path, "preview-enc");
        assert_eq!(cfg.preprocessor_path, "preview-pre");
        assert_eq!(cfg.preprocessor_arguments, "-x 18 -n {SID} -");
        assert_eq!(cfg.read_timeout_ms, 7_000);
    }

    /// BS4K is progressive 2160p: encoding it with the 1080i template
    /// (`--vpp-deinterlace` + no downscale) is what made the 4K preview
    /// break up. 4K channels must get their own template, and every other
    /// band must keep the ordinary one.
    #[test]
    fn preview_picks_the_4k_template_only_for_4k_channels() {
        let db = Database::open_in_memory().unwrap();
        db.update_preview_encoder_config(true, "-x 18 -n {SID} -", 7_000).unwrap();
        db.set_preview_command_path("preview-enc").unwrap();

        let (four_k, _) = load_preview_encoder_config(&db, PreviewBand::FourK).unwrap();
        assert!(
            !four_k.arguments.contains("--vpp-deinterlace"),
            "4K is progressive; the template must not deinterlace: {}",
            four_k.arguments
        );
        assert!(
            four_k.arguments.contains("--output-res"),
            "4K must be downscaled before encoding: {}",
            four_k.arguments
        );

        let (other, _) = load_preview_encoder_config(&db, PreviewBand::Other).unwrap();
        assert!(other.arguments.contains("--vpp-deinterlace"));
    }

    /// NID 0x000B/0x000C are the 4K networks; everything else (BS, CS,
    /// terrestrial) uses the ordinary template.
    #[test]
    fn preview_band_is_classified_from_the_nid() {
        assert_eq!(PreviewBand::of_nid(0x000B), PreviewBand::FourK);
        assert_eq!(PreviewBand::of_nid(0x000C), PreviewBand::FourK);
        assert_eq!(PreviewBand::of_nid(4), PreviewBand::Other);
        assert_eq!(PreviewBand::of_nid(32391), PreviewBand::Other);
    }

    /// Falling back keeps a (stuttering) preview working when the admin has
    /// deleted or disabled the 4K row, rather than failing the request.
    #[test]
    fn a_4k_channel_falls_back_to_the_ordinary_template_when_no_4k_profile_exists() {
        let db = Database::open_in_memory().unwrap();
        db.update_preview_encoder_config(true, "-x 18 -n {SID} -", 7_000).unwrap();
        db.set_preview_command_path("preview-enc").unwrap();
        let four_k_id = db
            .get_encode_profile_by_purpose("preview4k")
            .unwrap()
            .expect("seeded")
            .id;
        db.update_encode_profile(four_k_id, None, None, None, None, None, None, Some(false))
            .unwrap();

        let (cfg, _) = load_preview_encoder_config(&db, PreviewBand::FourK).unwrap();
        assert!(cfg.arguments.contains("--vpp-deinterlace"), "must fall back to the ordinary template");
    }

    // ------------------------------------------------------------------
    // stream_service_by_sid: SID ambiguity across networks (bug fix —
    // SID alone isn't a unique service identity, e.g. BS/BS4K reuse SIDs).
    // ------------------------------------------------------------------

    fn insert_channel_with_nid_sid(db: &Database, driver_name: &str, nid: u16, sid: u16) -> i64 {
        let driver_id = db.insert_bon_driver(&NewBonDriver::new(driver_name)).unwrap();
        let info = recisdb_protocol::ChannelInfo {
            nid,
            sid,
            tsid: 200,
            manual_sheet: None,
            raw_name: None,
            channel_name: Some("Test".to_string()),
            physical_ch: None,
            remote_control_key: None,
            service_type: None,
            network_name: None,
            bon_space: Some(0),
            bon_channel: Some(13),
            band_type: None,
            terrestrial_region: None,
        };
        db.insert_channel(driver_id, &info).unwrap()
    }

    #[tokio::test]
    async fn stream_by_sid_without_nid_is_409_when_sid_spans_multiple_networks() {
        let state = test_web_state();
        {
            let db = state.database.lock().await;
            // Same SID (e.g. 101), two different NIDs — mirrors BS vs BS4K
            // reusing a SID, or a terrestrial SID repeating across regions.
            insert_channel_with_nid_sid(&db, "driver-bs.dll", 4, 101);
            insert_channel_with_nid_sid(&db, "driver-bs4k.dll", 11, 101);
        }

        let app = axum::Router::new()
            .route("/api/stream/service/by-sid/:sid", axum::routing::get(stream_service_by_sid))
            .with_state(state);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stream/service/by-sid/101")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn stream_by_sid_with_nid_disambiguates_and_skips_409() {
        let state = test_web_state();
        {
            let db = state.database.lock().await;
            insert_channel_with_nid_sid(&db, "driver-bs.dll", 4, 101);
            insert_channel_with_nid_sid(&db, "driver-bs4k.dll", 11, 101);
        }

        let app = axum::Router::new()
            .route("/api/stream/service/by-sid/:sid", axum::routing::get(stream_service_by_sid))
            .with_state(state);

        // No real BonDriver DLL exists in this test environment, so tuner
        // startup will fail past resolution — the point of this test is
        // only that supplying `nid=` bypasses the 409 ambiguity gate (i.e.
        // the response must NOT be 409; resolution proceeds to the
        // (unrelated) tuner-start failure instead).
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stream/service/by-sid/101?nid=4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(res.status(), StatusCode::CONFLICT, "nid= must disambiguate instead of 409ing");
    }

    #[tokio::test]
    async fn stream_by_sid_without_nid_is_not_ambiguous_when_sid_is_on_one_network() {
        let state = test_web_state();
        {
            let db = state.database.lock().await;
            // Two rows, but both on the same NID (e.g. scanned from two
            // BonDrivers carrying the same multiplex) — not ambiguous.
            insert_channel_with_nid_sid(&db, "driver-a.dll", 4, 101);
            insert_channel_with_nid_sid(&db, "driver-b.dll", 4, 101);
        }

        let app = axum::Router::new()
            .route("/api/stream/service/by-sid/:sid", axum::routing::get(stream_service_by_sid))
            .with_state(state);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stream/service/by-sid/101")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(res.status(), StatusCode::CONFLICT, "a single-network SID must not be treated as ambiguous");
    }
}
