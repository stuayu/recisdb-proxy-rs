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

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::stream::{self, Stream};
use log::{debug, info, warn};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::database::Database;
use crate::server::channel_resolve::{self, ChannelResolveError};
use crate::tuner::encoder_pool::{self, EncodeKey, EncoderPoolError, EncoderRuntimeConfig, SharedEncoder};
use crate::tuner::{EncoderPool, SharedTuner, TunerPool};
use crate::web::state::WebState;

/// Query parameters for `GET /api/stream/service/:sid`.
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// `"preview"` selects the shared H.264 encoder pipeline. Absent/empty
    /// means raw passthrough.
    pub profile: Option<String>,
}

/// Cleanup for a live shared-encoder subscription, released together with
/// the tuner subscription in [`StreamCleanup::drop`].
struct EncoderCleanup {
    pool: Arc<EncoderPool>,
    key: EncodeKey,
    encoder: Arc<SharedEncoder>,
}

/// RAII guard tying an HTTP response body's lifetime to a tuner subscription
/// (and, for `?profile=preview`, a shared-encoder subscription).
///
/// The body stream (see [`broadcast_to_body_stream`]) owns exactly one of
/// these per request. Whenever axum/hyper drops that stream — client
/// disconnect, the connection resetting, or (never, in practice, since these
/// are unbounded live broadcasts) the stream ending on its own — `Drop` runs
/// and releases both subscriptions, mirroring what `server::session::Session`
/// does explicitly on every exit path (`tuner.unsubscribe()` +
/// `tuner_pool.schedule_idle_close(..)`, and `encoder_pool.release(..)` from
/// `stop_tsreplace_pipeline`). There is no synchronous "session loop exiting"
/// hook to hang that cleanup off here — the stream's lifetime *is* the
/// subscription's lifetime — so `Drop` spawns a short detached task to do the
/// (necessarily `async`) release work. This is the one part of P5 that
/// cannot be exercised by an integration test in this environment (no real
/// client to disconnect mid-stream); see the unit test below for the
/// closest available proxy: dropping the guard decrements
/// `SharedTuner`'s subscriber_count.
pub(crate) struct StreamCleanup {
    tuner: Arc<SharedTuner>,
    tuner_pool: Arc<TunerPool>,
    encoder: Option<EncoderCleanup>,
}

impl StreamCleanup {
    /// Build a cleanup guard for a plain tuner subscription with no shared
    /// encoder involved — what every Mirakurun-compatible passthrough stream
    /// uses (`web/mirakurun.rs`, STREAMING_DESIGN.md §7.1: "passthrough
    /// (無変換) が既定").
    pub(crate) fn tuner_only(tuner: Arc<SharedTuner>, tuner_pool: Arc<TunerPool>) -> Self {
        Self { tuner, tuner_pool, encoder: None }
    }
}

impl Drop for StreamCleanup {
    fn drop(&mut self) {
        let tuner = Arc::clone(&self.tuner);
        let tuner_pool = Arc::clone(&self.tuner_pool);
        let encoder = self.encoder.take();
        tokio::spawn(async move {
            tuner.unsubscribe();
            if !tuner.has_subscribers() {
                tuner_pool
                    .schedule_idle_close(tuner.key.clone(), Arc::clone(&tuner))
                    .await;
            }
            if let Some(EncoderCleanup { pool, key, encoder }) = encoder {
                pool.release(&key, &encoder).await;
            }
        });
    }
}

/// State owned by the `stream::unfold` powering the response body: the
/// broadcast receiver being forwarded, plus the cleanup guard that must
/// outlive every yielded chunk and only run once the stream itself is
/// dropped.
struct StreamState {
    rx: broadcast::Receiver<Bytes>,
    _cleanup: StreamCleanup,
}

/// Adapt a `broadcast::Receiver<Bytes>` into a `Stream` suitable for
/// `axum::body::Body::from_stream`.
///
/// `Lagged` is treated the same way `server/session.rs` treats it for
/// VIEW/PREVIEW classes (STREAMING_DESIGN.md §3.2/§3.3): logged and skipped,
/// not fatal — a lagging HTTP viewer keeps receiving the *current* live
/// edge rather than being disconnected. `Closed` (source tuner reader
/// stopped, or shared encoder chain stopped) ends the HTTP response body
/// normally, which the browser/mpegts.js/ffmpeg observes as EOF.
pub(crate) fn broadcast_to_body_stream(
    rx: broadcast::Receiver<Bytes>,
    cleanup: StreamCleanup,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    stream::unfold(StreamState { rx, _cleanup: cleanup }, |mut state| async move {
        loop {
            match state.rx.recv().await {
                Ok(data) => return Some((Ok(data), state)),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("[HTTP stream] receiver lagged, skipped {} chunks", n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
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
/// before handing the receiver to [`broadcast_to_body_stream`]).
pub(crate) async fn release_tuner_subscription(tuner_pool: &Arc<TunerPool>, tuner: &Arc<SharedTuner>) {
    tuner.unsubscribe();
    if !tuner.has_subscribers() {
        tuner_pool
            .schedule_idle_close(tuner.key.clone(), Arc::clone(tuner))
            .await;
    }
}

/// Load the runtime encoder settings for `?profile=preview`: codec/bitrate/
/// arguments from the `encode_profiles` row with `purpose='preview'`
/// (STREAMING_DESIGN.md §5.3), everything else from `preview_encoder_config`
/// — the browser-preview pipeline's own table, fully separate from the BNDP
/// (TVTest) `tsreplace_config` which is never consulted here. Both
/// executable paths are TOML-only (`[preview]` section, REVIEW S1).
fn load_preview_encoder_config(db: &Database) -> Result<(EncoderRuntimeConfig, u64), String> {
    let profile = db
        .get_encode_profile_by_purpose("preview")
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no enabled encode profile with purpose='preview' is configured".to_string())?;

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
    stream_resolved(web_state, resolved, query, sid).await
}

/// `GET /api/stream/service/by-sid/:sid[?profile=preview]` — same streaming
/// behaviour but `:sid` is the real broadcast service_id (what the UI shows
/// as SID everywhere).
pub async fn stream_service_by_sid(
    State(web_state): State<Arc<WebState>>,
    Path(sid): Path<u16>,
    Query(query): Query<StreamQuery>,
) -> Response {
    let resolved = {
        let db = web_state.database.lock().await;
        channel_resolve::resolve_service_by_sid(&db, sid)
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => return channel_resolve_error_response(sid as i64, &e),
    };
    stream_resolved(web_state, resolved, query, sid as i64).await
}

/// Shared body of the two `stream_service*` handlers once the channel row
/// has been resolved. `sid` is only used for log labels.
async fn stream_resolved(
    web_state: Arc<WebState>,
    resolved: channel_resolve::ResolvedService,
    query: StreamQuery,
    sid: i64,
) -> Response {
    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &resolved).await {
        Ok(t) => t,
        Err(e) => return channel_resolve_error_response(sid, &e),
    };

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
        info!("[HTTP stream] service {} -> raw passthrough (tuner={:?})", sid, resolved.channel_key);
        let cleanup = StreamCleanup {
            tuner: Arc::clone(&tuner),
            tuner_pool: Arc::clone(&web_state.tuner_pool),
            encoder: None,
        };
        return respond_with_stream(broadcast_to_body_stream(tuner_rx, cleanup));
    }

    if profile != "preview" {
        release_tuner_subscription(&web_state.tuner_pool, &tuner).await;
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("unknown profile '{}': only 'preview' is supported", profile),
        );
    }

    let (encoder_cfg, generation) = {
        let db = web_state.database.lock().await;
        match load_preview_encoder_config(&db) {
            Ok(v) => v,
            Err(msg) => {
                release_tuner_subscription(&web_state.tuner_pool, &tuner).await;
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
    let key = EncodeKey::new(resolved.channel_key.clone(), sids, generation);

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
                tuner: Arc::clone(&tuner),
                tuner_pool: Arc::clone(&web_state.tuner_pool),
                encoder: Some(EncoderCleanup {
                    pool: Arc::clone(&web_state.encoder_pool),
                    key,
                    encoder,
                }),
            };
            respond_with_stream(broadcast_to_body_stream(enc_rx, cleanup))
        }
        Err(EncoderPoolError::Saturated) => {
            release_tuner_subscription(&web_state.tuner_pool, &tuner).await;
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
            release_tuner_subscription(&web_state.tuner_pool, &tuner).await;
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

    fn test_web_state() -> Arc<WebState> {
        let database = Arc::new(tokio::sync::Mutex::new(Database::open_in_memory().unwrap()));
        let tuner_pool = Arc::new(TunerPool::new(4));
        let encoder_pool = Arc::new(EncoderPool::default());
        let session_registry = Arc::new(crate::web::SessionRegistry::new());
        let log_buffer = crate::logging::LogBuffer::new(crate::logging::LOG_BUFFER_CAPACITY);
        Arc::new(WebState::new(
            database,
            tuner_pool,
            encoder_pool,
            session_registry,
            AuthConfig { enabled: false, token: String::new() },
            log_buffer,
            std::path::PathBuf::from("logs"),
        ))
    }

    #[tokio::test]
    async fn dropping_stream_cleanup_releases_tuner_subscription() {
        let tuner = crate::tuner::SharedTuner::new(crate::tuner::ChannelKey::simple("/dev/test", 1), 2);
        let tuner_pool = Arc::new(TunerPool::new(4));
        let _rx = tuner.subscribe();
        assert!(tuner.has_subscribers());

        let cleanup = StreamCleanup {
            tuner: Arc::clone(&tuner),
            tuner_pool: Arc::clone(&tuner_pool),
            encoder: None,
        };
        drop(cleanup);

        // Drop spawns a detached task to do the actual unsubscribe; give it
        // a chance to run.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(!tuner.has_subscribers(), "dropping StreamCleanup must release the tracked subscription");
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
        let err = load_preview_encoder_config(&db).unwrap_err();
        assert!(
            err.contains("preview_encoder_config.enabled"),
            "error should name preview_encoder_config.enabled, got: {err}"
        );

        // Enabled but command_path unset -> rejected with the TOML hint.
        db.update_preview_encoder_config(true, "-x 18 -n {SID} -", 7_000).unwrap();
        let err = load_preview_encoder_config(&db).unwrap_err();
        assert!(
            err.contains("command_path"),
            "error should name the missing command_path, got: {err}"
        );

        // Fully configured -> allowed, and every value comes from the
        // preview table, none from the (garbage) tsreplace_config.
        db.set_preview_command_path("preview-enc").unwrap();
        db.set_preview_preprocessor_path("preview-pre").unwrap();
        let (cfg, _generation) =
            load_preview_encoder_config(&db).expect("configured preview should pass the gate");
        assert_eq!(cfg.command_path, "preview-enc");
        assert_eq!(cfg.preprocessor_path, "preview-pre");
        assert_eq!(cfg.preprocessor_arguments, "-x 18 -n {SID} -");
        assert_eq!(cfg.read_timeout_ms, 7_000);
    }
}
