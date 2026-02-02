//! Alert rule and history database operations.

use rusqlite::params;

use super::{AlertHistoryRecord, AlertRuleRecord, Database, Result};

impl Database {
    /// Get all alert rules.
    pub fn get_alert_rules(&self) -> Result<Vec<AlertRuleRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, metric, condition, threshold, severity, is_enabled, webhook_url, webhook_format, created_at FROM alert_rules ORDER BY id DESC",
        )?;

        let rules = stmt
            .query_map([], |row| {
                Ok(AlertRuleRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    metric: row.get(2)?,
                    condition: row.get(3)?,
                    threshold: row.get(4)?,
                    severity: row.get(5)?,
                    is_enabled: row.get::<_, i32>(6)? != 0,
                    webhook_url: row.get(7)?,
                    webhook_format: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rules)
    }

    /// Get enabled alert rules.
    pub fn get_enabled_alert_rules(&self) -> Result<Vec<AlertRuleRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, metric, condition, threshold, severity, is_enabled, webhook_url, webhook_format, created_at FROM alert_rules WHERE is_enabled = 1 ORDER BY id DESC",
        )?;

        let rules = stmt
            .query_map([], |row| {
                Ok(AlertRuleRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    metric: row.get(2)?,
                    condition: row.get(3)?,
                    threshold: row.get(4)?,
                    severity: row.get(5)?,
                    is_enabled: row.get::<_, i32>(6)? != 0,
                    webhook_url: row.get(7)?,
                    webhook_format: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rules)
    }

    /// Create a new alert rule.
    pub fn create_alert_rule(
        &self,
        name: &str,
        metric: &str,
        condition: &str,
        threshold: f64,
        severity: &str,
        is_enabled: bool,
        webhook_url: Option<&str>,
        webhook_format: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO alert_rules (name, metric, condition, threshold, severity, is_enabled, webhook_url, webhook_format) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                name,
                metric,
                condition,
                threshold,
                severity,
                is_enabled as i32,
                webhook_url,
                webhook_format,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Delete an alert rule.
    pub fn delete_alert_rule(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM alert_rules WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Insert alert history entry.
    pub fn insert_alert_history(
        &self,
        rule_id: i64,
        session_id: Option<i64>,
        triggered_at: i64,
        metric_value: Option<f64>,
        message: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO alert_history (rule_id, session_id, triggered_at, metric_value, message) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![rule_id, session_id, triggered_at, metric_value, message],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Resolve an alert history entry.
    pub fn resolve_alert_history(&self, id: i64, resolved_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE alert_history SET resolved_at = ?2 WHERE id = ?1",
            params![id, resolved_at],
        )?;
        Ok(())
    }

    /// Acknowledge an alert history entry.
    pub fn acknowledge_alert_history(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE alert_history SET acknowledged = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Get active alerts (resolved_at is NULL).
    pub fn get_active_alerts(&self) -> Result<Vec<AlertHistoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_id, session_id, triggered_at, resolved_at, metric_value, message, acknowledged FROM alert_history WHERE resolved_at IS NULL ORDER BY triggered_at DESC",
        )?;

        let records = stmt
            .query_map([], |row| {
                Ok(AlertHistoryRecord {
                    id: row.get(0)?,
                    rule_id: row.get(1)?,
                    session_id: row.get(2)?,
                    triggered_at: row.get(3)?,
                    resolved_at: row.get(4)?,
                    metric_value: row.get(5)?,
                    message: row.get(6)?,
                    acknowledged: row.get::<_, i32>(7)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get active alert history by rule and session.
    pub fn get_active_alert_for_rule_session(
        &self,
        rule_id: i64,
        session_id: Option<i64>,
    ) -> Result<Option<AlertHistoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_id, session_id, triggered_at, resolved_at, metric_value, message, acknowledged FROM alert_history WHERE rule_id = ?1 AND session_id IS ?2 AND resolved_at IS NULL ORDER BY triggered_at DESC LIMIT 1",
        )?;

        let result = stmt.query_row(params![rule_id, session_id], |row| {
            Ok(AlertHistoryRecord {
                id: row.get(0)?,
                rule_id: row.get(1)?,
                session_id: row.get(2)?,
                triggered_at: row.get(3)?,
                resolved_at: row.get(4)?,
                metric_value: row.get(5)?,
                message: row.get(6)?,
                acknowledged: row.get::<_, i32>(7)? != 0,
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
