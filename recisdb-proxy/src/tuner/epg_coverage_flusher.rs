//! Periodically persists EIT progress collected by live tuners.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use log::warn;

use crate::database::{EpgServiceCoverageUpsert, Result};
use crate::server::listener::DatabaseHandle;
use crate::tuner::epg_progress::EpgProgress;

static PROGRESS_REGISTRY: OnceLock<Mutex<Vec<Weak<EpgProgress>>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Weak<EpgProgress>>> {
    PROGRESS_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 生きている `EpgProgress` を弱参照で保持する。SharedTuner 側は
/// Database を持たないので、書き戻しは Database を持つタスクが行う。
pub fn register_progress(progress: &Arc<EpgProgress>) {
    registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(Arc::downgrade(progress));
}

/// 登録済みのうち生きているものを返し、死んだ弱参照を掃除する。
pub fn live_progresses() -> Vec<Arc<EpgProgress>> {
    let mut entries = registry().lock().unwrap_or_else(|error| error.into_inner());
    let mut live = Vec::with_capacity(entries.len());
    entries.retain(|weak| {
        let Some(progress) = weak.upgrade() else {
            return false;
        };
        live.push(progress);
        true
    });
    live
}

/// Whether a progress handle changed since the last successful persistence.
pub fn progress_advanced(previous: Option<u64>, current: u64) -> bool {
    previous != Some(current)
}

/// Convert only H-EIT completions to persistent service coverage rows.
pub fn coverage_rows_from_progress(
    progress: &EpgProgress,
    now: i64,
) -> Vec<EpgServiceCoverageUpsert> {
    progress
        .snapshot()
        .into_iter()
        .filter(|completion| completion.key.pid == crate::ts_analyzer::pid::EIT)
        .map(|completion| EpgServiceCoverageUpsert {
            network_id: completion.key.original_network_id,
            tsid: completion.key.transport_stream_id,
            service_id: completion.key.service_id,
            pf_complete: completion.pf_complete,
            schedule_basic_complete: completion.schedule_basic_complete,
            schedule_extended_complete: completion.schedule_extended_complete,
            coverage_until: completion.schedule_coverage_until,
            sections_seen: i64::from(completion.sections_seen),
            last_section_at: Some(completion.last_section_at),
            updated_at: now,
        })
        .collect()
}

/// Persist one progress snapshot and refresh mux-level section coverage.
pub async fn persist_progress(
    database: &DatabaseHandle,
    progress: &EpgProgress,
    now: i64,
) -> Result<()> {
    let rows = coverage_rows_from_progress(progress, now);
    let db = database.lock().await;
    db.upsert_epg_service_coverage(&rows)?;
    db.refresh_epg_section_coverage()?;
    Ok(())
}

/// Run the process-wide passive coverage flusher.
pub async fn run(database: DatabaseHandle) {
    let mut last_counters = HashMap::<usize, u64>::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        ticker.tick().await;
        let now = chrono::Utc::now().timestamp();
        for progress in live_progresses() {
            let key = Arc::as_ptr(&progress) as usize;
            let current = progress.progress_counter();
            if !progress_advanced(last_counters.get(&key).copied(), current) {
                continue;
            }
            match persist_progress(&database, &progress, now).await {
                Ok(()) => {
                    last_counters.insert(key, current);
                }
                Err(error) => warn!("[EpgCoverageFlusher] failed to persist coverage: {}", error),
            }
        }
        last_counters.retain(|key, _| {
            live_progresses()
                .iter()
                .any(|progress| Arc::as_ptr(progress) as usize == *key)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::{pid, PsiSection};

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

    fn section(table_id: u8) -> Vec<u8> {
        let mut bytes = vec![
            table_id, 0xB0, 15, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x00,
            table_id,
        ];
        bytes.extend_from_slice(&crc32_mpeg2(&bytes).to_be_bytes());
        bytes
    }

    fn observe(progress: &EpgProgress, pid_value: u16, table_id: u8) {
        let bytes = section(table_id);
        progress.observe(pid_value, &PsiSection::parse(&bytes).unwrap(), 10);
    }

    #[test]
    fn registry_drops_dead_progress_handles() {
        let progress = EpgProgress::new();
        let pointer = Arc::as_ptr(&progress);
        register_progress(&progress);
        drop(progress);
        assert!(live_progresses()
            .iter()
            .all(|item| Arc::as_ptr(item) != pointer));
    }

    #[test]
    fn registry_keeps_live_handles() {
        let progress = EpgProgress::new();
        register_progress(&progress);
        assert!(live_progresses()
            .iter()
            .any(|item| Arc::ptr_eq(item, &progress)));
    }

    #[test]
    fn flusher_skips_progress_that_did_not_advance() {
        assert!(!progress_advanced(Some(4), 4));
        assert!(progress_advanced(Some(4), 5));
    }

    #[test]
    fn snapshot_converts_only_h_eit_services() {
        let progress = EpgProgress::new();
        observe(&progress, pid::EIT, 0x4E);
        observe(&progress, pid::EIT_MOBILE, 0x4E);
        observe(&progress, pid::EIT_PARTIAL_RECEPTION, 0x4E);
        let rows = coverage_rows_from_progress(&progress, 20);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].network_id, 3);
    }
}
