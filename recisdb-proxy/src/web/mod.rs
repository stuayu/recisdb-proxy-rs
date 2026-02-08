//! Web dashboard server for monitoring and configuration.

pub mod api;
pub mod dashboard;
pub mod state;

use axum::{
    Router,
    routing::{delete, get, post},
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::server::listener::DatabaseHandle;
use crate::tuner::TunerPool;
use state::WebState;

pub use state::{SessionInfo, SessionRegistry};

/// Start the web dashboard server.
pub async fn start_web_server(
    listen_addr: SocketAddr,
    database: DatabaseHandle,
    tuner_pool: Arc<TunerPool>,
    session_registry: Arc<SessionRegistry>,
    scan_config: Option<state::ScanSchedulerInfo>,
    tuner_config: Option<state::TunerConfigInfo>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut web_state = WebState::new(database, tuner_pool, session_registry);
    if let Some(config) = scan_config {
        *web_state.scan_config.write().await = config;
    }
    if let Some(config) = tuner_config {
        *web_state.tuner_config.write().await = config;
    }
    let web_state = Arc::new(web_state);

    let app = Router::new()
        // Legacy API routes (for backwards compatibility)
        .route("/api/tuners", get(api::get_tuners))
        .route("/api/config", get(api::get_config))
        .route("/api/config", post(api::update_config))
        // Session/Client API
        .route("/api/clients", get(api::get_clients))
        .route("/api/stats", get(api::get_stats))
        .route("/api/client/:id/quality", get(api::get_client_quality))
        .route("/api/client/:id/metrics-history", get(api::get_client_metrics_history))
        .route("/api/client/:id/disconnect", post(api::disconnect_client))
        .route("/api/client/:id/controls", post(api::override_client_controls))
        .route("/api/session-history", get(api::get_session_history))
        // BonDriver API
        .route("/api/bondrivers", get(api::get_bondrivers))
        .route("/api/bondriver/:id", get(api::get_bondriver))
        .route("/api/bondriver/:id", post(api::update_bondriver))
        .route("/api/bondriver/:id", delete(api::delete_bondriver))
        .route("/api/bondriver/:id/scan", post(api::trigger_scan))
        .route("/api/bondriver/:id/quality", get(api::get_bondriver_quality))
        .route("/api/bondrivers/ranking", get(api::get_bondrivers_ranking))
        // Channel API
        .route("/api/channels", get(api::get_channels))
        .route("/api/channel/:id", post(api::update_channel))
        .route("/api/channel/:id/toggle", post(api::toggle_channel))
        .route("/api/channel/:id", delete(api::delete_channel))
        // Scan history API
        .route("/api/scan-history", get(api::get_scan_history))
        // Alert API
        .route("/api/alerts", get(api::get_alerts))
        .route("/api/alert-rules", get(api::get_alert_rules))
        .route("/api/alert-rules", post(api::create_alert_rule))
        .route("/api/alert-rules/:id", delete(api::delete_alert_rule))
        .route("/api/alerts/:id/acknowledge", post(api::acknowledge_alert))
        // Scan scheduler configuration API
        .route("/api/scan-config", get(api::get_scan_config))
        .route("/api/scan-config", post(api::update_scan_config))
        // Tuner optimization configuration API
        .route("/api/tuner-config", get(api::get_tuner_config))
        .route("/api/tuner-config", post(api::update_tuner_config))
        // Dashboard route
        .route("/", get(dashboard::index))
        .with_state(web_state)
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    log::info!("Web dashboard listening on http://{}", listen_addr);

    axum::serve(listener, app).await?;

    Ok(())
}
