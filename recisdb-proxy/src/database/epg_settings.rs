//! DB-backed EPG runtime configuration and effective-value resolution.
//!
//! This is the only configuration boundary used by the EPG scheduler/API.
//! `NULL` in preset/tuner rows means inheritance; it is never exposed as such
//! to the UI.

use crate::database::{Database, DatabaseError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const EPG_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS epg_global_settings (
 id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL DEFAULT 1,
 scheduler_interval_secs INTEGER NOT NULL DEFAULT 300, target_refresh_secs INTEGER NOT NULL DEFAULT 21600,
 max_stale_secs INTEGER NOT NULL DEFAULT 43200, min_future_coverage_hours INTEGER NOT NULL DEFAULT 24,
 target_future_coverage_hours INTEGER NOT NULL DEFAULT 168, startup_delay_secs INTEGER NOT NULL DEFAULT 30,
 startup_jitter_secs INTEGER NOT NULL DEFAULT 30, min_dwell_secs INTEGER NOT NULL DEFAULT 30,
 normal_dwell_secs INTEGER NOT NULL DEFAULT 90, max_dwell_secs INTEGER NOT NULL DEFAULT 180,
 idle_section_timeout_secs INTEGER NOT NULL DEFAULT 20, max_concurrent_scans INTEGER NOT NULL DEFAULT 1,
 reserve_tuners INTEGER NOT NULL DEFAULT 0, prefer_local INTEGER NOT NULL DEFAULT 1,
 allow_remote INTEGER NOT NULL DEFAULT 0, preemptible INTEGER NOT NULL DEFAULT 1,
 cpu_soft_limit_percent INTEGER NOT NULL DEFAULT 70, cpu_hard_limit_percent INTEGER NOT NULL DEFAULT 90,
 remote_prefer_metadata_execution INTEGER NOT NULL DEFAULT 1, remote_allow_ts_transport INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL DEFAULT(strftime('%s','now')), updated_at INTEGER NOT NULL DEFAULT(strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS epg_scan_presets (
 id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, description TEXT NOT NULL DEFAULT '',
 is_system INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1,
 target_refresh_secs INTEGER, max_stale_secs INTEGER, min_future_coverage_hours INTEGER,
 target_future_coverage_hours INTEGER, min_dwell_secs INTEGER, normal_dwell_secs INTEGER,
 max_dwell_secs INTEGER, idle_section_timeout_secs INTEGER, reserve_tuners INTEGER,
 prefer_local INTEGER, allow_remote INTEGER, preemptible INTEGER, cpu_soft_limit_percent INTEGER,
 cpu_hard_limit_percent INTEGER, remote_prefer_metadata_execution INTEGER, remote_allow_ts_transport INTEGER,
 created_at INTEGER NOT NULL DEFAULT(strftime('%s','now')), updated_at INTEGER NOT NULL DEFAULT(strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS physical_tuner_epg_settings (
 physical_tuner_id INTEGER PRIMARY KEY, enabled_override INTEGER, preset_id INTEGER,
 target_refresh_secs_override INTEGER, max_stale_secs_override INTEGER,
 min_dwell_secs_override INTEGER, normal_dwell_secs_override INTEGER, max_dwell_secs_override INTEGER,
 allow_remote_override INTEGER, prefer_local_override INTEGER, preemptible_override INTEGER,
 reserve_for_recording_override INTEGER, created_at INTEGER NOT NULL DEFAULT(strftime('%s','now')),
 updated_at INTEGER NOT NULL DEFAULT(strftime('%s','now')), FOREIGN KEY(preset_id) REFERENCES epg_scan_presets(id) ON DELETE SET NULL
);
CREATE TABLE IF NOT EXISTS epg_scan_states (
 network_id INTEGER NOT NULL, tsid INTEGER NOT NULL,
 last_scan_started_at INTEGER, last_scan_completed_at INTEGER,
 last_eit_received_at INTEGER, coverage_until INTEGER, next_eligible_at INTEGER, last_tuner_id INTEGER,
 last_node_id TEXT, failure_count INTEGER NOT NULL DEFAULT 0, last_failure_reason TEXT,
 PRIMARY KEY(network_id, tsid)
);
CREATE TABLE IF NOT EXISTS epg_scan_history (
 id INTEGER PRIMARY KEY AUTOINCREMENT, started_at INTEGER NOT NULL, finished_at INTEGER,
 status TEXT NOT NULL, reason TEXT, physical_tuner_id INTEGER, node_id TEXT, network_id INTEGER, tsid INTEGER,
 coverage_before INTEGER, coverage_after INTEGER, error TEXT
);
CREATE TABLE IF NOT EXISTS epg_scan_retention (id INTEGER PRIMARY KEY CHECK(id=1), max_rows INTEGER NOT NULL DEFAULT 1000,
 max_age_days INTEGER NOT NULL DEFAULT 30);
CREATE INDEX IF NOT EXISTS idx_epg_scan_history_started_at ON epg_scan_history(started_at);
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgGlobalSettings {
    pub enabled: bool,
    pub scheduler_interval_secs: i64,
    pub target_refresh_secs: i64,
    pub max_stale_secs: i64,
    pub min_future_coverage_hours: i64,
    pub target_future_coverage_hours: i64,
    pub startup_delay_secs: i64,
    pub startup_jitter_secs: i64,
    pub min_dwell_secs: i64,
    pub normal_dwell_secs: i64,
    pub max_dwell_secs: i64,
    pub idle_section_timeout_secs: i64,
    pub max_concurrent_scans: i64,
    pub reserve_tuners: bool,
    pub prefer_local: bool,
    pub allow_remote: bool,
    pub preemptible: bool,
    pub cpu_soft_limit_percent: i64,
    pub cpu_hard_limit_percent: i64,
    pub remote_prefer_metadata_execution: bool,
    pub remote_allow_ts_transport: bool,
    pub selected_preset_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgPreset {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub is_system: bool,
    pub enabled: bool,
    pub target_refresh_secs: Option<i64>,
    pub max_stale_secs: Option<i64>,
    pub min_future_coverage_hours: Option<i64>,
    pub target_future_coverage_hours: Option<i64>,
    pub min_dwell_secs: Option<i64>,
    pub normal_dwell_secs: Option<i64>,
    pub max_dwell_secs: Option<i64>,
    pub idle_section_timeout_secs: Option<i64>,
    pub reserve_tuners: Option<bool>,
    pub prefer_local: Option<bool>,
    pub allow_remote: Option<bool>,
    pub preemptible: Option<bool>,
    pub cpu_soft_limit_percent: Option<i64>,
    pub cpu_hard_limit_percent: Option<i64>,
    pub remote_prefer_metadata_execution: Option<bool>,
    pub remote_allow_ts_transport: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PhysicalTunerEpgSettings {
    pub physical_tuner_id: i64,
    pub enabled_override: Option<bool>,
    pub preset_id: Option<i64>,
    pub target_refresh_secs_override: Option<i64>,
    pub max_stale_secs_override: Option<i64>,
    pub min_dwell_secs_override: Option<i64>,
    pub normal_dwell_secs_override: Option<i64>,
    pub max_dwell_secs_override: Option<i64>,
    pub allow_remote_override: Option<bool>,
    pub prefer_local_override: Option<bool>,
    pub preemptible_override: Option<bool>,
    pub reserve_for_recording_override: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpgScanState {
    pub network_id: u16,
    pub tsid: u16,
    pub last_scan_started_at: Option<i64>,
    pub last_scan_completed_at: Option<i64>,
    pub last_eit_received_at: Option<i64>,
    pub coverage_until: Option<i64>,
    pub next_eligible_at: Option<i64>,
    pub last_tuner_id: Option<i64>,
    pub last_node_id: Option<String>,
    pub failure_count: i64,
    pub last_failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveEpgScanConfig {
    pub effective: EpgGlobalSettings,
    pub source: serde_json::Value,
}

fn b(v: bool) -> i64 {
    i64::from(v)
}
fn valid(c: &EpgGlobalSettings) -> bool {
    c.min_dwell_secs <= c.normal_dwell_secs
        && c.normal_dwell_secs <= c.max_dwell_secs
        && c.cpu_soft_limit_percent < c.cpu_hard_limit_percent
        && c.min_future_coverage_hours <= c.target_future_coverage_hours
}

pub(crate) fn seed_defaults(c: &Connection) -> Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO epg_global_settings(id) VALUES(1)",
        [],
    )?;
    c.execute("INSERT OR IGNORE INTO epg_scan_retention(id) VALUES(1)", [])?;
    for (name, desc) in [
        ("標準", "録画・視聴を優先しながら自動更新"),
        ("EPG優先", "番組表の鮮度を優先"),
        ("低負荷", "システムが空いている時だけ更新"),
        ("地デジ向け", "地デジの物理TSを優先"),
        ("BS/CS向け", "衛星放送のOther-TS EITを活用"),
        ("4K低負荷", "4Kの変換負荷を抑制"),
        ("リモート環境向け", "リモート実行を優先"),
    ] {
        c.execute(
            "INSERT OR IGNORE INTO epg_scan_presets(name,description,is_system) VALUES(?1,?2,1)",
            params![name, desc],
        )?;
    }
    Ok(())
}

impl Database {
    pub fn epg_scan_started(
        &self,
        tuner_id: i64,
        network_id: u16,
        tsid: u16,
        reason: &str,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        self.connection().execute("INSERT INTO epg_scan_states(network_id,tsid,last_scan_started_at,last_tuner_id,last_failure_reason) VALUES(?1,?2,?3,?4,NULL) ON CONFLICT(network_id,tsid) DO UPDATE SET last_scan_started_at=?3,last_tuner_id=?4,last_failure_reason=NULL", params![network_id,tsid,now,tuner_id])?;
        self.connection().execute("INSERT INTO epg_scan_history(started_at,status,reason,physical_tuner_id,network_id,tsid) VALUES(?1,'running',?2,?3,?4,?5)", params![now,reason,tuner_id,network_id,tsid])?;
        Ok(self.connection().last_insert_rowid())
    }
    pub fn epg_scan_finished(
        &self,
        history_id: i64,
        status: &str,
        network_id: u16,
        tsid: u16,
        error: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.connection().execute(
            "UPDATE epg_scan_history SET finished_at=?1,status=?2,error=?3 WHERE id=?4",
            params![now, status, error, history_id],
        )?;
        let failure = if status == "completed" { 0 } else { 1 };
        self.connection().execute("UPDATE epg_scan_states SET last_scan_completed_at=?1,failure_count=CASE WHEN ?2=0 THEN 0 ELSE failure_count+1 END,last_failure_reason=?3,next_eligible_at=?1+60 WHERE network_id=?4 AND tsid=?5",params![now,failure,error,network_id,tsid])?;
        self.connection().execute("DELETE FROM epg_scan_history WHERE id NOT IN (SELECT id FROM epg_scan_history ORDER BY started_at DESC LIMIT (SELECT max_rows FROM epg_scan_retention WHERE id=1)) OR started_at < ?1-(SELECT max_age_days FROM epg_scan_retention WHERE id=1)*86400",[now])?;
        Ok(())
    }
    pub fn epg_last_event_time(&self) -> Result<Option<i64>> {
        Ok(self
            .connection()
            .query_row("SELECT MAX(updated_at) FROM programs", [], |r| r.get(0))?)
    }

    /// Reconcile state with rows actually retained in `programs`.
    /// Called after writer flushes, never from the reader loop.
    pub fn refresh_epg_coverage(&self) -> Result<Option<i64>> {
        let coverage: Option<i64> = self.connection().query_row(
            "SELECT MAX(start_at + duration_secs) FROM programs",
            [],
            |r| r.get(0),
        )?;
        let configured: Vec<(i64, i64)> = self
            .connection()
            .prepare("SELECT DISTINCT nid,tsid FROM channels")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (nid, tsid) in configured {
            self.connection().execute(
                "INSERT INTO epg_scan_states(network_id,tsid) VALUES(?1,?2) ON CONFLICT(network_id,tsid) DO NOTHING",
                params![nid, tsid],
            )?;
        }
        let grouped: Vec<_> = self.connection().prepare("SELECT nid,tsid,MAX(start_at + duration_secs),MAX(updated_at) FROM programs GROUP BY nid,tsid")?.query_map([], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,i64>(1)?, r.get::<_,Option<i64>>(2)?, r.get::<_,Option<i64>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (nid, tsid, until, updated) in grouped {
            self.connection().execute("INSERT INTO epg_scan_states(network_id,tsid,coverage_until,last_eit_received_at) VALUES(?1,?2,?3,?4) ON CONFLICT(network_id,tsid) DO UPDATE SET coverage_until=?3,last_eit_received_at=?4", params![nid,tsid,until,updated])?;
        }
        self.connection().execute("UPDATE epg_scan_states SET coverage_until=NULL,last_eit_received_at=NULL WHERE NOT EXISTS (SELECT 1 FROM programs p WHERE p.nid=epg_scan_states.network_id AND p.tsid=epg_scan_states.tsid)", [])?;
        Ok(coverage)
    }
    pub fn get_epg_scan_states(&self) -> Result<Vec<EpgScanState>> {
        let mut stmt = self.connection().prepare("SELECT network_id,tsid,last_scan_started_at,last_scan_completed_at,last_eit_received_at,coverage_until,next_eligible_at,last_tuner_id,last_node_id,failure_count,last_failure_reason FROM epg_scan_states ORDER BY network_id,tsid")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EpgScanState {
                    network_id: r.get::<_, i64>(0)? as u16,
                    tsid: r.get::<_, i64>(1)? as u16,
                    last_scan_started_at: r.get(2)?,
                    last_scan_completed_at: r.get(3)?,
                    last_eit_received_at: r.get(4)?,
                    coverage_until: r.get(5)?,
                    next_eligible_at: r.get(6)?,
                    last_tuner_id: r.get(7)?,
                    last_node_id: r.get(8)?,
                    failure_count: r.get(9)?,
                    last_failure_reason: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn get_epg_global_settings(&self) -> Result<EpgGlobalSettings> {
        self.connection().query_row("SELECT enabled,scheduler_interval_secs,target_refresh_secs,max_stale_secs,min_future_coverage_hours,target_future_coverage_hours,startup_delay_secs,startup_jitter_secs,min_dwell_secs,normal_dwell_secs,max_dwell_secs,idle_section_timeout_secs,max_concurrent_scans,reserve_tuners,prefer_local,allow_remote,preemptible,cpu_soft_limit_percent,cpu_hard_limit_percent,remote_prefer_metadata_execution,remote_allow_ts_transport,selected_preset_id FROM epg_global_settings WHERE id=1",[],|r|Ok(EpgGlobalSettings{enabled:r.get::<_,i64>(0)?!=0,scheduler_interval_secs:r.get(1)?,target_refresh_secs:r.get(2)?,max_stale_secs:r.get(3)?,min_future_coverage_hours:r.get(4)?,target_future_coverage_hours:r.get(5)?,startup_delay_secs:r.get(6)?,startup_jitter_secs:r.get(7)?,min_dwell_secs:r.get(8)?,normal_dwell_secs:r.get(9)?,max_dwell_secs:r.get(10)?,idle_section_timeout_secs:r.get(11)?,max_concurrent_scans:r.get(12)?,reserve_tuners:r.get::<_,i64>(13)?!=0,prefer_local:r.get::<_,i64>(14)?!=0,allow_remote:r.get::<_,i64>(15)?!=0,preemptible:r.get::<_,i64>(16)?!=0,cpu_soft_limit_percent:r.get(17)?,cpu_hard_limit_percent:r.get(18)?,remote_prefer_metadata_execution:r.get::<_,i64>(19)?!=0,remote_allow_ts_transport:r.get::<_,i64>(20)?!=0,selected_preset_id:r.get(21)?})).map_err(Into::into)
    }
    pub fn update_epg_global_settings(&self, c: &EpgGlobalSettings) -> Result<()> {
        if !valid(c) {
            return Err(DatabaseError::MigrationFailed(
                "invalid EPG settings ordering or CPU limits".into(),
            ));
        }
        self.connection().execute("UPDATE epg_global_settings SET enabled=?1,scheduler_interval_secs=?2,target_refresh_secs=?3,max_stale_secs=?4,min_future_coverage_hours=?5,target_future_coverage_hours=?6,startup_delay_secs=?7,startup_jitter_secs=?8,min_dwell_secs=?9,normal_dwell_secs=?10,max_dwell_secs=?11,idle_section_timeout_secs=?12,max_concurrent_scans=?13,reserve_tuners=?14,prefer_local=?15,allow_remote=?16,preemptible=?17,cpu_soft_limit_percent=?18,cpu_hard_limit_percent=?19,remote_prefer_metadata_execution=?20,remote_allow_ts_transport=?21,selected_preset_id=?22,updated_at=strftime('%s','now') WHERE id=1",params![b(c.enabled),c.scheduler_interval_secs,c.target_refresh_secs,c.max_stale_secs,c.min_future_coverage_hours,c.target_future_coverage_hours,c.startup_delay_secs,c.startup_jitter_secs,c.min_dwell_secs,c.normal_dwell_secs,c.max_dwell_secs,c.idle_section_timeout_secs,b(c.reserve_tuners),b(c.prefer_local),b(c.allow_remote),b(c.preemptible),c.cpu_soft_limit_percent,c.cpu_hard_limit_percent,b(c.remote_prefer_metadata_execution),b(c.remote_allow_ts_transport),c.selected_preset_id])?;
        Ok(())
    }
    pub fn list_epg_presets(&self) -> Result<Vec<EpgPreset>> {
        let mut s=self.connection().prepare("SELECT id,name,description,is_system,enabled,target_refresh_secs,max_stale_secs,min_future_coverage_hours,target_future_coverage_hours,min_dwell_secs,normal_dwell_secs,max_dwell_secs,idle_section_timeout_secs,reserve_tuners,prefer_local,allow_remote,preemptible,cpu_soft_limit_percent,cpu_hard_limit_percent,remote_prefer_metadata_execution,remote_allow_ts_transport FROM epg_scan_presets ORDER BY is_system DESC,id")?;
        let rows = s
            .query_map([], |r| {
                Ok(EpgPreset {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    is_system: r.get::<_, i64>(3)? != 0,
                    enabled: r.get::<_, i64>(4)? != 0,
                    target_refresh_secs: r.get(5)?,
                    max_stale_secs: r.get(6)?,
                    min_future_coverage_hours: r.get(7)?,
                    target_future_coverage_hours: r.get(8)?,
                    min_dwell_secs: r.get(9)?,
                    normal_dwell_secs: r.get(10)?,
                    max_dwell_secs: r.get(11)?,
                    idle_section_timeout_secs: r.get(12)?,
                    reserve_tuners: r.get::<_, Option<i64>>(13)?.map(|v| v != 0),
                    prefer_local: r.get::<_, Option<i64>>(14)?.map(|v| v != 0),
                    allow_remote: r.get::<_, Option<i64>>(15)?.map(|v| v != 0),
                    preemptible: r.get::<_, Option<i64>>(16)?.map(|v| v != 0),
                    cpu_soft_limit_percent: r.get(17)?,
                    cpu_hard_limit_percent: r.get(18)?,
                    remote_prefer_metadata_execution: r.get::<_, Option<i64>>(19)?.map(|v| v != 0),
                    remote_allow_ts_transport: r.get::<_, Option<i64>>(20)?.map(|v| v != 0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn get_physical_tuner_epg_settings(&self, id: i64) -> Result<PhysicalTunerEpgSettings> {
        Ok(self.connection().query_row("SELECT enabled_override,preset_id,target_refresh_secs_override,max_stale_secs_override,min_dwell_secs_override,normal_dwell_secs_override,max_dwell_secs_override,allow_remote_override,prefer_local_override,preemptible_override,reserve_for_recording_override FROM physical_tuner_epg_settings WHERE physical_tuner_id=?", [id], |r| Ok(PhysicalTunerEpgSettings { physical_tuner_id:id, enabled_override:r.get::<_,Option<i64>>(0)?.map(|v|v!=0), preset_id:r.get(1)?, target_refresh_secs_override:r.get(2)?, max_stale_secs_override:r.get(3)?, min_dwell_secs_override:r.get(4)?, normal_dwell_secs_override:r.get(5)?, max_dwell_secs_override:r.get(6)?, allow_remote_override:r.get::<_,Option<i64>>(7)?.map(|v|v!=0), prefer_local_override:r.get::<_,Option<i64>>(8)?.map(|v|v!=0), preemptible_override:r.get::<_,Option<i64>>(9)?.map(|v|v!=0), reserve_for_recording_override:r.get::<_,Option<i64>>(10)?.map(|v|v!=0) })).optional()?.unwrap_or(PhysicalTunerEpgSettings { physical_tuner_id:id, ..Default::default() }))
    }
    pub fn update_physical_tuner_epg_settings(&self, c: &PhysicalTunerEpgSettings) -> Result<()> {
        self.connection().execute("INSERT INTO physical_tuner_epg_settings(physical_tuner_id,enabled_override,preset_id,target_refresh_secs_override,max_stale_secs_override,min_dwell_secs_override,normal_dwell_secs_override,max_dwell_secs_override,allow_remote_override,prefer_local_override,preemptible_override,reserve_for_recording_override,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,strftime('%s','now')) ON CONFLICT(physical_tuner_id) DO UPDATE SET enabled_override=?2,preset_id=?3,target_refresh_secs_override=?4,max_stale_secs_override=?5,min_dwell_secs_override=?6,normal_dwell_secs_override=?7,max_dwell_secs_override=?8,allow_remote_override=?9,prefer_local_override=?10,preemptible_override=?11,reserve_for_recording_override=?12,updated_at=strftime('%s','now')", params![c.physical_tuner_id,c.enabled_override.map(b),c.preset_id,c.target_refresh_secs_override,c.max_stale_secs_override,c.min_dwell_secs_override,c.normal_dwell_secs_override,c.max_dwell_secs_override,c.allow_remote_override.map(b),c.prefer_local_override.map(b),c.preemptible_override.map(b),c.reserve_for_recording_override.map(b)])?;
        Ok(())
    }
    pub fn get_epg_effective(&self, tuner_id: Option<i64>) -> Result<EffectiveEpgScanConfig> {
        let g = self.get_epg_global_settings()?;
        let physical = tuner_id.and_then(|id| self.get_physical_tuner_epg_settings(id).ok());
        let p=tuner_id.and_then(|id|self.connection().query_row("SELECT preset_id FROM physical_tuner_epg_settings WHERE physical_tuner_id=?",[id],|r|r.get::<_,Option<i64>>(0)).optional().ok().flatten()).flatten().or(g.selected_preset_id);
        let preset = p.and_then(|id| {
            self.list_epg_presets()
                .ok()?
                .into_iter()
                .find(|x| x.id == id)
        });
        let mut e = g.clone();
        let mut src = serde_json::Map::new();
        if let Some(ref x) = physical {
            if let Some(v) = x.enabled_override {
                e.enabled = v;
                src.insert("enabled".into(), serde_json::json!("tunerOverride"));
            }
        }
        macro_rules! pick {
            ($f:ident) => {
                if let Some(ref x) = preset {
                    if let Some(v) = x.$f {
                        e.$f = v;
                        src.insert(stringify!($f).into(), serde_json::json!("preset"));
                    } else {
                        src.insert(stringify!($f).into(), serde_json::json!("global"));
                    }
                } else {
                    src.insert(stringify!($f).into(), serde_json::json!("global"));
                }
            };
        }
        pick!(target_refresh_secs);
        pick!(max_stale_secs);
        pick!(min_future_coverage_hours);
        pick!(target_future_coverage_hours);
        pick!(min_dwell_secs);
        pick!(normal_dwell_secs);
        pick!(max_dwell_secs);
        pick!(idle_section_timeout_secs);
        pick!(cpu_soft_limit_percent);
        pick!(cpu_hard_limit_percent);
        if let Some(ref x) = physical {
            if let Some(v) = x.target_refresh_secs_override {
                e.target_refresh_secs = v;
                src.insert(
                    "target_refresh_secs".into(),
                    serde_json::json!("tunerOverride"),
                );
            }
            if let Some(v) = x.max_stale_secs_override {
                e.max_stale_secs = v;
                src.insert("max_stale_secs".into(), serde_json::json!("tunerOverride"));
            }
            if let Some(v) = x.min_dwell_secs_override {
                e.min_dwell_secs = v;
                src.insert("min_dwell_secs".into(), serde_json::json!("tunerOverride"));
            }
            if let Some(v) = x.normal_dwell_secs_override {
                e.normal_dwell_secs = v;
                src.insert(
                    "normal_dwell_secs".into(),
                    serde_json::json!("tunerOverride"),
                );
            }
            if let Some(v) = x.max_dwell_secs_override {
                e.max_dwell_secs = v;
                src.insert("max_dwell_secs".into(), serde_json::json!("tunerOverride"));
            }
        }
        Ok(EffectiveEpgScanConfig {
            effective: e,
            source: serde_json::Value::Object(src),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ProgramUpsert;

    #[test]
    fn epg_defaults_are_bootstrapped_in_db() {
        let db = Database::open_in_memory().unwrap();
        let global = db.get_epg_global_settings().unwrap();
        assert!(global.enabled);
        assert_eq!(global.target_refresh_secs, 21_600);
        assert_eq!(db.list_epg_presets().unwrap().len(), 7);
    }

    #[test]
    fn epg_validation_rejects_inconsistent_limits() {
        let db = Database::open_in_memory().unwrap();
        let mut global = db.get_epg_global_settings().unwrap();
        global.normal_dwell_secs = 10;
        assert!(db.update_epg_global_settings(&global).is_err());
    }

    #[test]
    fn coverage_is_reconciled_from_program_rows() {
        let mut db = Database::open_in_memory().unwrap();
        db.upsert_programs(&[ProgramUpsert {
            nid: 1,
            sid: 2,
            tsid: 3,
            event_id: 4,
            start_at: 10_000,
            duration_secs: 600,
            free_ca_mode: false,
            name: Some("test".into()),
            description: None,
            extended: None,
            genre: None,
            updated_at: 9_000,
        }])
        .unwrap();
        assert_eq!(db.refresh_epg_coverage().unwrap(), Some(10_600));
        assert_eq!(
            db.connection()
                .query_row(
                    "SELECT coverage_until FROM epg_scan_states WHERE network_id=1 AND tsid=3",
                    [],
                    |r| { r.get::<_, Option<i64>>(0) }
                )
                .unwrap(),
            Some(10_600)
        );
    }
}
