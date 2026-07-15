//! Client/session endpoints: connected clients, server stats, session
//! history, and per-client quality/metrics history.

use axum::{
    extract::{Path, Query, State},
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    Json,
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use crate::web::state::WebState;

use super::error::ApiError;

/// Server statistics.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerStats {
    pub total_sessions: u64,
    pub active_sessions: u64,
    pub total_tuners: usize,
    pub active_tuners: usize,
    pub uptime_seconds: u64,
    pub total_sessions_db: u64,
    pub total_channels: u64,
}

/// Session history query.
#[derive(Debug, Deserialize)]
pub struct SessionHistoryQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    pub client_address: Option<String>,
}

/// Client control override request.
#[derive(Debug, Deserialize)]
pub struct ClientControlOverrideRequest {
    pub override_priority: Option<Option<i32>>,
    pub override_exclusive: Option<Option<bool>>,
}

/// Get all connected clients.
pub async fn get_clients(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let sessions = web_state.session_registry.get_all().await;

    let clients: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let effective_priority = s.override_priority.or(s.client_priority);
            let effective_exclusive = s.override_exclusive.unwrap_or(s.client_exclusive);
            json!({
                "session_id": s.id,
                "address": s.addr,
                "host": s.host,
                "tuner_path": s.tuner_path,
                "channel_info": s.channel_info,
                "channel_name": s.channel_name,
                "nid": s.channel_nid,
                "sid": s.channel_sid,
                "is_streaming": s.is_streaming,
                "connected_seconds": s.connected_seconds(),
                "signal_level": (s.signal_level * 10.0).round() / 10.0,
                "packets_sent": s.packets_sent,
                "packets_dropped": s.packets_dropped,
                "packets_scrambled": s.packets_scrambled,
                "packets_error": s.packets_error,
                "current_bitrate_mbps": (s.current_bitrate_mbps * 100.0).round() / 100.0,
                "client_priority": s.client_priority,
                "client_exclusive": s.client_exclusive,
                "override_priority": s.override_priority,
                "override_exclusive": s.override_exclusive,
                "effective_priority": effective_priority,
                "effective_exclusive": effective_exclusive,
                "stream_class": s.stream_class,
                "prefilling": s.prefilling
            })
        })
        .collect();

    let count = clients.len();

    Json(json!({
        "success": true,
        "clients": clients,
        "count": count
    }))
}

/// Get server statistics.
pub async fn get_stats(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let active_sessions = web_state.session_registry.count().await;
    let tuner_keys = web_state.tuner_pool.keys().await;
    let total_tuners = tuner_keys.len();

    let mut active_tuners = 0;
    for key in tuner_keys.iter() {
        if let Some(tuner) = web_state.tuner_pool.get(key).await {
            if tuner.is_running() {
                active_tuners += 1;
            }
        }
    }

    let (total_sessions_db, total_channels) = {
        let db = web_state.database.lock().await;
        (
            db.get_total_session_count().unwrap_or(0),
            db.get_total_channel_count().unwrap_or(0),
        )
    };

    let stats = ServerStats {
        total_sessions: total_sessions_db,
        active_sessions: active_sessions as u64,
        total_tuners,
        active_tuners,
        uptime_seconds: 0,
        total_sessions_db,
        total_channels,
    };

    Json(json!({
        "success": true,
        "stats": stats
    }))
}

/// Lightweight dashboard invalidation stream.
///
/// The stream deliberately sends an event rather than duplicating every API
/// response. The Vue client keeps the existing typed REST endpoints as the
/// source of truth and refreshes them when this event arrives.
pub async fn dashboard_events(
    State(_web_state): State<Arc<WebState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let events = stream::unfold(0_u64, |sequence| async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let event = Event::default()
            .event("refresh")
            .id(sequence.to_string())
            .data("dashboard");
        Some((Ok(event), sequence.wrapping_add(1)))
    });
    Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// Get session history (paginated).
pub async fn get_session_history(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<SessionHistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 200);

    let db = web_state.database.lock().await;
    let (rows, total) = db.get_session_history(page, per_page, query.client_address.as_deref())?;
    Ok(Json(json!({
        "success": true,
        "total": total,
        "page": page,
        "per_page": per_page,
        "history": rows
    })))
}

/// Get time-series quality data for a client.
pub async fn get_client_quality(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = web_state.session_registry.get_all().await;
    if let Some(session) = sessions.into_iter().find(|s| s.id == id) {
        let bitrate: Vec<(i64, f64)> = session.metrics_history.bitrate_history.into_iter().collect();
        let packet_loss: Vec<(i64, f64)> = session.metrics_history.packet_loss_history.into_iter().collect();

        return Ok(Json(json!({
            "success": true,
            "bitrate": bitrate,
            "packet_loss": packet_loss,
            // Loss-source breakdown (STREAMING_DESIGN.md §3.1 / §8, P1).
            "loss_broadcast_lag_chunks": session.loss_broadcast_lag_chunks,
            "loss_ts_queue_chunks": session.loss_ts_queue_chunks,
            "loss_encoder_stall_events": session.loss_encoder_stall_events,
            "top_loss_pids": session.top_loss_pids,
        })));
    }

    Err(ApiError::not_found("Session not found"))
}

/// Get metrics history for a client (bitrate, packet loss, signal level).
pub async fn get_client_metrics_history(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = web_state.session_registry.get_all().await;
    if let Some(session) = sessions.into_iter().find(|s| s.id == id) {
        let bitrate: Vec<(i64, f64)> = session.metrics_history.bitrate_history.into_iter().collect();
        let packet_loss: Vec<(i64, f64)> = session.metrics_history.packet_loss_history.into_iter().collect();
        let signal_level: Vec<(i64, f32)> = session.metrics_history.signal_history.into_iter().collect();

        return Ok(Json(json!({
            "success": true,
            "bitrate": bitrate,
            "packet_loss": packet_loss,
            "signal_level": signal_level
        })));
    }

    Err(ApiError::not_found("Session not found"))
}

/// Disconnect a client session remotely.
pub async fn disconnect_client(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let ok = web_state.session_registry.request_shutdown(id).await;
    Json(json!({
        "success": ok
    }))
}

/// Override client controls (priority/exclusive).
pub async fn override_client_controls(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
    Json(payload): Json<ClientControlOverrideRequest>,
) -> impl IntoResponse {
    // Treat JSON null as explicit clear. Absence means no change.
    web_state
        .session_registry
        .update_override_controls(id, payload.override_priority, payload.override_exclusive)
        .await;
    Json(json!({
        "success": true
    }))
}
