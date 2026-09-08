//! Shared EIT reception progress for the SI collector and EPG scheduler.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ts_analyzer::{EitServiceCompletion, EpgSectionTracker, ObserveOutcome, PsiSection};

/// EIT 取得進捗の共有ハンドル。SI collector タスクが書き、
/// EPG スケジューラが読む。ロックは1セクションごとに極短時間しか握らない。
pub struct EpgProgress {
    tracker: Mutex<EpgSectionTracker>,
    progress_counter: AtomicU64,
}

/// mux (original_network_id, transport_stream_id) 単位の集計。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuxCompletion {
    pub services_total: usize,
    pub services_pf_complete: usize,
    pub services_schedule_basic_complete: usize,
    pub services_schedule_extended_complete: usize,
    /// schedule_coverage_until を持つサービスにおけるその **最小値**。
    /// 該当サービスが無ければ None。
    ///
    /// **注意: これは追跡できたサービスだけの最小値**。schedule を1つも
    /// 受信していないサービス(p/f だけ届いた等)はここに現れないので、
    /// この値だけを見て「mux 全体が揃った」と判断してはいけない。
    /// 判断するときは必ず `services_without_coverage == 0` を併せて見る。
    /// 番組表対象サービスの母数を `channels` から取った厳密な集計は
    /// `Database::get_epg_mux_coverage` 側で行う。
    pub coverage_until: Option<i64>,
    /// `schedule_coverage_until` が None だった追跡中サービスの数。
    pub services_without_coverage: usize,
    pub sections_seen: u64,
    pub last_section_at: Option<i64>,
}

impl EpgProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tracker: Mutex::new(EpgSectionTracker::new()),
            progress_counter: AtomicU64::new(0),
        })
    }

    /// 選局し直したときに呼ぶ。累計カウンタは単調増加のため保持する。
    pub fn reset(&self) {
        let mut tracker = self
            .tracker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *tracker = EpgSectionTracker::new();
    }

    pub fn observe(&self, pid: u16, section: &PsiSection<'_>, now: i64) -> ObserveOutcome {
        let outcome = self
            .tracker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(pid, section, now);
        if matches!(
            outcome,
            ObserveOutcome::Recorded | ObserveOutcome::VersionChanged
        ) {
            self.progress_counter.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }

    /// 指定 mux の集計。番組表の完成判定には H-EIT (PID 0x0012) だけを使う。
    pub fn mux_completion(
        &self,
        original_network_id: u16,
        transport_stream_id: u16,
    ) -> MuxCompletion {
        let tracker = self
            .tracker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let completions: Vec<_> = tracker
            .snapshot()
            .into_iter()
            .filter(|completion| {
                completion.key.pid == crate::ts_analyzer::pid::EIT
                    && completion.key.original_network_id == original_network_id
                    && completion.key.transport_stream_id == transport_stream_id
            })
            .collect();

        let mut result = MuxCompletion {
            services_total: completions.len(),
            ..MuxCompletion::default()
        };
        for completion in completions {
            result.services_pf_complete += usize::from(completion.pf_complete);
            result.services_schedule_basic_complete +=
                usize::from(completion.schedule_basic_complete);
            result.services_schedule_extended_complete +=
                usize::from(completion.schedule_extended_complete);
            match completion.schedule_coverage_until {
                Some(coverage) => {
                    result.coverage_until = Some(
                        result
                            .coverage_until
                            .map_or(coverage, |current| current.min(coverage)),
                    );
                }
                None => result.services_without_coverage += 1,
            }
            result.sections_seen += u64::from(completion.sections_seen);
            result.last_section_at = Some(
                result
                    .last_section_at
                    .map_or(completion.last_section_at, |current| {
                        current.max(completion.last_section_at)
                    }),
            );
        }
        result
    }

    pub fn snapshot(&self) -> Vec<EitServiceCompletion> {
        self.tracker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot()
    }

    pub fn total_sections_seen(&self) -> u64 {
        self.tracker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .total_sections_seen()
    }

    pub fn progress_counter(&self) -> u64 {
        self.progress_counter.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::{pid, table_id};
    fn crc32_mpeg2(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= (byte as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    fn section_bytes(
        table_id: u8,
        service_id: u16,
        tsid: u16,
        onid: u16,
        version: u8,
        number: u8,
        last: u8,
        segment_last: u8,
        current: bool,
    ) -> Vec<u8> {
        let mut bytes = vec![
            table_id,
            0xB0,
            15,
            (service_id >> 8) as u8,
            service_id as u8,
            0xC0 | ((version & 0x1F) << 1) | current as u8,
            number,
            last,
            (tsid >> 8) as u8,
            tsid as u8,
            (onid >> 8) as u8,
            onid as u8,
            segment_last,
            table_id,
        ];
        let crc = crc32_mpeg2(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn observe(progress: &EpgProgress, bytes: &[u8], pid_value: u16, now: i64) {
        let section = PsiSection::parse(bytes).expect("valid test section");
        progress.observe(pid_value, &section, now);
    }

    #[test]
    fn mux_completion_counts_services_of_that_mux_only() {
        let progress = EpgProgress::new();
        for service_id in [1, 2] {
            let bytes = section_bytes(0x4E, service_id, 10, 20, 1, 0, 0, 0, true);
            observe(&progress, &bytes, pid::EIT, 1);
        }
        let bytes = section_bytes(0x4E, 3, 11, 20, 1, 0, 0, 0, true);
        observe(&progress, &bytes, pid::EIT, 1);
        assert_eq!(progress.mux_completion(20, 10).services_total, 2);
    }

    #[test]
    fn mux_coverage_is_the_minimum_across_services() {
        let progress = EpgProgress::new();
        for (service_id, sections) in [(1, vec![0, 1, 8, 9]), (2, vec![0, 1])] {
            for number in sections {
                let bytes = section_bytes(
                    0x50,
                    service_id,
                    10,
                    20,
                    1,
                    number,
                    9,
                    if number < 8 { 1 } else { 9 },
                    true,
                );
                observe(&progress, &bytes, pid::EIT, 1_725_168_000);
            }
        }
        let completion = progress.mux_completion(20, 10);
        let now: i64 = 1_725_168_000;
        let midnight = ((now + 9 * 3600).div_euclid(86400) * 86400) - 9 * 3600;
        assert_eq!(completion.coverage_until, Some(midnight + 3 * 3600));
    }

    #[test]
    fn mux_completion_ignores_mobile_and_partial_reception_pids() {
        let progress = EpgProgress::new();
        for pid_value in [pid::EIT_MOBILE, pid::EIT_PARTIAL_RECEPTION] {
            let bytes = section_bytes(0x4E, pid_value, 10, 20, 1, 0, 0, 0, true);
            observe(&progress, &bytes, pid_value, 1);
        }
        assert_eq!(progress.mux_completion(20, 10).services_total, 0);
    }

    #[test]
    fn reset_clears_tracked_state_but_not_progress_counter() {
        let progress = EpgProgress::new();
        let bytes = section_bytes(table_id::EIT_PF_ACTUAL, 1, 10, 20, 1, 0, 0, 0, true);
        observe(&progress, &bytes, pid::EIT, 1);
        let counter = progress.progress_counter();
        progress.reset();
        assert!(progress.snapshot().is_empty());
        assert_eq!(progress.total_sections_seen(), 0);
        assert_eq!(progress.progress_counter(), counter);
    }

    #[test]
    fn progress_counter_advances_only_on_new_sections() {
        let progress = EpgProgress::new();
        let bytes = section_bytes(0x4E, 1, 10, 20, 1, 0, 0, 0, true);
        observe(&progress, &bytes, pid::EIT, 1);
        observe(&progress, &bytes, pid::EIT, 1);
        assert_eq!(progress.progress_counter(), 1);
    }
}
