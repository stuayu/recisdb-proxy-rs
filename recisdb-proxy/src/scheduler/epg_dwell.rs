use std::time::Duration;

use crate::database::{EpgGlobalSettings, EpgScanStatus};
use crate::tuner::MuxCompletion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpgDwellConfig {
    pub min_dwell: Duration,
    pub normal_dwell: Duration,
    pub max_dwell: Duration,
    pub idle_section_timeout: Duration,
    pub target_future_coverage_hours: i64,
}

impl EpgDwellConfig {
    pub fn from_settings(config: &EpgGlobalSettings) -> Self {
        let min = config.min_dwell_secs.max(1) as u64;
        let normal = config.normal_dwell_secs.max(1) as u64;
        let max = config.max_dwell_secs.max(1) as u64;
        let min = Duration::from_secs(min);
        let normal = Duration::from_secs(normal.max(min.as_secs()));
        let max = Duration::from_secs(max.max(normal.as_secs()));
        Self {
            min_dwell: min,
            normal_dwell: normal,
            max_dwell: max,
            idle_section_timeout: Duration::from_secs(
                config.idle_section_timeout_secs.max(1) as u64
            ),
            target_future_coverage_hours: config.target_future_coverage_hours,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwellObservation {
    pub elapsed: Duration,
    pub since_progress: Duration,
    pub any_sections: bool,
    pub reached_target: bool,
    pub any_service_complete: bool,
    pub stream_closed: bool,
    pub cpu_hard_limit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DwellVerdict {
    Continue,
    Stop(EpgScanStatus),
}

pub fn evaluate_dwell(config: &EpgDwellConfig, observation: &DwellObservation) -> DwellVerdict {
    if observation.cpu_hard_limit {
        return DwellVerdict::Stop(EpgScanStatus::CpuAborted);
    }
    if observation.elapsed >= config.max_dwell {
        return DwellVerdict::Stop(if observation.any_sections {
            EpgScanStatus::Partial
        } else {
            EpgScanStatus::NoData
        });
    }
    if observation.elapsed < config.min_dwell {
        return DwellVerdict::Continue;
    }
    if observation.reached_target {
        return DwellVerdict::Stop(EpgScanStatus::Complete);
    }
    if observation.stream_closed {
        return DwellVerdict::Stop(if observation.any_sections {
            EpgScanStatus::Partial
        } else {
            EpgScanStatus::NoData
        });
    }
    if observation.since_progress >= config.idle_section_timeout {
        return DwellVerdict::Stop(if observation.any_sections {
            EpgScanStatus::Partial
        } else {
            EpgScanStatus::NoData
        });
    }
    if observation.elapsed >= config.normal_dwell && observation.any_service_complete {
        return DwellVerdict::Stop(EpgScanStatus::Partial);
    }
    DwellVerdict::Continue
}

pub fn mux_reached_target(
    mux: &MuxCompletion,
    now: i64,
    target_future_coverage_hours: i64,
) -> bool {
    mux.services_total > 0
        && mux.services_without_coverage == 0
        && mux.coverage_until.is_some_and(|until| {
            until >= now.saturating_add(target_future_coverage_hours.saturating_mul(3600))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EpgDwellConfig {
        EpgDwellConfig {
            min_dwell: Duration::from_secs(10),
            normal_dwell: Duration::from_secs(20),
            max_dwell: Duration::from_secs(30),
            idle_section_timeout: Duration::from_secs(5),
            target_future_coverage_hours: 24,
        }
    }

    fn obs(elapsed: u64) -> DwellObservation {
        DwellObservation {
            elapsed: Duration::from_secs(elapsed),
            since_progress: Duration::from_secs(0),
            any_sections: true,
            reached_target: false,
            any_service_complete: false,
            stream_closed: false,
            cpu_hard_limit: false,
        }
    }

    #[test]
    fn cpu_hard_limit_stops_before_min_dwell() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    cpu_hard_limit: true,
                    ..obs(1)
                }
            ),
            DwellVerdict::Stop(EpgScanStatus::CpuAborted)
        );
    }
    #[test]
    fn min_dwell_is_respected_even_when_target_reached() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    reached_target: true,
                    elapsed: Duration::from_secs(1),
                    ..obs(1)
                }
            ),
            DwellVerdict::Continue
        );
    }
    #[test]
    fn reaching_target_after_min_dwell_stops_as_complete() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    reached_target: true,
                    elapsed: Duration::from_secs(10),
                    ..obs(10)
                }
            ),
            DwellVerdict::Stop(EpgScanStatus::Complete)
        );
    }
    #[test]
    fn max_dwell_stops_as_partial_when_sections_were_seen() {
        assert_eq!(
            evaluate_dwell(&config(), &obs(30)),
            DwellVerdict::Stop(EpgScanStatus::Partial)
        );
    }
    #[test]
    fn max_dwell_stops_as_no_data_when_nothing_arrived() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    any_sections: false,
                    ..obs(30)
                }
            ),
            DwellVerdict::Stop(EpgScanStatus::NoData)
        );
    }
    #[test]
    fn idle_section_timeout_stops_as_partial() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    since_progress: Duration::from_secs(5),
                    ..obs(12)
                }
            ),
            DwellVerdict::Stop(EpgScanStatus::Partial)
        );
    }
    #[test]
    fn normal_dwell_stops_only_when_some_service_is_complete() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    elapsed: Duration::from_secs(20),
                    any_service_complete: true,
                    ..obs(20)
                }
            ),
            DwellVerdict::Stop(EpgScanStatus::Partial)
        );
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    elapsed: Duration::from_secs(20),
                    ..obs(20)
                }
            ),
            DwellVerdict::Continue
        );
    }
    #[test]
    fn stream_closed_stops() {
        assert_eq!(
            evaluate_dwell(
                &config(),
                &DwellObservation {
                    stream_closed: true,
                    ..obs(12)
                }
            ),
            DwellVerdict::Stop(EpgScanStatus::Partial)
        );
    }
    #[test]
    fn from_settings_clamps_out_of_order_dwells() {
        let db = crate::database::Database::open_in_memory().unwrap();
        let mut s = db.get_epg_global_settings().unwrap();
        s.min_dwell_secs = 300;
        s.normal_dwell_secs = 10;
        s.max_dwell_secs = 5;
        let c = EpgDwellConfig::from_settings(&s);
        assert!(c.min_dwell <= c.normal_dwell && c.normal_dwell <= c.max_dwell);
    }
    #[test]
    fn mux_reached_target_requires_every_service_to_have_coverage() {
        let m = MuxCompletion {
            services_total: 2,
            services_without_coverage: 1,
            coverage_until: Some(100_000),
            ..Default::default()
        };
        assert!(!mux_reached_target(&m, 0, 1));
    }
    #[test]
    fn mux_reached_target_is_false_for_empty_mux() {
        assert!(!mux_reached_target(&MuxCompletion::default(), 0, 1));
    }
}
