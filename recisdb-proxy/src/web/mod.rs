//! Web dashboard server for monitoring and configuration.

pub mod api;
pub mod auth;
pub mod channel_files;
pub mod dashboard;
pub mod mirakurun;
pub mod mirakurun_docs;
mod mirakurun_events;
mod mirakurun_program_stream;
pub mod state;
pub mod stream;

use axum::{
    Router,
    extract::{ConnectInfo, Request},
    middleware::Next,
    response::Response,
    routing::{delete, get, post},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::logging::LogBuffer;
use crate::server::listener::DatabaseHandle;
use crate::tuner::{EncoderPool, TunerPool};
use auth::AuthConfig;
use state::WebState;

pub use state::{SessionInfo, SessionRegistry};

/// Build the `/api/*` router (auth middleware attached, no state bound yet).
///
/// Split out from [`build_app`] so tests can exercise the auth-gated routes
/// without needing to serve the full dashboard HTML.
fn build_api_router() -> Router<Arc<WebState>> {
    Router::new()
        // Legacy API routes (for backwards compatibility)
        .route("/tuners", get(api::get_tuners))
        .route("/config", get(api::get_config))
        .route("/config", post(api::update_config))
        .route("/version", get(api::get_version))
        // Update notification / self-update API (web/api/update.rs)
        .route("/update/check", get(api::check_update))
        .route("/update/apply", post(api::apply_update))
        .route("/update/status", get(api::update_status))

        // OSサービス連携 (service/mod.rs)。登録/削除は権限昇格の経路に
        // なるため Web からは行わない (CLI とセットアップGUIのみ)。
        .route("/service/status", get(api::get_service_status))
        .route("/service/restart", post(api::restart_service))
        // Log viewer API (web/api/logs.rs)
        .route("/logs", get(api::get_logs))
        .route("/logs/files", get(api::list_log_files))
        .route("/logs/files/:name", get(api::download_log_file))
        // Session/Client API
        .route("/clients", get(api::get_clients))
        .route("/stats", get(api::get_stats))
        .route("/events", get(api::dashboard_events))
        .route("/client/:id/quality", get(api::get_client_quality))
        .route("/client/:id/metrics-history", get(api::get_client_metrics_history))
        .route("/client/:id/disconnect", post(api::disconnect_client))
        .route("/client/:id/controls", post(api::override_client_controls))
        .route("/session-history", get(api::get_session_history))
        // BonDriver API
        .route("/bondrivers", get(api::get_bondrivers))
        .route("/bondriver", post(api::create_bondriver))
        .route("/bondriver/:id", get(api::get_bondriver))
        .route("/bondriver/:id", post(api::update_bondriver))
        .route("/bondriver/:id", delete(api::delete_bondriver))
        .route("/bondriver/:id/scan", post(api::trigger_scan))
        .route("/bondriver/:id/quality", get(api::get_bondriver_quality))
        .route("/bondrivers/ranking", get(api::get_bondrivers_ranking))
        // Channel API
        .route("/client-view/targets", get(api::get_client_view_targets))
        .route("/client-view", get(api::get_client_view))
        .route("/client-view/files/:kind", get(api::get_client_view_file))
        .route("/channels", get(api::get_channels))
        .route("/channels/export", get(api::export_channels))
        .route("/channels/import", post(api::import_channels))
        .route("/channels/batch", post(api::batch_update_channels))
        .route("/channel", post(api::create_channel))
        .route("/channel/:id", post(api::update_channel))
        .route("/channel/:id/toggle", post(api::toggle_channel))
        .route("/channel/:id", delete(api::delete_channel))
        // Scan history API
        .route("/scan-history", get(api::get_scan_history))
        // EPG (program guide) API
        .route("/programs", get(api::get_programs))
        // Alert API
        .route("/alerts", get(api::get_alerts))
        .route("/alert-rules", get(api::get_alert_rules))
        .route("/alert-rules", post(api::create_alert_rule))
        .route("/alert-rules/:id", delete(api::delete_alert_rule))
        .route("/alerts/:id/acknowledge", post(api::acknowledge_alert))
        // Scan scheduler configuration API
        .route("/scan-config", get(api::get_scan_config))
        .route("/scan-config", post(api::update_scan_config))
        // Tuner optimization configuration API
        .route("/tuner-config", get(api::get_tuner_config))
        .route("/tuner-config", post(api::update_tuner_config))
        // External encoder (tsreplace) configuration API — BNDP sessions only
        .route("/tsreplace-config", get(api::get_tsreplace_config))
        .route("/tsreplace-config", post(api::update_tsreplace_config))
        // Browser-preview encoder configuration API (`preview_encoder_config`,
        // fully separate from tsreplace-config)
        .route("/card-reader", get(api::get_card_readers))
        .route("/card-reader", post(api::update_card_reader))
        .route("/preview-config", get(api::get_preview_config))
        .route("/preview-config", post(api::update_preview_config))
        .route("/preview-config/auto-setup", post(api::auto_setup_preview))
        // Encode profile catalogue API (STREAMING_DESIGN.md §5.3/§9 P5)
        .route("/encode-profiles", get(api::get_encode_profiles))
        .route("/encode-profiles", post(api::create_encode_profile))
        .route("/encode-profiles/:id", post(api::update_encode_profile))
        .route("/encode-profiles/:id", delete(api::delete_encode_profile))
        // HTTP-TS streaming endpoints (STREAMING_DESIGN.md §6.3/§7.2).
        // Auth is applied the same way as every other route here (see
        // `build_app`'s `route_layer(...require_auth)`) — §6.5.
        .route("/stream/service/:sid", get(stream::stream_service))
        // Same stream, keyed by the real broadcast service_id (dashboard UI).
        .route("/stream/service/by-sid/:sid", get(stream::stream_service_by_sid))
}

/// Build the `/mirakurun/api/*` router (STREAMING_DESIGN.md §7.1, P6).
///
/// **No auth middleware is applied here** — see `web/mirakurun.rs`'s module
/// doc comment for why (real Mirakurun clients never send an Authorization
/// header). Only nested into the app when `[mirakurun] enabled = true`
/// (`main.rs`, default `false`).
fn build_mirakurun_router() -> Router<Arc<WebState>> {
    Router::new()
        // `/docs` — must come first conceptually (not order-sensitive for
        // axum's router, but see mirakurun_docs.rs module doc comment for
        // why every other route below is unreachable to a real `mirakurun`
        // client until this one works).
        .route("/docs", get(mirakurun_docs::get_docs))
        .route("/version", get(mirakurun::get_version))
        .route("/status", get(mirakurun::get_status))
        .route("/channels", get(mirakurun::get_channels))
        .route("/services", get(mirakurun::get_services))
        .route("/programs", get(mirakurun::get_programs))
        .route("/tuners", get(mirakurun::get_tuners))
        .route("/config/server", get(mirakurun::get_server_config))
        .route("/services/:id/stream", get(mirakurun::stream_service_by_mirakurun_id))
        .route("/services/:id/logo", get(mirakurun::get_logo_stub))
        .route("/channels/:type/:channel/stream", get(mirakurun::stream_channel_by_type))
        .route("/programs/:id/stream", get(mirakurun::stream_program_by_mirakurun_id))
        // Incremental EPG update stream — see `mirakurun_events.rs` module
        // doc comment for the wire format EPGStation requires.
        .route("/events/stream", get(mirakurun_events::stream_events))
}

/// HTTP access-log middleware, layered over the whole router in
/// [`build_app`].
///
/// Emits exactly one `INFO` line per request *after* the handler produced a
/// response: remote address, method, path (including the query string),
/// status code, and elapsed time in milliseconds.
///
/// Notes:
/// - The remote address comes from [`ConnectInfo`], which is only present
///   when the app is served via `into_make_service_with_connect_info` (see
///   [`start_web_server`]). In `oneshot` tests it is absent, so it is read
///   as an `Option` and logged as `-`.
/// - For streaming endpoints (`/api/stream/service/:sid` etc.)
///   `next.run(req)` returns as soon as the response *headers* are ready,
///   so this logs the moment the stream was accepted, not its total
///   duration — the middleware never blocks on the body.
/// - Header values are deliberately NOT logged: `Authorization` carries the
///   API bearer token. The query string is safe to log — auth is
///   header-only (see `web/auth.rs`), no token ever travels in the URL.
async fn access_log(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.to_string());
    let start = Instant::now();

    let response = next.run(request).await;

    log::info!(
        "{} \"{} {}\" {} {}ms",
        remote.as_deref().unwrap_or("-"),
        method,
        path_and_query,
        response.status().as_u16(),
        start.elapsed().as_millis(),
    );
    response
}

/// Build the full application router bound to `web_state`.
///
/// # Security (REVIEW_2026-07.md S2)
/// - `/api/*` is wrapped with [`auth::require_auth`], requiring
///   `Authorization: Bearer <token>` unless `web_state.auth.enabled` is
///   false. `route_layer` applies the middleware only to routes registered
///   on `build_api_router()`, so `GET /` and `/logos/:file` stay reachable
///   without a token (needed so the browser can load the token-entry UI).
/// - No CORS layer: the dashboard is served same-origin, so no
///   `Access-Control-Allow-Origin` header is ever sent and cross-origin
///   `fetch()` calls (e.g. a malicious third-party page, or CSRF-style
///   browser requests) are refused by the browser itself. Previously this
///   used `CorsLayer::permissive()`, which defeated that protection.
/// - `/mirakurun/api/*` (STREAMING_DESIGN.md §7.1, P6) is a *separate*,
///   unauthenticated router, only nested in when `mirakurun_enabled` is
///   true. It is its own namespace precisely so it never shares a path with
///   (and is never accidentally covered by the auth `route_layer` bound to)
///   `/api/*` — see `build_mirakurun_router` and `web/mirakurun.rs`.
fn build_app(web_state: Arc<WebState>, mirakurun_enabled: bool) -> Router {
    let api_router = build_api_router()
        .route_layer(axum::middleware::from_fn_with_state(web_state.clone(), auth::require_auth));

    let mut router = Router::new()
        .nest("/api", api_router)
        // Dashboard route (unauthenticated: serves the token-entry UI)
        .route("/", get(dashboard::index))
        .route("/logos/:file", get(api::get_logo))
        .route("/static/vue/*path", get(api::get_vue_asset));

    if mirakurun_enabled {
        router = router.nest("/mirakurun/api", build_mirakurun_router());
    }

    router
        .with_state(web_state)
        // Access log covers every route above (dashboard, /api/*, and the
        // Mirakurun router when enabled).
        .layer(axum::middleware::from_fn(access_log))
}

/// Start the web dashboard server.
///
/// `mirakurun_enabled` gates the entire `/mirakurun/api/*` router
/// (STREAMING_DESIGN.md §7.1, P6): when `false` (the default — see
/// `[mirakurun] enabled` in `main.rs`), that path prefix is not registered at
/// all and returns 404, same as any other unmapped path. When `true`, a WARN
/// is logged once at startup: unlike `/api/*`, that router carries no
/// authentication (see `web/mirakurun.rs`'s module doc comment for why).
#[allow(clippy::too_many_arguments)]
pub async fn start_web_server(
    listen_addr: SocketAddr,
    database: DatabaseHandle,
    tuner_pool: Arc<TunerPool>,
    encoder_pool: Arc<EncoderPool>,
    session_registry: Arc<SessionRegistry>,
    scan_config: Option<state::ScanSchedulerInfo>,
    tuner_config: Option<state::TunerConfigInfo>,
    auth: AuthConfig,
    log_buffer: Arc<LogBuffer>,
    log_dir: PathBuf,
    mirakurun_enabled: bool,
    proxy_listen_addr: Option<SocketAddr>,
    config_path: Option<PathBuf>,
    epg_events_tx: tokio::sync::broadcast::Sender<crate::database::ProgramUpsert>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut web_state = WebState::new(
        database,
        tuner_pool,
        encoder_pool,
        session_registry,
        auth,
        log_buffer,
        log_dir,
        epg_events_tx,
    );
    web_state.proxy_listen_addr = proxy_listen_addr;
    web_state.config_path = config_path;
    if let Some(config) = scan_config {
        *web_state.scan_config.write().await = config;
    }
    if let Some(config) = tuner_config {
        *web_state.tuner_config.write().await = config;
    }
    let web_state = Arc::new(web_state);

    if mirakurun_enabled {
        log::warn!(
            "Mirakurun-compatible API is ENABLED at /mirakurun/api/* ([mirakurun] enabled = true). \
             This endpoint is UNAUTHENTICATED by design (real Mirakurun clients send no Authorization \
             header) — expose it only on a trusted network/localhost."
        );
    }

    let app = build_app(web_state, mirakurun_enabled);

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    log::info!("Web dashboard listening on http://{}", listen_addr);

    // `with_connect_info` makes the client's SocketAddr available to the
    // access-log middleware (see `access_log`) via `ConnectInfo`.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(web_shutdown_signal())
        .await?;

    Ok(())
}

/// Stop accepting new HTTP connections when the process receives a normal
/// termination signal. Existing responses are allowed to finish.
async fn web_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    fn test_web_state(auth: AuthConfig) -> Arc<WebState> {
        let database: DatabaseHandle = Arc::new(tokio::sync::Mutex::new(
            crate::database::Database::open_in_memory().expect("open in-memory db"),
        ));
        let tuner_pool = Arc::new(TunerPool::new(4));
        let encoder_pool = Arc::new(EncoderPool::default());
        let session_registry = Arc::new(SessionRegistry::new());
        let log_buffer = crate::logging::LogBuffer::new(crate::logging::LOG_BUFFER_CAPACITY);
        let (epg_events_tx, _epg_events_rx) = tokio::sync::broadcast::channel(16);
        Arc::new(WebState::new(
            database,
            tuner_pool,
            encoder_pool,
            session_registry,
            auth,
            log_buffer,
            std::path::PathBuf::from("logs"),
            epg_events_tx,
        ))
    }

    /// REVIEW S1 for `/api/preview-config`: the two executable paths are
    /// TOML-only. Sending them in the POST body must be silently ignored
    /// (serde drops unknown fields), while the legitimate fields
    /// (`enabled` / `preprocessor_arguments` / `read_timeout_ms`) apply.
    #[tokio::test]
    async fn preview_config_api_ignores_executable_paths() {
        let state = test_web_state(AuthConfig { enabled: false, token: String::new() });
        {
            // Paths arrive via the TOML-only setters (main.rs `[preview]`).
            let db = state.database.lock().await;
            db.set_preview_command_path("C:/toml/enc.exe").unwrap();
            db.set_preview_preprocessor_path("C:/toml/pre.exe").unwrap();
        }
        let app = build_app(Arc::clone(&state), false);

        let body = serde_json::json!({
            "enabled": true,
            "preprocessor_arguments": "-x 18 -n {SID} -",
            "read_timeout_ms": 5000,
            // Injection attempts — must be ignored:
            "command_path": "C:/evil/enc.exe",
            "preprocessor_path": "C:/evil/pre.exe",
        });
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/preview-config")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(Request::builder().uri("/api/preview-config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let cfg = &json["config"];
        assert_eq!(cfg["command_path"], "C:/toml/enc.exe", "TOML-only path must be untouched");
        assert_eq!(cfg["preprocessor_path"], "C:/toml/pre.exe", "TOML-only path must be untouched");
        assert_eq!(cfg["enabled"], true);
        assert_eq!(cfg["preprocessor_arguments"], "-x 18 -n {SID} -");
        assert_eq!(cfg["read_timeout_ms"], 5000);
    }

    #[tokio::test]
    async fn api_request_without_token_is_rejected() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_request_with_wrong_token_is_rejected() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_request_with_correct_token_is_accepted() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .header(header::AUTHORIZATION, "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    /// The client-setup guide must show exactly what a BonDriver client will
    /// enumerate: group targets first, then the virtual space/channel table
    /// with client-facing indices and physical mappings.
    #[tokio::test]
    async fn client_view_reports_what_a_client_will_enumerate() {
        let state = test_web_state(AuthConfig { enabled: false, token: String::new() });
        {
            let db = state.database.lock().await;
            let d1 = db
                .insert_bon_driver(&crate::database::NewBonDriver::new("BonDriver_A.dll"))
                .unwrap();
            let d2 = db
                .insert_bon_driver(&crate::database::NewBonDriver::new("BonDriver_B.dll"))
                .unwrap();
            db.set_group_name(d1, Some("PX")).unwrap();
            db.set_group_name(d2, Some("PX")).unwrap();

            let mk = |nid: u16, sid: u16, tsid: u16, name: &str, space: u32, channel: u32| {
                recisdb_protocol::ChannelInfo {
                    nid,
                    sid,
                    tsid,
                    manual_sheet: None,
                    raw_name: Some(name.to_string()),
                    channel_name: Some(name.to_string()),
                    physical_ch: None,
                    remote_control_key: None,
                    service_type: None,
                    network_name: None,
                    bon_space: Some(space),
                    bon_channel: Some(channel),
                    band_type: None,
                    terrestrial_region: None,
                }
            };
            // Same logical channel (NID+TSID) on both drivers with different
            // physical bon_channel values, plus one BS channel on driver A.
            db.insert_channel(d1, &mk(0x7FE8, 1024, 0x7FE8, "NHK総合", 0, 27)).unwrap();
            db.insert_channel(d2, &mk(0x7FE8, 1024, 0x7FE8, "NHK総合", 0, 5)).unwrap();
            db.insert_channel(d1, &mk(4, 101, 0x4010, "BS朝日", 1, 0)).unwrap();
        }
        let app = build_app(state, false);

        // Targets: the group comes first (recommended), then the drivers.
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/client-view/targets").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], true);
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets[0]["type"], "group");
        assert_eq!(targets[0]["name"], "PX");
        // Distinct (NID, TSID) across the group — the shared NHK channel on
        // both drivers counts once, matching what STEP 3 enumerates.
        assert_eq!(targets[0]["enabled_channels"], 2);
        assert!(targets.iter().any(|t| t["type"] == "driver" && t["name"] == "BonDriver_A.dll"));

        // Client view for the group: terrestrial space first, then BS, with
        // 0-based client-facing indices and both physical mappings for the
        // shared logical channel.
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/client-view?tuner=PX").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], true, "body: {json}");
        assert_eq!(json["resolved_type"], "group");
        let spaces = json["spaces"].as_array().unwrap();
        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0]["index"], 0);
        assert_eq!(spaces[0]["name"], "地デジ (関東)");
        let channels = spaces[0]["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0]["index"], 0);
        assert_eq!(channels[0]["name"], "NHK総合");
        assert_eq!(channels[0]["physical"].as_array().unwrap().len(), 2);
        assert_eq!(spaces[1]["name"], "BS");

        // A single driver resolves too, but sees only its own channels.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/client-view?tuner=BonDriver_B.dll")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["spaces"].as_array().unwrap().len(), 1, "driver B has no BS channel");

        // Unknown tuner names are an explicit error, never someone else's
        // channel list.
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/client-view?tuner=nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], false);

        // Channel-file downloads: the ch2 uses the same space/channel
        // indices as the enumeration above, encoded in Shift_JIS.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/client-view/files/tvtest-ch2?tuner=PX")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let disposition = res
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.contains("BonDriver_NetworkProxy.ch2"), "{disposition}");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let (decoded, _, _) = encoding_rs::SHIFT_JIS.decode(&body);
        assert!(decoded.contains(";#SPACE(0,地デジ （関東）)"), "{decoded}");
        // NHK on terrestrial space 0 channel 0, enabled.
        assert!(decoded.contains("NHK総合,0,0,0,,1024,32744,32744,1"), "{decoded}");
        // BS asahi in space 1.
        assert!(decoded.contains(";#SPACE(1,BS)"), "{decoded}");

        // The zip bundle contains INI (with Host-derived address), channel
        // files, and README.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/client-view/files/bundle?tuner=PX")
                    .header(header::HOST, "192.168.10.20:40080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body.to_vec())).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        for expected in [
            "BonDriver_NetworkProxy.ini",
            "BonDriver_NetworkProxy.ch2",
            "BonDriver_NetworkProxy(BonDriver_NetworkProxy).ChSet4.txt",
            "ChSet5.txt",
            "README.txt",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected} in {names:?}");
        }
        let mut ini = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("BonDriver_NetworkProxy.ini").unwrap(), &mut ini)
            .unwrap();
        assert!(ini.contains("Tuner = PX"), "{ini}");
        // Host header host + default proxy port (proxy_listen_addr is None in tests).
        assert!(ini.contains("Address = 192.168.10.20:40070"), "{ini}");

        // Unknown kind → 404.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/client-view/files/nonsense?tuner=PX")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn auth_disabled_bypasses_token_check() {
        let state = test_web_state(AuthConfig { enabled: false, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn stream_endpoint_requires_auth_like_every_other_api_route() {
        // STREAMING_DESIGN.md §6.5: the streaming endpoint must sit behind
        // the exact same auth gate as the rest of `/api/*`.
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/stream/service/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn dashboard_root_is_reachable_without_token() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn version_api_requires_auth_and_reports_crate_version() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        // Same auth gate as every other `/api/*` route.
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/version").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/version")
                    .header(header::AUTHORIZATION, "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["version"], crate::VERSION);
    }

    #[tokio::test]
    async fn mirakurun_router_is_not_mounted_when_disabled() {
        // STREAMING_DESIGN.md §7.1 (P6): opt-in, default disabled — the
        // whole `/mirakurun/api/*` prefix must be unreachable (404, not
        // "reachable but empty") when the router was never nested in.
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/mirakurun/api/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mirakurun_version_is_reachable_without_auth_when_enabled() {
        // Auth is enabled for `/api/*` here specifically to prove this test
        // isn't just exercising a globally-disabled auth config — the
        // `/mirakurun/api/*` router must bypass auth on its own.
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, true);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/mirakurun/api/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("current").is_some(), "expected a `current` field: {:?}", json);
    }

    #[tokio::test]
    async fn mirakurun_services_and_channels_are_reachable_without_auth_when_enabled() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, true);

        for path in ["/mirakurun/api/services", "/mirakurun/api/channels", "/mirakurun/api/status"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "GET {} should be 200", path);
        }
    }

    /// EPGStation-compat pass 1: `/docs`, `/tuners`, `/config/server` must be
    /// reachable and unauthenticated like the rest of the Mirakurun router
    /// (`docs/EPGSTATION_COMPAT.md` §1/§3/§6). `/docs` additionally must
    /// come back as `application/json` — see `mirakurun_docs.rs` module doc
    /// comment for why the client silently breaks otherwise — and its
    /// `paths` must resolve the id EPGStation's `mirakurun` client uses for
    /// `GET /services/:id/stream` to the exact route this router mounts, so
    /// a real client's resolved request actually lands somewhere.
    #[tokio::test]
    async fn mirakurun_docs_tuners_and_config_server_are_reachable_without_auth_when_enabled() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, true);

        for path in ["/mirakurun/api/tuners", "/mirakurun/api/config/server"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "GET {} should be 200", path);
        }

        let res = app
            .clone()
            .oneshot(Request::builder().uri("/mirakurun/api/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let content_type = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap().to_string();
        assert!(content_type.starts_with("application/json"), "{content_type}");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let docs: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let stream_path = &docs["paths"]["/services/{id}/stream"];
        assert_eq!(stream_path["get"]["operationId"], "getServiceStream");

        // `/events/stream` is now implemented (`mirakurun_events.rs`): it
        // must answer 200 with a JSON content type and never touch the body
        // — the body is an intentionally-unbounded stream (real Mirakurun
        // clients keep this connection open indefinitely), so awaiting it
        // to completion here would hang the test forever. `oneshot` already
        // returns as soon as the response *headers* are ready (same
        // property `access_log`'s doc comment relies on), so checking only
        // status/headers and dropping the response (which drops the body
        // stream) is both correct and sufficient.
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/mirakurun/api/events/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "GET /events/stream should be 200");
        let content_type = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap().to_string();
        assert!(content_type.starts_with("application/json"), "{content_type}");

        // `/programs/:id/stream` is now implemented — with no matching
        // program in the (empty, in-memory) test DB, it must resolve
        // routing-wise (not 404-because-unmounted) and answer 404 because
        // the program itself is not found, distinguishing "not implemented"
        // from "implemented, nothing to stream".
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/mirakurun/api/programs/1/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "GET /programs/1/stream should be 404 (no such program)");
    }

    #[tokio::test]
    async fn logs_api_requires_auth_like_every_other_api_route() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state, false);

        let res = app
            .oneshot(Request::builder().uri("/api/logs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logs_api_returns_entries_pushed_to_the_shared_buffer() {
        let state = test_web_state(AuthConfig { enabled: false, token: String::new() });
        // Simulate what LogBufferLayer::on_event does, without spinning up a
        // whole tracing subscriber for this test.
        state.log_buffer.query(crate::logging::LogQuery::default()); // sanity: starts empty
        {
            let result = state.log_buffer.query(crate::logging::LogQuery::default());
            assert!(result.entries.is_empty());
        }
        // Push directly isn't exposed publicly (push() is crate-private to
        // logging::buffer), so drive it the same way production code does:
        // through a real tracing event routed at this buffer.
        use tracing_subscriber::layer::SubscriberExt;
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(crate::logging::LogBufferLayer::new(Arc::clone(&state.log_buffer))),
            || {
                tracing::warn!(target: "recisdb_proxy::test", "something happened");
            },
        );

        let app = build_app(Arc::clone(&state), false);
        let res = app
            .oneshot(Request::builder().uri("/api/logs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entries = json["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["level"], "WARN");
        assert_eq!(entries[0]["target"], "recisdb_proxy::test");
        assert_eq!(entries[0]["message"], "something happened");
    }
}
