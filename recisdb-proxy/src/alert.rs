//! Alert manager for monitoring session metrics.

use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::time::interval;

use crate::database::AlertRuleRecord;
use crate::server::listener::DatabaseHandle;
use crate::web::SessionRegistry;

#[cfg(feature = "webhook")]
use reqwest::Client;

/// Alert manager task.
pub struct AlertManager {
    database: DatabaseHandle,
    session_registry: Arc<SessionRegistry>,
    #[cfg(feature = "webhook")]
    webhook_sender: WebhookSender,
}

impl AlertManager {
    /// Create a new alert manager.
    pub fn new(database: DatabaseHandle, session_registry: Arc<SessionRegistry>) -> Self {
        Self {
            database,
            session_registry,
            #[cfg(feature = "webhook")]
            webhook_sender: WebhookSender::new(),
        }
    }

    /// Run alert monitoring loop.
    pub async fn run(self) {
        let mut ticker = interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            if let Err(e) = self.check_rules().await {
                warn!("AlertManager error: {}", e);
            }
        }
    }

    async fn check_rules(&self) -> crate::database::Result<()> {
        let sessions = self.session_registry.get_all().await;
        let db = self.database.lock().await;
        let rules = db.get_enabled_alert_rules()?;

        for rule in rules.iter() {
            for session in sessions.iter() {
                let value = match metric_value(rule, session) {
                    Some(v) => v,
                    None => continue,
                };

                let triggered = evaluate_condition(&rule.condition, value, rule.threshold);
                let active = db.get_active_alert_for_rule_session(rule.id, Some(session.id as i64))?;

                if triggered && active.is_none() {
                    let message = format!(
                        "{} {} {} (value={:.2})",
                        rule.metric, rule.condition, rule.threshold, value
                    );
                    let alert_id = db.insert_alert_history(
                        rule.id,
                        Some(session.id as i64),
                        chrono::Utc::now().timestamp(),
                        Some(value),
                        Some(&message),
                    )?;

                    info!("Alert triggered: rule={} session={} id={}", rule.name, session.id, alert_id);

                    #[cfg(feature = "webhook")]
                    if let Some(url) = rule.webhook_url.as_deref() {
                        let format = rule.webhook_format.as_deref().unwrap_or("generic");
                        if let Err(e) = self.webhook_sender.send_alert(url, format, &rule, session.id, value, &message).await {
                            warn!("Webhook send failed: {}", e);
                        }
                    }
                } else if !triggered {
                    if let Some(active_alert) = active {
                        db.resolve_alert_history(active_alert.id, chrono::Utc::now().timestamp())?;
                        debug!("Alert resolved: rule={} session={}", rule.name, session.id);
                    }
                }
            }
        }

        Ok(())
    }
}

fn metric_value(rule: &AlertRuleRecord, session: &crate::web::SessionInfo) -> Option<f64> {
    match rule.metric.as_str() {
        "drop_rate" => Some(rate_percent(session.packets_dropped, session.packets_sent)),
        "scramble_rate" => Some(rate_percent(session.packets_scrambled, session.packets_sent)),
        "error_rate" => Some(rate_percent(session.packets_error, session.packets_sent)),
        "signal_level" => Some(session.signal_level as f64),
        "bitrate" => Some(session.current_bitrate_mbps),
        _ => None,
    }
}

fn rate_percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64) * 100.0
    }
}

fn evaluate_condition(condition: &str, value: f64, threshold: f64) -> bool {
    match condition {
        "gt" => value > threshold,
        "lt" => value < threshold,
        "gte" => value >= threshold,
        "lte" => value <= threshold,
        _ => false,
    }
}

#[cfg(feature = "webhook")]
struct WebhookSender {
    client: Client,
}

#[cfg(feature = "webhook")]
impl WebhookSender {
    fn new() -> Self {
        Self { client: Client::new() }
    }

    pub async fn send_alert(
        &self,
        url: &str,
        format: &str,
        rule: &AlertRuleRecord,
        session_id: u64,
        metric_value: f64,
        message: &str,
    ) -> crate::database::Result<()> {
        let payload = match format {
            "discord" => self.format_discord_payload(rule, session_id, metric_value, message),
            "slack" => self.format_slack_payload(rule, session_id, metric_value, message),
            "line" => self.format_line_payload(rule, session_id, metric_value, message),
            _ => self.format_generic_payload(rule, session_id, metric_value, message),
        };

        self.client.post(url).json(&payload).send().await.map_err(|e| {
            crate::database::DatabaseError::MigrationFailed(format!("Webhook error: {}", e))
        })?;
        Ok(())
    }

    fn format_discord_payload(
        &self,
        rule: &AlertRuleRecord,
        session_id: u64,
        metric_value: f64,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "embeds": [{
                "title": format!("Alert: {}", rule.name),
                "description": message,
                "color": 15158332,
                "fields": [
                    {"name": "Session", "value": session_id.to_string(), "inline": true},
                    {"name": "Metric", "value": rule.metric, "inline": true},
                    {"name": "Value", "value": format!("{:.2}", metric_value), "inline": true}
                ]
            }]
        })
    }

    fn format_slack_payload(
        &self,
        rule: &AlertRuleRecord,
        session_id: u64,
        metric_value: f64,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "blocks": [
                {
                    "type": "section",
                    "text": {"type": "mrkdwn", "text": format!("*Alert:* {}", rule.name)}
                },
                {
                    "type": "section",
                    "fields": [
                        {"type": "mrkdwn", "text": format!("*Session:* {}", session_id)},
                        {"type": "mrkdwn", "text": format!("*Metric:* {}", rule.metric)},
                        {"type": "mrkdwn", "text": format!("*Value:* {:.2}", metric_value)}
                    ]
                },
                {
                    "type": "section",
                    "text": {"type": "mrkdwn", "text": message}
                }
            ]
        })
    }

    fn format_line_payload(
        &self,
        rule: &AlertRuleRecord,
        session_id: u64,
        metric_value: f64,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "message": format!("[Alert] {}\nSession: {}\nMetric: {}\nValue: {:.2}\n{}", rule.name, session_id, rule.metric, metric_value, message)
        })
    }

    fn format_generic_payload(
        &self,
        rule: &AlertRuleRecord,
        session_id: u64,
        metric_value: f64,
        message: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "alert_name": rule.name,
            "session_id": session_id,
            "metric": rule.metric,
            "value": metric_value,
            "message": message,
            "severity": rule.severity,
        })
    }
}
