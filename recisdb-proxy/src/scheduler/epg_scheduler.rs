//! Active EPG collection scheduler.
//!
//! Policy is deliberately pure and execution is deliberately small: channel
//! arbitration still belongs to `tuner::acquire::acquire`, which owns the
//! `SlotPermit` and all policy/preemption side effects.

use super::epg_dwell::{
    evaluate_dwell, mux_reached_target, DwellObservation, DwellVerdict, EpgDwellConfig,
};
use crate::node::{MuxLeaseGuard, MuxLeaseManager, NodeTransportState};
use crate::{
    database::{epg_reason, EpgGlobalSettings, EpgReasonCode, EpgScanState, EpgScanStatus},
    server::listener::DatabaseHandle,
    tuner::{
        acquire::{self, AcquireError, AcquireRequest},
        ChannelKey, TunerPool,
    },
};

#[derive(Debug, thiserror::Error)]
enum EpgScanError {
    #[error(transparent)]
    Acquire(#[from] AcquireError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpgScanOutcome {
    pub status: EpgScanStatus,
    pub elapsed_secs: i64,
    pub sections_seen: u64,
    pub services_total: usize,
    pub services_complete: usize,
    pub coverage_before: Option<i64>,
    pub coverage_after: Option<i64>,
}

fn startup_wait_secs(delay_secs: i64, jitter_secs: i64, random_value: u32) -> u64 {
    let base = delay_secs.max(1) as u64;
    let jitter = jitter_secs.max(0) as u64;
    base + u64::from(random_value) % (jitter + 1)
}

fn startup_random_value() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0)
}

fn full_evaluation_is_due(
    last_evaluation: Option<Instant>,
    scheduler_interval_secs: i64,
    now: Instant,
) -> bool {
    last_evaluation.is_none_or(|last| {
        now.duration_since(last) >= Duration::from_secs(scheduler_interval_secs.max(1) as u64)
    })
}
use recisdb_protocol::{broadcast_region::classify_nid, BroadcastType};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{watch, Notify},
    time::{interval, timeout},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpgScanDecision {
    Start,
    Disabled,
    SoftCpuLimit,
    AtCapacity,
    Backoff,
    AutoTunerScanDisabled,
    NotDue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpgExecutionPath {
    Local,
    RemoteMetadata,
    RemoteTs,
}

/// Select transport without inspecting names, files, or live state.
pub fn choose_execution_path(
    config: &EpgGlobalSettings,
    local_available: bool,
    remote_available: bool,
) -> EpgExecutionPath {
    if config.prefer_local && local_available {
        return EpgExecutionPath::Local;
    }
    if config.allow_remote && remote_available {
        if config.remote_prefer_metadata_execution {
            return EpgExecutionPath::RemoteMetadata;
        }
        if config.remote_allow_ts_transport {
            return EpgExecutionPath::RemoteTs;
        }
    }
    EpgExecutionPath::Local
}

impl EpgScanDecision {
    pub fn reason_code(self) -> Option<EpgReasonCode> {
        match self {
            Self::Disabled => Some(EpgReasonCode::Disabled),
            Self::SoftCpuLimit => Some(EpgReasonCode::CpuSoftLimit),
            Self::AtCapacity => Some(EpgReasonCode::NoTunerAvailable),
            Self::Backoff => Some(EpgReasonCode::Backoff),
            Self::AutoTunerScanDisabled => Some(EpgReasonCode::AutoTunerScanDisabled),
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
    last_complete_at: Option<i64>,
) -> EpgScanDecision {
    if !config.enabled {
        return EpgScanDecision::Disabled;
    }
    if !config.auto_tuner_scan_enabled {
        return EpgScanDecision::AutoTunerScanDisabled;
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
    // target_refresh is a soft refresh interval; max_stale is the hard limit
    // after which even recently sufficient coverage cannot be trusted.
    let stale = last_complete_at.is_none_or(|at| at + config.target_refresh_secs <= now);
    let hard_stale = last_eit_received_at.is_none_or(|at| at + config.max_stale_secs <= now);
    if target_covered && !stale && !hard_stale {
        return EpgScanDecision::NotDue;
    }
    EpgScanDecision::Start
}

pub struct EpgScanScheduler {
    database: DatabaseHandle,
    pool: Arc<TunerPool>,
    active: Arc<AtomicUsize>,
    stopped: watch::Sender<bool>,
    notify: Arc<Notify>,
    in_flight: Arc<StdMutex<HashSet<(u16, u16)>>>,
    mux_leases: Arc<MuxLeaseManager>,
    remote_state: Arc<tokio::sync::RwLock<Option<Arc<NodeTransportState>>>>,
}

struct ActiveScanGuard {
    active: Arc<AtomicUsize>,
}

impl ActiveScanGuard {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self { active }
    }
}

impl Drop for ActiveScanGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct InFlightGuard {
    in_flight: Arc<StdMutex<HashSet<(u16, u16)>>>,
    mux: (u16, u16),
    notify: Arc<Notify>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        in_flight.remove(&self.mux);
        self.notify.notify_one();
    }
}

impl EpgScanScheduler {
    pub fn new(
        database: DatabaseHandle,
        pool: Arc<TunerPool>,
        mux_leases: Arc<MuxLeaseManager>,
    ) -> Self {
        Self {
            database,
            pool,
            active: Arc::new(AtomicUsize::new(0)),
            stopped: watch::channel(false).0,
            notify: Arc::new(Notify::new()),
            in_flight: Arc::new(StdMutex::new(HashSet::new())),
            mux_leases,
            remote_state: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    pub async fn set_remote_state(&self, state: Arc<NodeTransportState>) {
        *self.remote_state.write().await = Some(state);
    }
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }
    pub async fn stop(&self) {
        let _ = self.stopped.send(true);
        self.notify.notify_waiters();
    }
    async fn run(self: &Arc<Self>) {
        // Let EpgWriter install its broadcast subscriber before the first
        // scheduled acquisition. Runtime settings remain DB-backed.
        let startup = self.database.lock().await.get_epg_global_settings();
        let (delay_secs, jitter_secs) = startup
            .map(|config| (config.startup_delay_secs, config.startup_jitter_secs))
            .unwrap_or((1, 0));
        let startup_wait = Duration::from_secs(startup_wait_secs(
            delay_secs,
            jitter_secs,
            startup_random_value(),
        ));
        let mut tick = interval(Duration::from_secs(5));
        let mut stopped = self.stopped.subscribe();
        tokio::select! {
            _ = tokio::time::sleep(startup_wait) => {}
            result = stopped.changed() => {
                if result.is_err() || *stopped.borrow() { return; }
            }
        }
        let mut last_evaluation = None;
        let mut scheduler_interval_secs = 300;
        loop {
            let tick_woke = tokio::select! {
                _ = tick.tick() => true,
                _ = self.notify.notified() => false,
                result = stopped.changed() => {
                    if result.is_err() || *stopped.borrow() { break; }
                    continue;
                }
            };
            if *stopped.borrow() {
                break;
            }
            if tick_woke {
                if !full_evaluation_is_due(last_evaluation, scheduler_interval_secs, Instant::now())
                {
                    continue;
                }
            }
            match self.evaluate().await {
                Ok(interval) => scheduler_interval_secs = interval,
                Err(e) => log::warn!("EPG scheduler evaluation failed: {}", e),
            }
            last_evaluation = Some(Instant::now());
        }
    }
    async fn evaluate(self: &Arc<Self>) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let (config, states) = {
            let db = self.database.lock().await;
            db.refresh_epg_coverage()?;
            (db.get_epg_global_settings()?, db.get_epg_scan_states()?)
        };
        let now = chrono::Utc::now().timestamp();
        let candidates = {
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
            rank_targets(&targets, now, &config)
                .into_iter()
                .filter_map(|target| {
                    drivers.iter().find_map(|driver| {
                        db.get_enabled_channels_by_bon_driver(driver.id)
                            .ok()?
                            .into_iter()
                            .find(|channel| {
                                channel.nid == target.network_id && channel.tsid == target.tsid
                            })
                            .map(|channel| (driver.clone(), channel))
                    })
                })
                .collect::<Vec<_>>()
        };
        let slots = (config.max_concurrent_scans.max(1) as usize)
            .saturating_sub(self.active.load(Ordering::SeqCst));
        if slots == 0 {
            return Ok(config.scheduler_interval_secs);
        }
        let mut started = 0;
        for (driver, channel) in candidates {
            if started >= slots {
                break;
            }
            let mux_key = (channel.nid, channel.tsid);
            {
                let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                if !in_flight.insert(mux_key) {
                    continue;
                }
            }
            let effective = {
                let db = self.database.lock().await;
                db.get_epg_effective(Some(driver.id))?.effective
            };
            let state = states
                .iter()
                .find(|state| state.network_id == channel.nid && state.tsid == channel.tsid);
            let cpu = cpu_percent();
            let decision = decide(
                &effective,
                self.active.load(Ordering::SeqCst),
                cpu,
                now,
                state.and_then(|s| s.next_eligible_at),
                state.and_then(|s| s.section_coverage_until),
                state.and_then(|s| s.last_eit_received_at),
                state.and_then(|s| s.last_complete_at),
            );
            if decision != EpgScanDecision::Start {
                {
                    let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                    in_flight.remove(&mux_key);
                }
                if decision == EpgScanDecision::AtCapacity {
                    break;
                }
                if let Some(code) = decision.reason_code() {
                    let record_history = !matches!(
                        decision,
                        EpgScanDecision::NotDue | EpgScanDecision::AutoTunerScanDisabled
                    );
                    let reason = epg_reason(
                        code,
                        decision_details(
                            decision,
                            &effective,
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
                continue;
            }
            let mux = crate::node::LogicalMuxId {
                nid: channel.nid,
                tsid: channel.tsid,
            };
            let Some(lease) = self.mux_leases.try_acquire(mux) else {
                self.in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&mux_key);
                let db = self.database.lock().await;
                let reason = epg_reason(
                    EpgReasonCode::MuxLeaseUnavailable,
                    serde_json::json!({"network_id": channel.nid, "tsid": channel.tsid}),
                );
                db.record_epg_deferred(channel.nid, channel.tsid, &reason, true)?;
                continue;
            };
            let active_guard = ActiveScanGuard::new(self.active.clone());
            let in_flight_guard = InFlightGuard {
                in_flight: self.in_flight.clone(),
                mux: mux_key,
                notify: self.notify.clone(),
            };
            let scheduler = Arc::clone(self);
            let stopped = self.stopped.subscribe();
            tokio::spawn(async move {
                let _ = scheduler
                    .run_scan(
                        driver,
                        channel,
                        effective,
                        lease,
                        active_guard,
                        in_flight_guard,
                        stopped,
                    )
                    .await;
            });
            started += 1;
        }
        Ok(config.scheduler_interval_secs)
    }

    async fn run_scan(
        self: Arc<Self>,
        driver: crate::database::BonDriverRecord,
        channel: crate::database::ChannelRecord,
        config: EpgGlobalSettings,
        _mux_lease: MuxLeaseGuard,
        _active_guard: ActiveScanGuard,
        _in_flight_guard: InFlightGuard,
        mut stopped: watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if *stopped.borrow() {
            return Ok(());
        }
        let remote_available = if config.allow_remote {
            let db = self.database.lock().await;
            let store = crate::node::NodeStore::new(&db)?;
            store
                .remote_routes_for(crate::node::LogicalMuxId {
                    nid: channel.nid,
                    tsid: channel.tsid,
                })
                .map(|routes| !routes.is_empty())?
        } else {
            false
        };
        let execution_path = choose_execution_path(
            &config,
            channel.bon_space.zip(channel.bon_channel).is_some(),
            remote_available,
        );
        let history = {
            let db = self.database.lock().await;
            db.epg_scan_started(
                driver.id,
                channel.nid,
                channel.tsid,
                &epg_reason(EpgReasonCode::Scheduled, serde_json::json!({})),
            )?
        };
        if execution_path == EpgExecutionPath::RemoteMetadata {
            let metadata_result = tokio::select! {
                _ = stopped.changed() => {
                    let now = chrono::Utc::now().timestamp();
                    let db = self.database.lock().await;
                    let _ = db.epg_scan_finished(history, "preempted", channel.nid, channel.tsid, None);
                    let _ = db.set_epg_scan_status(channel.nid, channel.tsid, EpgScanStatus::Preempted, now);
                    return Ok(());
                }
                result = self.try_remote_metadata(&config, channel.nid, channel.tsid) => result,
            };
            if let Err(error) = metadata_result {
                let reason = epg_reason(
                    EpgReasonCode::RemoteMetadataFailed,
                    serde_json::json!({
                        "network_id": channel.nid,
                        "tsid": channel.tsid,
                        "error": error.to_string(),
                    }),
                );
                let db = self.database.lock().await;
                let _ = db.epg_scan_finished(
                    history,
                    "failed",
                    channel.nid,
                    channel.tsid,
                    Some(&reason),
                );
                let _ = db.record_epg_deferred(channel.nid, channel.tsid, &reason, true);
            } else {
                let outcome = metadata_result.unwrap();
                let status = if matches!(
                    outcome.status,
                    EpgScanStatus::Complete | EpgScanStatus::Partial
                ) {
                    "completed"
                } else if outcome.status == EpgScanStatus::Preempted {
                    "preempted"
                } else {
                    "failed"
                };
                log::info!("EPG scan nid={} tsid={} path=remote_metadata status={} elapsed={}s sections={} services={}/{} coverage={:?}->{:?}", channel.nid, channel.tsid, outcome.status.as_str(), outcome.elapsed_secs, outcome.sections_seen, outcome.services_complete, outcome.services_total, outcome.coverage_before, outcome.coverage_after);
                let db = self.database.lock().await;
                let details = outcome_details(&outcome);
                let _ = db.epg_scan_finished(
                    history,
                    status,
                    channel.nid,
                    channel.tsid,
                    if status == "completed" {
                        None
                    } else {
                        Some(&details)
                    },
                );
                let _ = db.refresh_epg_coverage();
                return Ok(());
            }
        }
        if execution_path == EpgExecutionPath::RemoteTs {
            let remote_ts_result = self
                .try_remote_ts(&config, channel.nid, channel.tsid, &mut stopped)
                .await;
            if remote_ts_result.is_err() {
                let error = remote_ts_result.unwrap_err();
                let reason = epg_reason(
                    EpgReasonCode::RemoteTsFailed,
                    serde_json::json!({
                        "network_id": channel.nid,
                        "tsid": channel.tsid,
                        "error": error.to_string(),
                    }),
                );
                let db = self.database.lock().await;
                let _ = db.epg_scan_finished(
                    history,
                    "failed",
                    channel.nid,
                    channel.tsid,
                    Some(&reason),
                );
                let _ = db.record_epg_deferred(channel.nid, channel.tsid, &reason, true);
            } else {
                let outcome = remote_ts_result.unwrap();
                let status = if matches!(
                    outcome.status,
                    EpgScanStatus::Complete | EpgScanStatus::Partial
                ) {
                    "completed"
                } else if outcome.status == EpgScanStatus::Preempted {
                    "preempted"
                } else {
                    "failed"
                };
                log::info!("EPG scan nid={} tsid={} path=remote_ts status={} elapsed={}s sections={} services={}/{} coverage={:?}->{:?}", channel.nid, channel.tsid, outcome.status.as_str(), outcome.elapsed_secs, outcome.sections_seen, outcome.services_complete, outcome.services_total, outcome.coverage_before, outcome.coverage_after);
                let db = self.database.lock().await;
                let details = outcome_details(&outcome);
                let _ = db.epg_scan_finished(
                    history,
                    status,
                    channel.nid,
                    channel.tsid,
                    if status == "completed" {
                        None
                    } else {
                        Some(&details)
                    },
                );
                let _ = db.refresh_epg_coverage();
                return Ok(());
            }
        }
        if *stopped.borrow() {
            let now = chrono::Utc::now().timestamp();
            let db = self.database.lock().await;
            let _ = db.epg_scan_finished(history, "preempted", channel.nid, channel.tsid, None);
            let _ =
                db.set_epg_scan_status(channel.nid, channel.tsid, EpgScanStatus::Preempted, now);
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
        let result = self
            .scan_one(&config, key, channel.nid, channel.tsid, &mut stopped)
            .await;
        let db = self.database.lock().await;
        match result {
            Ok(outcome) => {
                let status = if matches!(
                    outcome.status,
                    EpgScanStatus::Complete | EpgScanStatus::Partial
                ) {
                    "completed"
                } else if outcome.status == EpgScanStatus::Preempted {
                    "preempted"
                } else {
                    "failed"
                };
                let details = outcome_details(&outcome);
                log::info!("EPG scan nid={} tsid={} path=local status={} elapsed={}s sections={} services={}/{} coverage={:?}->{:?}", channel.nid, channel.tsid, outcome.status.as_str(), outcome.elapsed_secs, outcome.sections_seen, outcome.services_complete, outcome.services_total, outcome.coverage_before, outcome.coverage_after);
                let _ = db.epg_scan_finished(
                    history,
                    status,
                    channel.nid,
                    channel.tsid,
                    if status == "completed" {
                        None
                    } else {
                        Some(&details)
                    },
                );
            }
            Err(e) => {
                let (code, conflict) =
                    match &e {
                        EpgScanError::Acquire(AcquireError::AtCapacity { conflict, .. }) => {
                            let code = conflict.as_ref().map_or(
                                EpgReasonCode::NoTunerAvailable,
                                |conflict| match conflict.usage {
                                    crate::tuner::shared::TunerUsage::Record => {
                                        EpgReasonCode::PreemptedByRecord
                                    }
                                    crate::tuner::shared::TunerUsage::View => {
                                        EpgReasonCode::PreemptedByView
                                    }
                                    _ => EpgReasonCode::NoTunerAvailable,
                                },
                            );
                            (code, conflict.as_ref().map(|conflict| &conflict.tuner))
                        }
                        _ => (EpgReasonCode::ScanFailed, None),
                    };
                let message = e.to_string();
                let reason = epg_reason(
                    code,
                    serde_json::json!({
                        "network_id": channel.nid,
                        "tsid": channel.tsid,
                        "message": message,
                        "cpu_percent": cpu_percent(),
                        "conflict_tuner": conflict.map(|tuner| format!("{tuner:?}")),
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

    async fn try_remote_metadata(
        &self,
        config: &EpgGlobalSettings,
        network_id: u16,
        tsid: u16,
    ) -> Result<EpgScanOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let Some(state) = self.remote_state.read().await.clone() else {
            return Err("node transport is not ready".into());
        };
        let (local, routes) = {
            let db = self.database.lock().await;
            let store = crate::node::NodeStore::new(&db)?;
            let mux = crate::node::LogicalMuxId {
                nid: network_id,
                tsid,
            };
            (store.local_identity()?, store.remote_routes_for(mux)?)
        };
        let dwell = EpgDwellConfig::from_settings(config);
        let coverage_before = self.coverage_until(network_id, tsid).await;
        for route in routes {
            let credential = {
                let db = self.database.lock().await;
                crate::node::NodeStore::new(&db)?.credential_for(&route.node_id)?
            };
            let Some(credential) = credential else {
                continue;
            };
            let endpoints = {
                let db = self.database.lock().await;
                crate::node::NodeStore::new(&db)?.endpoints(&route.node_id)?
            };
            let client = crate::node::NodeTransportClient::new(local.node_id.clone(), credential)?;
            for endpoint in endpoints.into_iter().filter(|endpoint| endpoint.enabled) {
                let request = crate::node::RemoteEpgMetadataRequest {
                    context: crate::node::RequestContext {
                        request_id: format!("epg-{network_id:04x}-{tsid:04x}"),
                        trace_id: format!(
                            "epg-{}",
                            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                        ),
                        stream_class: recisdb_protocol::StreamClass::View,
                        claim: crate::tuner::EffectiveClaim::new(-1000, false),
                        remaining_ms: (dwell.max_dwell.as_secs() * 1000).saturating_add(30_000),
                        origin_node: local.node_id.clone(),
                        visited_nodes: vec![local.node_id.clone()],
                        hop_count: 0,
                        max_hops: 3,
                    },
                    mux: route.mux,
                    sid: None,
                    dwell_secs: dwell.max_dwell.as_secs(),
                    spent_ms: 0,
                    dwell: Some(dwell.into()),
                };
                if let Ok(reply) = client
                    .collect_epg_metadata(&endpoint.address, &request)
                    .await
                {
                    let has_programs = !reply.programs.is_empty();
                    let status = remote_metadata_status(reply.status.as_deref(), has_programs);
                    let records = reply.programs.into_iter().map(Into::into);
                    crate::tuner::epg_collector::submit_metadata_records(records);
                    let now = chrono::Utc::now().timestamp();
                    let coverage_after = self.coverage_until(network_id, tsid).await;
                    let db = self.database.lock().await;
                    let _ = db.set_epg_scan_status(network_id, tsid, status, now);
                    let _ = db.set_epg_remote_coverage(network_id, tsid, reply.coverage_until, now);
                    return Ok(EpgScanOutcome {
                        status,
                        elapsed_secs: reply.elapsed_secs.unwrap_or_default(),
                        sections_seen: reply.sections_seen.unwrap_or_default(),
                        services_total: reply.services_total.unwrap_or_default(),
                        services_complete: reply.services_complete.unwrap_or_default(),
                        coverage_before,
                        // Remote metadata contains no local EIT sections, so this
                        // remains section-derived local coverage, not submitted rows.
                        coverage_after,
                    });
                }
            }
        }
        let _ = state;
        Err("no authenticated remote metadata route responded".into())
    }

    async fn try_remote_ts(
        &self,
        config: &EpgGlobalSettings,
        network_id: u16,
        tsid: u16,
        stopped: &mut watch::Receiver<bool>,
    ) -> Result<EpgScanOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let Some(state) = self.remote_state.read().await.clone() else {
            return Err("node transport is not ready".into());
        };
        let (local, routes) = {
            let db = self.database.lock().await;
            let store = crate::node::NodeStore::new(&db)?;
            let mux = crate::node::LogicalMuxId {
                nid: network_id,
                tsid,
            };
            (store.local_identity()?, store.remote_routes_for(mux)?)
        };
        for route in routes {
            let (Some(credential), endpoints) = ({
                let db = self.database.lock().await;
                let store = crate::node::NodeStore::new(&db)?;
                (
                    store.credential_for(&route.node_id)?,
                    store.endpoints(&route.node_id)?,
                )
            }) else {
                continue;
            };
            let client = Arc::new(crate::node::NodeTransportClient::new(
                local.node_id.clone(),
                credential,
            )?);
            for endpoint in endpoints.into_iter().filter(|endpoint| endpoint.enabled) {
                let context = crate::node::RequestContext {
                    request_id: format!("epg-ts-{network_id:04x}-{tsid:04x}"),
                    trace_id: format!(
                        "epg-ts-{}",
                        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                    ),
                    stream_class: recisdb_protocol::StreamClass::View,
                    claim: crate::tuner::EffectiveClaim::new(-1000, false),
                    remaining_ms: 300_000,
                    origin_node: local.node_id.clone(),
                    visited_nodes: vec![local.node_id.clone()],
                    hop_count: 0,
                    max_hops: 3,
                };
                let Ok(remote) = crate::node::RemoteMuxStream::open(
                    client.clone(),
                    endpoint.address,
                    context,
                    route.mux,
                    None,
                    0,
                )
                .await
                else {
                    continue;
                };
                let mut subscription = remote.subscribe();
                let mut collector = crate::tuner::epg_collector::EpgCollector::new_metadata();
                let progress = collector.progress().clone();
                let coverage_before = self.coverage_until(network_id, tsid).await;
                let started = tokio::time::Instant::now();
                let dwell = EpgDwellConfig::from_settings(config);
                let mut last_progress = progress.progress_counter();
                let mut last_progress_at = started;
                let status = loop {
                    if *stopped.borrow() {
                        break EpgScanStatus::Preempted;
                    }
                    let counter = progress.progress_counter();
                    if counter != last_progress {
                        last_progress = counter;
                        last_progress_at = tokio::time::Instant::now();
                    }
                    let mux = progress.mux_completion(network_id, tsid);
                    let observation = DwellObservation {
                        elapsed: started.elapsed(),
                        since_progress: last_progress_at.elapsed(),
                        any_sections: mux.sections_seen > 0,
                        reached_target: mux_reached_target(
                            &mux,
                            chrono::Utc::now().timestamp(),
                            dwell.target_future_coverage_hours,
                        ),
                        any_service_complete: mux.services_schedule_basic_complete > 0,
                        stream_closed: false,
                        cpu_hard_limit: cpu_percent() as i64 >= config.cpu_hard_limit_percent,
                    };
                    match evaluate_dwell(&dwell, &observation) {
                        DwellVerdict::Stop(status) => break status,
                        DwellVerdict::Continue => {}
                    }
                    let stream_closed = tokio::select! {
                        _ = stopped.changed() => break EpgScanStatus::Preempted,
                        result = timeout(Duration::from_secs(1), subscription.recv()) =>
                            match result {
                                Ok(Ok(chunk)) => {
                                    collector.process_ts_chunk(&chunk);
                                    false
                                }
                                Err(_)
                                | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => false,
                                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => true,
                            }
                    };
                    if stream_closed {
                        let mux = progress.mux_completion(network_id, tsid);
                        let observation = DwellObservation {
                            elapsed: started.elapsed(),
                            since_progress: last_progress_at.elapsed(),
                            any_sections: mux.sections_seen > 0,
                            reached_target: mux_reached_target(
                                &mux,
                                chrono::Utc::now().timestamp(),
                                dwell.target_future_coverage_hours,
                            ),
                            any_service_complete: mux.services_schedule_basic_complete > 0,
                            stream_closed: true,
                            cpu_hard_limit: cpu_percent() as i64 >= config.cpu_hard_limit_percent,
                        };
                        if let DwellVerdict::Stop(status) = evaluate_dwell(&dwell, &observation) {
                            break status;
                        }
                    }
                };
                let records = collector.drain_metadata_records();
                if !records.is_empty() {
                    crate::tuner::epg_collector::submit_metadata_records(records);
                }
                let now = chrono::Utc::now().timestamp();
                self.persist_progress(&progress, now).await;
                let mux = progress.mux_completion(network_id, tsid);
                let coverage_after = self.coverage_until(network_id, tsid).await;
                let db = self.database.lock().await;
                let _ = db.set_epg_scan_status(network_id, tsid, status, now);
                drop(remote);
                return Ok(EpgScanOutcome {
                    status,
                    elapsed_secs: started.elapsed().as_secs() as i64,
                    sections_seen: mux.sections_seen,
                    services_total: mux.services_total,
                    services_complete: mux.services_schedule_basic_complete,
                    coverage_before,
                    coverage_after,
                });
            }
        }
        let _ = state;
        Err("no authenticated remote TS route produced EIT".into())
    }
    async fn scan_one(
        &self,
        config: &EpgGlobalSettings,
        key: ChannelKey,
        network_id: u16,
        tsid: u16,
        stopped: &mut watch::Receiver<bool>,
    ) -> Result<EpgScanOutcome, EpgScanError> {
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
        let progress = outcome.tuner.epg_progress();
        let coverage_before = self.coverage_until(network_id, tsid).await;
        let mut subscription = outcome.tuner.subscribe_with_claim_class(
            -1000,
            false,
            crate::tuner::shared::TunerUsage::EpgActiveScan,
        );
        let started = tokio::time::Instant::now();
        let mut last_progress = progress.progress_counter();
        let mut last_progress_at = started;
        let dwell = EpgDwellConfig::from_settings(config);
        let status = loop {
            if *stopped.borrow() {
                break EpgScanStatus::Preempted;
            }
            let counter = progress.progress_counter();
            if counter != last_progress {
                last_progress = counter;
                last_progress_at = tokio::time::Instant::now();
            }
            let mux = progress.mux_completion(network_id, tsid);
            let observation = DwellObservation {
                elapsed: started.elapsed(),
                since_progress: last_progress_at.elapsed(),
                any_sections: mux.sections_seen > 0,
                reached_target: mux_reached_target(
                    &mux,
                    chrono::Utc::now().timestamp(),
                    dwell.target_future_coverage_hours,
                ),
                any_service_complete: mux.services_schedule_basic_complete > 0,
                stream_closed: false,
                cpu_hard_limit: cpu_percent() as i64 >= config.cpu_hard_limit_percent,
            };
            match evaluate_dwell(&dwell, &observation) {
                DwellVerdict::Stop(status) => break status,
                DwellVerdict::Continue => {}
            }
            let stream_closed = tokio::select! {
                _ = stopped.changed() => break EpgScanStatus::Preempted,
                result = timeout(Duration::from_secs(1), subscription.recv()) =>
                    match result {
                        Ok(Ok(_)) | Err(_) => false,
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => false,
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => true,
                    }
            };
            if stream_closed {
                let mux = progress.mux_completion(network_id, tsid);
                let observation = DwellObservation {
                    elapsed: started.elapsed(),
                    since_progress: last_progress_at.elapsed(),
                    any_sections: mux.sections_seen > 0,
                    reached_target: mux_reached_target(
                        &mux,
                        chrono::Utc::now().timestamp(),
                        dwell.target_future_coverage_hours,
                    ),
                    any_service_complete: mux.services_schedule_basic_complete > 0,
                    stream_closed: true,
                    cpu_hard_limit: cpu_percent() as i64 >= config.cpu_hard_limit_percent,
                };
                if let DwellVerdict::Stop(status) = evaluate_dwell(&dwell, &observation) {
                    break status;
                }
                // Closed before `min_dwell`: `recv()` now returns
                // immediately every time, so without this the loop spins at
                // 100% CPU until the minimum dwell elapses.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        let now = chrono::Utc::now().timestamp();
        self.persist_progress(&progress, now).await;
        let mux = progress.mux_completion(network_id, tsid);
        let coverage_after = self.coverage_until(network_id, tsid).await;
        {
            let db = self.database.lock().await;
            let _ = db.set_epg_scan_status(network_id, tsid, status, now);
        }
        Ok(EpgScanOutcome {
            status,
            elapsed_secs: started.elapsed().as_secs() as i64,
            sections_seen: mux.sections_seen,
            services_total: mux.services_total,
            services_complete: mux.services_schedule_basic_complete,
            coverage_before,
            coverage_after,
        })
    }

    async fn coverage_until(&self, network_id: u16, tsid: u16) -> Option<i64> {
        let db = self.database.lock().await;
        db.get_epg_mux_coverage()
            .ok()?
            .into_iter()
            .find(|coverage| coverage.network_id == network_id && coverage.tsid == tsid)
            .and_then(|coverage| coverage.coverage_until)
    }

    async fn persist_progress(&self, progress: &crate::tuner::EpgProgress, now: i64) {
        let _ = crate::tuner::epg_coverage_flusher::persist_progress(&self.database, progress, now)
            .await;
    }
}

fn outcome_details(outcome: &EpgScanOutcome) -> String {
    serde_json::json!({
        "status": outcome.status.as_str(),
        "elapsed_secs": outcome.elapsed_secs,
        "sections_seen": outcome.sections_seen,
        "services_total": outcome.services_total,
        "services_complete": outcome.services_complete,
        "coverage_before": outcome.coverage_before,
        "coverage_after": outcome.coverage_after,
    })
    .to_string()
}

fn remote_metadata_status(status: Option<&str>, has_programs: bool) -> EpgScanStatus {
    status
        .and_then(EpgScanStatus::from_str_opt)
        .unwrap_or(if has_programs {
            EpgScanStatus::Partial
        } else {
            EpgScanStatus::NoData
        })
}

#[derive(Debug, Clone, Copy)]
struct EpgTarget {
    network_id: u16,
    tsid: u16,
    broadcast_type: BroadcastType,
    /// EIT section 由来の mux coverage。判定に使う。
    section_coverage_until: Option<i64>,
    /// programs 由来の補助指標。診断表示用で、判定には使わない。
    program_coverage_until: Option<i64>,
    last_eit_received_at: Option<i64>,
    last_complete_at: Option<i64>,
    next_eligible_at: Option<i64>,
    failure_count: i64,
}

impl EpgTarget {
    fn from_state(network_id: u16, tsid: u16, state: Option<&EpgScanState>) -> Self {
        Self {
            network_id,
            tsid,
            broadcast_type: classify_nid(network_id).0,
            section_coverage_until: state.and_then(|s| s.section_coverage_until),
            program_coverage_until: state.and_then(|s| s.coverage_until),
            last_eit_received_at: state.and_then(|s| s.last_eit_received_at),
            last_complete_at: state.and_then(|s| s.last_complete_at),
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
    rank_targets(targets, now, config).into_iter().next()
}

fn rank_targets(targets: &[EpgTarget], now: i64, config: &EpgGlobalSettings) -> Vec<EpgTarget> {
    let mut candidates = targets
        .iter()
        .copied()
        .filter(|target| target_needs_scan(target, now, config))
        .collect::<Vec<_>>();

    // BS/CS Other-TS EIT can fill every TS in one network from one tuned mux.
    // FourK is excluded: dantto4k-converted TS EIT does not carry other-TS data.
    let mut representatives = Vec::new();
    for network_id in candidates
        .iter()
        .filter(|target| matches!(target.broadcast_type, BroadcastType::BS | BroadcastType::CS))
        .map(|target| target.network_id)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let group = candidates
            .iter()
            .copied()
            .filter(|target| target.network_id == network_id)
            .collect::<Vec<_>>();
        if let Some(representative) = choose_satellite_representative(&group, now, config) {
            representatives.push(representative);
        }
    }
    candidates.retain(|target| {
        matches!(
            target.broadcast_type,
            BroadcastType::Terrestrial | BroadcastType::FourK
        )
    });
    candidates.extend(representatives);
    candidates.sort_by_key(|target| {
        let min_coverage_missing = !target
            .section_coverage_until
            .is_some_and(|until| until >= now + config.min_future_coverage_hours * 3600);
        let coverage_missing = !target
            .section_coverage_until
            .is_some_and(|until| until >= now + config.target_future_coverage_hours * 3600);
        let stale = !target
            .last_complete_at
            .is_some_and(|at| at + config.target_refresh_secs > now);
        (
            !min_coverage_missing,
            !coverage_missing,
            !stale,
            target.failure_count,
            target.section_coverage_until.unwrap_or(i64::MIN),
            target.tsid,
        )
    });
    candidates
}

fn target_needs_scan(target: &EpgTarget, now: i64, config: &EpgGlobalSettings) -> bool {
    let covered = target
        .section_coverage_until
        .is_some_and(|until| until >= now + config.target_future_coverage_hours * 3600);
    // target_refresh is a soft candidate interval; max_stale is a hard EIT
    // freshness threshold. Same rule applies to terrestrial, BS, CS, FourK.
    let stale = target
        .last_complete_at
        .is_none_or(|at| at + config.target_refresh_secs <= now);
    let hard_stale = target
        .last_eit_received_at
        .is_none_or(|at| at + config.max_stale_secs <= now);
    !covered || stale || hard_stale
}

/// 同じ network_id の衛星ターゲット群から、選局する代表を1つ選ぶ。
/// gain は、この mux を取ればまとめて更新できる「要スキャン TS 数」。
/// cost は現状 tsid 昇順の安定タイブレークだけ。将来は tuner 占有、CPU、
/// remote 通信、4K 変換負荷を比較対象にできる。
fn choose_satellite_representative(
    targets: &[EpgTarget],
    now: i64,
    config: &EpgGlobalSettings,
) -> Option<EpgTarget> {
    let eligible = targets
        .iter()
        .copied()
        .filter(|target| target_needs_scan(target, now, config));
    if eligible.count() == 0 {
        return None;
    }
    targets
        .iter()
        .copied()
        .filter(|target| target_needs_scan(target, now, config))
        .min_by_key(|target| {
            (
                target.failure_count,
                target.section_coverage_until.unwrap_or(i64::MIN),
                target.tsid,
            )
        })
}

/// Whole-host CPU utilisation in percent, as seen by the EPG scheduler.
///
/// Measured utilisation, not load average: the run-queue length that
/// `getloadavg`/`/proc/loadavg` report counts threads waiting on I/O too, so on
/// a busy-but-not-CPU-bound host it sits far above the real usage and deferred
/// every scan against `cpu_soft_limit_percent` forever.
///
/// 0 while the sampler has no reading yet, which lets a scan through rather
/// than blocking on a limit nothing can measure; `cpu_limit_source` reports
/// that state as unavailable.
pub(crate) fn cpu_percent() -> u32 {
    crate::metrics::system::cpu_usage_percent()
        .map(|percent| percent.round().clamp(0.0, 100.0) as u32)
        .unwrap_or(0)
}

pub fn cpu_limit_source() -> &'static str {
    if crate::metrics::system::cpu_usage_percent().is_some() {
        "sysinfo:global_cpu_usage"
    } else {
        "unavailable:cpu sample not ready"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_metadata_outcome_without_status_is_partial_when_rows_returned() {
        assert_eq!(remote_metadata_status(None, true), EpgScanStatus::Partial);
        assert_eq!(remote_metadata_status(None, false), EpgScanStatus::NoData);
    }
    fn config() -> EpgGlobalSettings {
        EpgGlobalSettings {
            enabled: true,
            auto_tuner_scan_enabled: true,
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
            prefer_local: true,
            allow_remote: false,
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
            decide(&config(), 0, 70, 10, None, None, None, None),
            EpgScanDecision::SoftCpuLimit
        )
    }

    #[test]
    fn execution_path_honors_local_and_remote_settings() {
        let mut c = config();
        c.allow_remote = false;
        c.prefer_local = false;
        c.remote_prefer_metadata_execution = true;
        assert_eq!(
            choose_execution_path(&c, false, true),
            EpgExecutionPath::Local
        );

        c.allow_remote = true;
        assert_eq!(
            choose_execution_path(&c, false, true),
            EpgExecutionPath::RemoteMetadata
        );
        c.remote_prefer_metadata_execution = false;
        c.remote_allow_ts_transport = false;
        assert_eq!(
            choose_execution_path(&c, false, true),
            EpgExecutionPath::Local
        );
        c.remote_allow_ts_transport = true;
        assert_eq!(
            choose_execution_path(&c, false, true),
            EpgExecutionPath::RemoteTs
        );
        c.prefer_local = true;
        assert_eq!(
            choose_execution_path(&c, true, true),
            EpgExecutionPath::Local
        );
    }

    #[test]
    fn policy_reason_codes_cover_deferred_branches() {
        let mut disabled = config();
        disabled.enabled = false;
        assert_eq!(
            decide(&disabled, 0, 0, 10, None, None, None, None).reason_code(),
            Some(EpgReasonCode::Disabled)
        );
        let mut auto_disabled = config();
        auto_disabled.auto_tuner_scan_enabled = false;
        assert_eq!(
            decide(&auto_disabled, 0, 0, 10, None, None, None, None).reason_code(),
            Some(EpgReasonCode::AutoTunerScanDisabled)
        );
        assert_eq!(
            decide(&config(), 0, 70, 10, None, None, None, None).reason_code(),
            Some(EpgReasonCode::CpuSoftLimit)
        );
        assert_eq!(
            decide(&config(), 1, 0, 10, None, None, None, None).reason_code(),
            Some(EpgReasonCode::NoTunerAvailable)
        );
        assert_eq!(
            decide(&config(), 0, 0, 10, Some(100), None, None, None).reason_code(),
            Some(EpgReasonCode::Backoff)
        );
    }
    #[test]
    fn policy_starts_when_due() {
        assert_eq!(
            decide(&config(), 0, 0, 10, None, None, None, None),
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
            EpgScanDecision::AutoTunerScanDisabled,
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
                section_coverage_until: Some(10 + 168 * 3600),
                program_coverage_until: None,
                last_eit_received_at: Some(10),
                last_complete_at: Some(10),
                next_eligible_at: None,
                failure_count: 0,
            },
            EpgTarget {
                network_id: 2,
                tsid: 2,
                broadcast_type: BroadcastType::Terrestrial,
                section_coverage_until: None,
                program_coverage_until: None,
                last_eit_received_at: None,
                last_complete_at: None,
                next_eligible_at: None,
                failure_count: 0,
            },
            EpgTarget {
                network_id: 3,
                tsid: 3,
                broadcast_type: BroadcastType::Terrestrial,
                section_coverage_until: None,
                program_coverage_until: None,
                last_eit_received_at: None,
                last_complete_at: None,
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
                section_coverage_until: Some(10 + 168 * 3600),
                program_coverage_until: None,
                last_eit_received_at: Some(10),
                last_complete_at: Some(10),
                next_eligible_at: None,
                failure_count: 0,
            },
            EpgTarget {
                network_id: 0x7fe8,
                tsid: 2,
                broadcast_type: BroadcastType::Terrestrial,
                section_coverage_until: None,
                program_coverage_until: None,
                last_eit_received_at: None,
                last_complete_at: None,
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
            section_coverage_until: Some(10 + 168 * 3600),
            program_coverage_until: None,
            last_eit_received_at: Some(100),
            last_complete_at: Some(100),
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
            section_coverage_until: None,
            program_coverage_until: None,
            last_eit_received_at: None,
            last_complete_at: None,
            next_eligible_at: None,
            failure_count: 0,
        };
        assert!(select_next_target(&[target], 10, &config()).is_some());
    }

    fn target(
        network_id: u16,
        tsid: u16,
        broadcast_type: BroadcastType,
        section_coverage_until: Option<i64>,
        last_complete_at: Option<i64>,
        last_eit_received_at: Option<i64>,
    ) -> EpgTarget {
        EpgTarget {
            network_id,
            tsid,
            broadcast_type,
            section_coverage_until,
            program_coverage_until: None,
            last_eit_received_at,
            last_complete_at,
            next_eligible_at: None,
            failure_count: 0,
        }
    }

    #[test]
    fn rank_targets_returns_candidates_in_priority_order() {
        let targets = [
            target(
                1,
                1,
                BroadcastType::Terrestrial,
                Some(10),
                Some(10),
                Some(10),
            ),
            target(1, 2, BroadcastType::Terrestrial, None, Some(10), Some(10)),
            target(1, 3, BroadcastType::Terrestrial, Some(10), Some(0), Some(0)),
            {
                let mut target = target(1, 4, BroadcastType::Terrestrial, None, None, None);
                target.failure_count = 2;
                target
            },
        ];
        let ranked = rank_targets(&targets, 10, &config());
        assert_eq!(
            ranked.iter().map(|target| target.tsid).collect::<Vec<_>>(),
            [3, 4, 2, 1]
        );
    }

    #[test]
    fn rank_targets_puts_targets_below_the_minimum_coverage_first() {
        let mut config = config();
        config.min_future_coverage_hours = 3;
        config.target_future_coverage_hours = 7;
        let targets = [
            target(
                1,
                1,
                BroadcastType::Terrestrial,
                Some(10 + 4 * 3600),
                Some(0),
                Some(0),
            ),
            target(
                1,
                2,
                BroadcastType::Terrestrial,
                Some(10 + 5400),
                Some(0),
                Some(0),
            ),
        ];

        let ranked = rank_targets(&targets, 10, &config);
        assert_eq!(
            ranked.iter().map(|target| target.tsid).collect::<Vec<_>>(),
            [2, 1]
        );
    }

    #[test]
    fn rank_targets_is_unchanged_when_every_target_clears_the_minimum() {
        let mut config = config();
        config.min_future_coverage_hours = 1;
        config.target_future_coverage_hours = 2;
        let targets = [
            target(
                1,
                1,
                BroadcastType::Terrestrial,
                Some(10 + 5400),
                Some(10),
                Some(10),
            ),
            target(
                1,
                2,
                BroadcastType::Terrestrial,
                Some(10 + 3600),
                Some(10),
                Some(10),
            ),
            target(
                1,
                3,
                BroadcastType::Terrestrial,
                Some(10 + 3600),
                Some(0),
                Some(0),
            ),
            {
                let mut target = target(
                    1,
                    4,
                    BroadcastType::Terrestrial,
                    Some(10 + 3600),
                    None,
                    None,
                );
                target.failure_count = 2;
                target
            },
        ];

        let ranked = rank_targets(&targets, 10, &config);
        assert_eq!(
            ranked.iter().map(|target| target.tsid).collect::<Vec<_>>(),
            [3, 4, 2, 1]
        );
    }

    #[test]
    fn rank_targets_yields_one_representative_per_satellite_network() {
        let targets = [
            target(4, 3, BroadcastType::BS, None, None, None),
            target(4, 1, BroadcastType::BS, None, None, None),
            target(4, 2, BroadcastType::BS, None, None, None),
        ];
        let ranked = rank_targets(&targets, 10, &config());
        assert_eq!(
            ranked
                .iter()
                .filter(|target| target.network_id == 4)
                .count(),
            1
        );
    }

    #[test]
    fn rank_targets_keeps_terrestrial_transport_streams_separate() {
        let targets = [
            target(0x100, 1, BroadcastType::Terrestrial, None, None, None),
            target(0x100, 2, BroadcastType::Terrestrial, None, None, None),
        ];
        let ranked = rank_targets(&targets, 10, &config());
        assert_eq!(
            ranked.iter().map(|target| target.tsid).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn active_scan_guard_decrements_on_drop() {
        let active = Arc::new(AtomicUsize::new(0));
        let guard = ActiveScanGuard::new(active.clone());
        assert_eq!(active.load(Ordering::SeqCst), 1);
        drop(guard);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn active_scan_guard_decrements_on_panic() {
        let active = Arc::new(AtomicUsize::new(0));
        let active_for_panic = active.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = ActiveScanGuard::new(active_for_panic);
            panic!("test panic");
        });
        assert!(result.is_err());
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn in_flight_set_blocks_a_second_scan_of_the_same_mux() {
        let set = Arc::new(StdMutex::new(HashSet::new()));
        let mux = (1, 2);
        assert!(set.lock().unwrap().insert(mux));
        assert!(!set.lock().unwrap().insert(mux));
    }

    #[test]
    fn decide_stops_ranking_when_at_capacity() {
        let mut config = config();
        config.max_concurrent_scans = 3;
        assert_eq!(
            decide(&config, 3, 0, 10, None, None, None, None),
            EpgScanDecision::AtCapacity
        );
    }

    #[test]
    fn stale_satellite_target_is_rescanned_even_when_covered() {
        let target = target(
            4,
            1,
            BroadcastType::BS,
            Some(10 + 168 * 3600),
            Some(0),
            Some(0),
        );
        assert!(target_needs_scan(&target, 10, &config()));
    }

    #[test]
    fn fresh_and_covered_satellite_target_is_skipped() {
        let target = target(
            4,
            1,
            BroadcastType::BS,
            Some(10 + 168 * 3600),
            Some(10),
            Some(0),
        );
        let mut config = config();
        config.target_refresh_secs = 100;
        config.max_stale_secs = 100;
        assert!(!target_needs_scan(&target, 10, &config));
    }

    #[test]
    fn program_coverage_is_not_used_for_the_decision() {
        let mut target = target(4, 1, BroadcastType::BS, None, Some(10), Some(10));
        target.program_coverage_until = Some(10 + 7 * 86400);
        assert!(target_needs_scan(&target, 10, &config()));
    }

    #[test]
    fn terrestrial_transport_streams_stay_independent() {
        let covered = target(
            0x7fe8,
            1,
            BroadcastType::Terrestrial,
            Some(10 + 168 * 3600),
            Some(100),
            Some(100),
        );
        let missing = target(0x7fe8, 2, BroadcastType::Terrestrial, None, None, None);
        assert_eq!(
            select_next_target(&[covered, missing], 10, &config())
                .unwrap()
                .tsid,
            2
        );
    }

    #[test]
    fn satellite_group_yields_one_representative_per_network() {
        let targets = [
            target(4, 1, BroadcastType::BS, None, None, None),
            target(4, 2, BroadcastType::BS, None, None, None),
            target(4, 3, BroadcastType::BS, None, None, None),
        ];
        assert_eq!(select_next_target(&targets, 10, &config()).unwrap().tsid, 1);
    }

    #[test]
    fn satellite_representative_prefers_the_most_behind_transport_stream() {
        let targets = [
            target(4, 1, BroadcastType::BS, Some(10 + 1000), None, None),
            target(4, 2, BroadcastType::BS, Some(10 + 100), None, None),
        ];
        assert_eq!(
            choose_satellite_representative(&targets, 10, &config())
                .unwrap()
                .tsid,
            2
        );
    }

    #[test]
    fn four_k_targets_are_not_grouped() {
        let targets = [
            target(0x000b, 1, BroadcastType::FourK, None, None, None),
            target(0x000b, 2, BroadcastType::FourK, None, None, None),
        ];
        assert_eq!(select_next_target(&targets, 10, &config()).unwrap().tsid, 1);
        assert!(target_needs_scan(&targets[1], 10, &config()));
    }

    #[test]
    fn hard_stale_forces_rescan_even_within_target_refresh() {
        let target = target(
            4,
            1,
            BroadcastType::BS,
            Some(10 + 168 * 3600),
            Some(10),
            Some(0),
        );
        let mut config = config();
        config.target_refresh_secs = 100;
        config.max_stale_secs = 1;
        assert!(target_needs_scan(&target, 10, &config));
    }

    #[test]
    fn startup_wait_uses_delay_and_jitter() {
        assert_eq!(startup_wait_secs(10, 5, 0), 10);
        assert_eq!(startup_wait_secs(10, 5, 5), 15);
    }

    #[test]
    fn startup_wait_is_at_least_one_second() {
        assert!(startup_wait_secs(0, 0, 0) >= 1);
    }

    #[test]
    fn full_evaluation_is_due_only_after_scheduler_interval() {
        let last = Instant::now();
        assert!(!full_evaluation_is_due(
            Some(last),
            60,
            last + Duration::from_secs(59)
        ));
        assert!(full_evaluation_is_due(
            Some(last),
            60,
            last + Duration::from_secs(60)
        ));
    }
}
