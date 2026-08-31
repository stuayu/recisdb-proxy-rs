//! Active EPG collection scheduler.
//!
//! Policy is deliberately pure and execution is deliberately small: channel
//! arbitration still belongs to `tuner::acquire::acquire`, which owns the
//! `SlotPermit` and all policy/preemption side effects.

use crate::{
    database::{BonDriverRecord, ChannelRecord, EpgGlobalSettings},
    server::listener::DatabaseHandle,
    tuner::{
        acquire::{self, AcquireRequest},
        ChannelKey, TunerPool,
    },
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::Mutex,
    time::{interval, timeout},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpgScanDecision {
    Start,
    Disabled,
    SoftCpuLimit,
    AtCapacity,
    NotDue,
}

pub fn decide(
    config: &EpgGlobalSettings,
    active: usize,
    cpu_percent: u32,
    now: i64,
    next: Option<i64>,
    coverage_until: Option<i64>,
    last_eit_received_at: Option<i64>,
) -> EpgScanDecision {
    if !config.enabled {
        return EpgScanDecision::Disabled;
    }
    if cpu_percent as i64 >= config.cpu_soft_limit_percent {
        return EpgScanDecision::SoftCpuLimit;
    }
    if active >= config.max_concurrent_scans.max(1) as usize {
        return EpgScanDecision::AtCapacity;
    }
    let target_covered =
        coverage_until.is_some_and(|at| at >= now + config.target_future_coverage_hours * 3600);
    let fresh = last_eit_received_at.is_some_and(|at| at + config.max_stale_secs > now);
    if target_covered && fresh || next.is_some_and(|at| at > now) {
        return EpgScanDecision::NotDue;
    }
    EpgScanDecision::Start
}

pub struct EpgScanScheduler {
    database: DatabaseHandle,
    pool: Arc<TunerPool>,
    active: Arc<AtomicUsize>,
    stopped: Arc<Mutex<bool>>,
}

impl EpgScanScheduler {
    pub fn new(database: DatabaseHandle, pool: Arc<TunerPool>) -> Self {
        Self {
            database,
            pool,
            active: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::new(Mutex::new(false)),
        }
    }
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }
    pub async fn stop(&self) {
        *self.stopped.lock().await = true;
    }
    async fn run(&self) {
        // Let EpgWriter install its broadcast subscriber before the first
        // scheduled acquisition. Runtime settings remain DB-backed.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut tick = interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if *self.stopped.lock().await {
                break;
            }
            if let Err(e) = self.evaluate().await {
                log::warn!("EPG scheduler evaluation failed: {}", e)
            }
        }
    }
    async fn evaluate(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (config, next, coverage, last_event) = {
            let db = self.database.lock().await;
            let coverage = db.refresh_epg_coverage()?;
            let last_event = db.epg_last_event_time()?;
            (
                db.get_epg_global_settings()?,
                db.epg_next_eligible()?,
                coverage,
                last_event,
            )
        };
        let decision = decide(
            &config,
            self.active.load(Ordering::SeqCst),
            cpu_percent(),
            chrono::Utc::now().timestamp(),
            next,
            coverage,
            last_event,
        );
        if decision != EpgScanDecision::Start {
            return Ok(());
        }
        let candidate = {
            let db = self.database.lock().await;
            let drivers = db.get_all_bon_drivers()?;
            first_candidate(&db, &drivers)
        };
        let Some((driver, channel)) = candidate else {
            return Ok(());
        };
        let Some((space, number)) = channel.bon_space.zip(channel.bon_channel) else {
            return Ok(());
        };
        let key = ChannelKey::space_channel(driver.dll_path.clone(), space, number);
        let history = {
            let db = self.database.lock().await;
            db.epg_scan_started(driver.id, "scheduled")?
        };
        self.active.fetch_add(1, Ordering::SeqCst);
        let result = self.scan_one(&config, key, channel.id).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let db = self.database.lock().await;
        match result {
            Ok(()) => {
                let _ = db.epg_scan_finished(history, "completed", driver.id, None);
            }
            Err(e) => {
                let _ = db.epg_scan_finished(history, "failed", driver.id, Some(&e.to_string()));
            }
        }
        let _ = db.refresh_epg_coverage();
        Ok(())
    }
    async fn scan_one(
        &self,
        config: &EpgGlobalSettings,
        key: ChannelKey,
        channel_id: i64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let outcome = acquire::acquire(
            &self.pool,
            &self.database,
            AcquireRequest {
                candidates: vec![key],
                priority: -1000,
                exclusive: false,
                client_host: "epg-scheduler".into(),
                bondriver_version: 2,
                carried_permit: None,
                warm: None,
                own_key: None,
                own_key_will_free_slot: false,
            },
        )
        .await?;
        let mut subscription = outcome.tuner.subscribe();
        let started = tokio::time::Instant::now();
        let mut useful = false;
        loop {
            if cpu_percent() as i64 >= config.cpu_hard_limit_percent {
                return Err("EPG scan interrupted: CPU hard limit reached".into());
            }
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_secs(config.max_dwell_secs as u64) {
                break;
            }
            let wait_for = Duration::from_secs(config.idle_section_timeout_secs.max(1) as u64);
            match timeout(wait_for, subscription.recv()).await {
                Ok(Ok(_)) => {
                    useful = true;
                    if elapsed >= Duration::from_secs(config.min_dwell_secs as u64) && useful {
                        break;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {
                    if elapsed >= Duration::from_secs(config.min_dwell_secs as u64) {
                        break;
                    }
                }
            }
        }
        if !useful {
            return Err("no TS data during EPG dwell".into());
        }
        let _ = channel_id;
        Ok(())
    }
}

fn first_candidate(
    db: &crate::database::Database,
    drivers: &[BonDriverRecord],
) -> Option<(BonDriverRecord, ChannelRecord)> {
    drivers
        .iter()
        .filter_map(|d| {
            db.get_enabled_channels_by_bon_driver(d.id)
                .ok()
                .and_then(|mut c| c.drain(..).next().map(|ch| (d.clone(), ch)))
        })
        .next()
}

#[cfg(unix)]
fn cpu_percent() -> u32 {
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    ((load / cores) * 100.0).round().clamp(0.0, 100.0) as u32
}

#[cfg(not(unix))]
fn cpu_percent() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> EpgGlobalSettings {
        EpgGlobalSettings {
            enabled: true,
            scheduler_interval_secs: 1,
            target_refresh_secs: 1,
            max_stale_secs: 2,
            min_future_coverage_hours: 1,
            target_future_coverage_hours: 2,
            startup_delay_secs: 0,
            startup_jitter_secs: 0,
            min_dwell_secs: 1,
            normal_dwell_secs: 2,
            max_dwell_secs: 3,
            idle_section_timeout_secs: 1,
            max_concurrent_scans: 1,
            reserve_tuners: false,
            prefer_local: true,
            allow_remote: false,
            preemptible: true,
            cpu_soft_limit_percent: 70,
            cpu_hard_limit_percent: 90,
            remote_prefer_metadata_execution: true,
            remote_allow_ts_transport: false,
            selected_preset_id: None,
        }
    }
    #[test]
    fn policy_blocks_soft_cpu() {
        assert_eq!(
            decide(&config(), 0, 70, 10, None, None, None),
            EpgScanDecision::SoftCpuLimit
        )
    }
    #[test]
    fn policy_starts_when_due() {
        assert_eq!(
            decide(&config(), 0, 0, 10, None, None, None),
            EpgScanDecision::Start
        )
    }
}
