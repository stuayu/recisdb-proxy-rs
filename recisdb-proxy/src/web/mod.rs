//! Web dashboard server for monitoring and configuration.

pub mod api;
pub mod auth;
pub mod dashboard;
pub mod state;
pub mod stream;

use axum::{
    Router,
    routing::{delete, get, post},
};
use std::net::SocketAddr;
use std::sync::Arc;

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
        // Session/Client API
        .route("/clients", get(api::get_clients))
        .route("/stats", get(api::get_stats))
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
        // External encoder (tsreplace) configuration API
        .route("/tsreplace-config", get(api::get_tsreplace_config))
        .route("/tsreplace-config", post(api::update_tsreplace_config))
        // Encode profile catalogue API (STREAMING_DESIGN.md §5.3/§9 P5)
        .route("/encode-profiles", get(api::get_encode_profiles))
        .route("/encode-profiles", post(api::create_encode_profile))
        .route("/encode-profiles/:id", post(api::update_encode_profile))
        .route("/encode-profiles/:id", delete(api::delete_encode_profile))
        // HTTP-TS streaming endpoints (STREAMING_DESIGN.md §6.3/§7.2).
        // Auth is applied the same way as every other route here (see
        // `build_app`'s `route_layer(...require_auth)`) — §6.5.
        .route("/stream/service/:sid", get(stream::stream_service))
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
fn build_app(web_state: Arc<WebState>) -> Router {
    let api_router = build_api_router()
        .route_layer(axum::middleware::from_fn_with_state(web_state.clone(), auth::require_auth));

    Router::new()
        .nest("/api", api_router)
        // Dashboard route (unauthenticated: serves the token-entry UI)
        .route("/", get(dashboard::index))
        .route("/logos/:file", get(api::get_logo))
        // Static assets (currently just an optional local mpegts.js — see
        // STREAMING_DESIGN.md §6.4). Unauthenticated like /logos/:file: a
        // fixed allow-list, no path traversal, no confidential content.
        .route("/static/:file", get(api::get_static_asset))
        .with_state(web_state)
}

/// Start the web dashboard server.
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
) -> Result<(), Box<dyn std::error::Error>> {
    let mut web_state = WebState::new(database, tuner_pool, encoder_pool, session_registry, auth);
    if let Some(config) = scan_config {
        *web_state.scan_config.write().await = config;
    }
    if let Some(config) = tuner_config {
        *web_state.tuner_config.write().await = config;
    }
    let web_state = Arc::new(web_state);

    let app = build_app(web_state);

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    log::info!("Web dashboard listening on http://{}", listen_addr);

    axum::serve(listener, app).await?;

    Ok(())
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
        Arc::new(WebState::new(database, tuner_pool, encoder_pool, session_registry, auth))
    }

    #[tokio::test]
    async fn api_request_without_token_is_rejected() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state);

        let res = app
            .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_request_with_wrong_token_is_rejected() {
        let state = test_web_state(AuthConfig { enabled: true, token: "secret-token".to_string() });
        let app = build_app(state);

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
        let app = build_app(state);

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

    #[tokio::test]
    async fn auth_disabled_bypasses_token_check() {
        let state = test_web_state(AuthConfig { enabled: false, token: "secret-token".to_string() });
        let app = build_app(state);

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
        let app = build_app(state);

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
        let app = build_app(state);

        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }
}
