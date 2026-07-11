//! Alert rule and alert-history endpoints.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;

use super::error::ApiError;

/// Alert rule create/update request.
#[derive(Debug, Deserialize)]
pub struct AlertRuleRequest {
    pub name: String,
    pub metric: String,
    pub condition: String,
    pub threshold: f64,
    pub severity: Option<String>,
    pub is_enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub webhook_format: Option<String>,
}

/// Get active alerts.
pub async fn get_alerts(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    let alerts = db.get_active_alerts()?;
    Ok(Json(json!({
        "success": true,
        "alerts": alerts,
        "count": alerts.len()
    })))
}

/// Get alert rules.
pub async fn get_alert_rules(
    State(web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    let rules = db.get_alert_rules()?;
    Ok(Json(json!({
        "success": true,
        "rules": rules,
        "count": rules.len()
    })))
}

/// Create alert rule.
pub async fn create_alert_rule(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<AlertRuleRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    let severity = payload.severity.unwrap_or_else(|| "warning".to_string());
    let is_enabled = payload.is_enabled.unwrap_or(true);

    let id = db.create_alert_rule(
        &payload.name,
        &payload.metric,
        &payload.condition,
        payload.threshold,
        &severity,
        is_enabled,
        payload.webhook_url.as_deref(),
        payload.webhook_format.as_deref(),
    )?;
    Ok(Json(json!({
        "success": true,
        "id": id
    })))
}

/// Delete alert rule.
pub async fn delete_alert_rule(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    db.delete_alert_rule(id)?;
    Ok(Json(json!({"success": true})))
}

/// Acknowledge alert.
pub async fn acknowledge_alert(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = web_state.database.lock().await;
    db.acknowledge_alert_history(id)?;
    Ok(Json(json!({"success": true})))
}
