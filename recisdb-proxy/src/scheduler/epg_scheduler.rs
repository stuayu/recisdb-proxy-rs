//! Active EPG collection scheduler.
//!
//! Policy is deliberately pure and execution is deliberately small: channel
//! arbitration still belongs to `tuner::acquire::acquire`, which owns the
//! `SlotPermit` and all policy/preemption side effects.

use crate::{
    database::{epg_reason, EpgGlobalSettings, EpgReasonCode, EpgScanState},
    server::listener::DatabaseHandle,
    tuner::{
        acquire::{self, AcquireRequest},
        ChannelKey, TunerPool,
    },
};
use recisdb_protocol::{broadcast_region::classify_nid, BroadcastType};
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
    Backoff,
    NotDue,
}

impl EpgScanDecision {
    pub fn reason_code(self) -> Option<EpgReasonCode> {
        match self {
            Self::Disabled => Some(EpgReasonCode::Disabled),
            Self::SoftCpuLimit => Some(EpgReasonCode::CpuSoftLimit),
            Self::AtCapacity => Some(EpgReasonCode::NoTunerAvailable),
            Self::Backoff => Some(EpgReasonCode::Backoff),
            Self::NotDue => Some(EpgReasonCode::NotDue),
            Self::Start => None,
        }
    }
}

fn decision_details(
    decision: EpgScanDecision,
    config: &EpgGlobalSettings,
    active: usize,
    cpu: u32,
    now: i64,
    next: Option<i64>,
    network_id: u16,
    tsid: u16,
) -> serde_json::Value {
    let mut additional_codes = Vec::new();
    if cpu as i64 >= config.cpu_soft_limit_percent && decision != EpgScanDecision::SoftCpuLimit {
        additional_codes.push(EpgReasonCode::CpuSoftLimit);
    }
    if active >= config.max_concurrent_scans.max(1) as usize
        && decision != EpgScanDecision::AtCapacity
    {
        additional_codes.push(EpgReasonCode::NoTunerAvailable);
    }
    if next.is_some_and(|at| at > now) && decision != EpgScanDecision::Backoff {
        additional_codes.push(EpgReasonCode::Backoff);
    }
    serde_json::json!({
        "network_id": network_id,
        "tsid": tsid,
        "cpu_percent": cpu,
        "next_eligible_at": next,
        "active_scans": active,
        "additional_codes": additional_codes,
    })
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
    if next.is_some_and(|at| at > now) {
        return EpgScanDecision::Backoff;
    }
    let target_covered =
        coverage_until.is_some_and(|at| at >= now + config.target_future_coverage_hours * 3600);
    let fresh = last_eit_received_at.is_some_and(|at| at + config.max_stale_secs > now);
    if (target_covered && fresh) || next.is_some_and(|at| at > now) {
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
        let (config, states) = {
            let db = self.database.lock().await;
            db.refresh_epg_coverage()?;
            (db.get_epg_global_settings()?, db.get_epg_scan_states()?)
        };
        let now = chrono::Utc::now().timestamp();
        let candidate = {
            let db = self.database.lock().await;
            let drivers = db.get_all_bon_drivers()?;
            let mut targets = Vec::new();
            for driver in &drivers {
                for channel in db.get_enabled_channels_by_bon_driver(driver.id)? {
                    if targets.iter().any(|target: &EpgTarget| {
                        target.network_id == channel.nid && target.tsid == channel.tsid
                    }) {
                        continue;
                    }
                    let state = states.iter().find(|state| {
                        state.network_id == channel.nid && state.tsid == channel.tsid
                    });
                    targets.push(EpgTarget::from_state(channel.nid, channel.tsid, state));
                }
            }
            let Some(target) = select_next_target(&targets, now, &config) else {
                return Ok(());
            };
            drivers.iter().find_map(|driver| {
                db.get_enabled_channels_by_bon_driver(driver.id)
                    .ok()?
                    .into_iter()
                    .find(|channel| channel.nid == target.network_id && channel.tsid == target.tsid)
                    .map(|channel| (driver.clone(), channel))
            })
        };
        let Some((driver, channel)) = candidate else {
            return Ok(());
        };
        let state = states
            .iter()
            .find(|state| state.network_id == channel.nid && state.tsid == channel.tsid);
        let cpu = cpu_percent();
        let decision = decide(
            &config,
            self.active.load(Ordering::SeqCst),
            cpu,
            now,
            state.and_then(|s| s.next_eligible_at),
            state.and_then(|s| s.coverage_until),
            state.and_then(|s| s.last_eit_received_at),
        );
        if decision != EpgScanDecision::Start {
            if let Some(code) = decision.reason_code() {
                let record_history = !matches!(decision, EpgScanDecision::NotDue);
                let reason = epg_reason(
                    code,
                    decision_details(
                        decision,
                        &config,
                        self.active.load(Ordering::SeqCst),
                        cpu,
                        now,
                        state.and_then(|s| s.next_eligible_at),
                        channel.nid,
                        channel.tsid,
                    ),
                );
                let db = self.database.lock().await;
                db.record_epg_deferred(channel.nid, channel.tsid, &reason, record_history)?;
            }
            return Ok(());
        }
        let Some((space, number)) = channel.bon_space.zip(channel.bon_channel) else {
            let reason = epg_reason(
                EpgReasonCode::NoCompatibleTuner,
                serde_json::json!({"network_id": channel.nid, "tsid": channel.tsid}),
            );
            let db = self.database.lock().await;
            db.record_epg_deferred(channel.nid, channel.tsid, &reason, true)?;
            return Ok(());
        };
        let key = ChannelKey::space_channel(driver.dll_path.clone(), space, number);
        let history = {
            let db = self.database.lock().await;
            db.epg_scan_started(
                driver.id,
                channel.nid,
                channel.tsid,
                &epg_reason(EpgReasonCode::Scheduled, serde_json::json!({})),
            )?
        };
        self.active.fetch_add(1, Ordering::SeqCst);
        let result = self.scan_one(&config, key, channel.id).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let db = self.database.lock().await;
        match result {
            Ok(()) => {
                let _ = db.epg_scan_finished(history, "completed", channel.nid, channel.tsid, None);
            }
            Err(e) => {
                let message = e.to_string();
                let code = if message == "EPG scan interrupted: CPU hard limit reached" {
                    EpgReasonCode::CpuHardLimit
                } else {
                    EpgReasonCode::ScanFailed
                };
                let reason = epg_reason(
                    code,
                    serde_json::json!({
                        "network_id": channel.nid,
                        "tsid": channel.tsid,
                        "message": message,
                        "cpu_percent": cpu_percent(),
                    }),
                );
                let _ = db.epg_scan_finished(
                    history,
                    "failed",
                    channel.nid,
                    channel.tsid,
                    Some(&reason),
                );
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

#[derive(Debug, Clone, Copy)]
struct EpgTarget {
    network_id: u16,
    tsid: u16,
    broadcast_type: BroadcastType,
    coverage_until: Option<i64>,
    last_eit_received_at: Option<i64>,
    next_eligible_at: Option<i64>,
    failure_count: i64,
}

impl EpgTarget {
    fn from_state(network_id: u16, tsid: u16, state: Option<&EpgScanState>) -> Self {
        Self {
            network_id,
            tsid,
            broadcast_type: classify_nid(network_id).0,
            coverage_until: state.and_then(|s| s.coverage_until),
            last_eit_received_at: state.and_then(|s| s.last_eit_received_at),
            next_eligible_at: state.and_then(|s| s.next_eligible_at),
            failure_count: state.map_or(0, |s| s.failure_count),
        }
    }
}

fn select_next_target(
    targets: &[EpgTarget],
    now: i64,
    config: &EpgGlobalSettings,
) -> Option<EpgTarget> {
    targets
        .iter()
        .copied()
        .filter(|target| target_needs_scan(target, now, config))
        .min_by_key(|target| {
            let coverage_missing = !target
                .coverage_until
                .is_some_and(|until| until >= now + config.target_future_coverage_hours * 3600);
            let stale = !target
                .last_eit_received_at
                .is_some_and(|at| at + config.max_stale_secs > now);
            (
                !coverage_missing,
                !stale,
                target.failure_count,
                target.coverage_until.unwrap_or(i64::MIN),
            )
        })
}

/// Satellite EIT may populate another multiplex's `(NID, TSID)` directly.
/// Therefore a satellite target with sufficient retained coverage is skipped;
/// terrestrial targets remain independently eligible per physical TS.
fn target_needs_scan(target: &EpgTarget, now: i64, config: &EpgGlobalSettings) -> bool {
    let covered = target
        .coverage_until
        .is_some_and(|until| until >= now + config.target_future_coverage_hours * 3600);
    match target.broadcast_type {
        BroadcastType::Terrestrial => {
            let fresh = target
                .last_eit_received_at
                .is_some_and(|at| at + config.max_stale_secs > now);
            !(covered && fresh)
        }
        // For satellite multiplexes, Other-TS EIT may have supplied this
        // target without a direct tune. Retained coverage is sufficient.
        BroadcastType::BS | BroadcastType::CS | BroadcastType::FourK => !covered,
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn cpu_percent() -> u32 {
    let mut loads = [0.0_f64; 1];
    let count = unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) };
    if count != 1 {
        return 0;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    ((loads[0] / cores) * 100.0).round().clamp(0.0, 100.0) as u32
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cpu_percent() -> u32 {
    0
}

pub fn cpu_limit_source() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux:/proc/loadavg"
    }
    #[cfg(target_os = "macos")]
    {
        "macos:getloadavg"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "unavailable:cpu limit disabled"
    }
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
    fn policy_reason_codes_cover_deferred_branches() {
        let mut disabled = config();
        disabled.enabled = false;
        assert_eq!(
            decide(&disabled, 0, 0, 10, None, None, None).reason_code(),
            Some(EpgReasonCode::Disabled)
        );
        assert_eq!(
            decide(&config(), 0, 70, 10, None, None, None).reason_code(),
            Some(EpgReasonCode::CpuSoftLimit)
        );
        assert_eq!(
            decide(&config(), 1, 0, 10, None, None, None).reason_code(),
            Some(EpgReasonCode::NoTunerAvailable)
        );
        assert_eq!(
            decide(&config(), 0, 0, 10, Some(100), None, None).reason_code(),
            Some(EpgReasonCode::Backoff)
        );
    }
    #[test]
    fn policy_starts_when_due() {
        assert_eq!(
            decide(&config(), 0, 0, 10, None, None, None),
            EpgScanDecision::Start
        )
    }

    #[test]
    fn every_decision_reason_is_serializable() {
        for decision in [
            EpgScanDecision::Start,
            EpgScanDecision::Disabled,
            EpgScanDecision::SoftCpuLimit,
            EpgScanDecision::AtCapacity,
            EpgScanDecision::Backoff,
            EpgScanDecision::NotDue,
        ] {
            if let Some(code) = decision.reason_code() {
                let value = serde_json::to_value(crate::database::EpgReason {
                    code,
                    details: serde_json::json!({}),
                })
                .unwrap();
                assert!(value.get("code").is_some());
            }
        }
    }

    #[test]
    fn target_selection_prefers_missing_coverage_and_skips_backoff() {
        let targets = [
            EpgTarget {
                network_id: 1,
                tsid: 1,
                broadcast_type: BroadcastType::Terrestrial,
                coverage_until: Some(10 + 168 * 3600),
                last_eit_received_at: Some(10),
                next_eligible_at: None,
                failure_count: 0,
            },
            EpgTarget {
                network_id: 2,
                tsid: 2,
                broadcast_type: BroadcastType::Terrestrial,
                coverage_until: None,
                last_eit_received_at: None,
                next_eligible_at: None,
                failure_count: 0,
            },
            EpgTarget {
                network_id: 3,
                tsid: 3,
                broadcast_type: BroadcastType::Terrestrial,
                coverage_until: None,
                last_eit_received_at: None,
                next_eligible_at: Some(100),
                failure_count: 0,
            },
        ];
        let selected = select_next_target(&targets, 10, &config()).unwrap();
        assert_eq!((selected.network_id, selected.tsid), (2, 2));
    }

    #[test]
    fn target_selection_keeps_terrestrial_transport_streams_independent() {
        let targets = [
            EpgTarget {
                network_id: 0x7fe8,
                tsid: 1,
                broadcast_type: BroadcastType::Terrestrial,
                coverage_until: Some(10 + 168 * 3600),
                last_eit_received_at: Some(10),
                next_eligible_at: None,
                failure_count: 0,
            },
            EpgTarget {
                network_id: 0x7fe8,
                tsid: 2,
                broadcast_type: BroadcastType::Terrestrial,
                coverage_until: None,
                last_eit_received_at: None,
                next_eligible_at: None,
                failure_count: 0,
            },
        ];
        assert_eq!(select_next_target(&targets, 10, &config()).unwrap().tsid, 2);
    }

    #[test]
    fn target_selection_skips_covered_bs_other_ts() {
        let target = EpgTarget {
            network_id: 4,
            tsid: 1,
            broadcast_type: BroadcastType::BS,
            coverage_until: Some(10 + 168 * 3600),
            last_eit_received_at: Some(10),
            next_eligible_at: None,
            failure_count: 0,
        };
        assert!(!target_needs_scan(&target, 10, &config()));
        assert!(select_next_target(&[target], 10, &config()).is_none());
    }

    #[test]
    fn target_selection_keeps_uncovered_bs_other_ts_eligible() {
        let target = EpgTarget {
            network_id: 4,
            tsid: 2,
            broadcast_type: BroadcastType::BS,
            coverage_until: None,
            last_eit_received_at: None,
            next_eligible_at: None,
            failure_count: 0,
        };
        assert!(select_next_target(&[target], 10, &config()).is_some());
    }
}
