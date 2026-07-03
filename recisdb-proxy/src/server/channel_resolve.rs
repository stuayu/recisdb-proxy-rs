//! Service (channels.id) → physical BonDriver channel resolution, shared by
//! the HTTP streaming endpoints (STREAMING_DESIGN.md §6.3) and, in spirit,
//! mirroring the single-tuner-mode branch of
//! `server::session::Session::handle_set_channel_space`.
//!
//! # Why this isn't a full extraction of `handle_set_channel_space`
//!
//! `handle_set_channel_space` (server/session.rs) is ~450 lines of
//! session-scoped policy: virtual-space-index → actual-space translation,
//! group-mode NID/TSID driver selection across multiple DLLs (picking the
//! least-loaded driver that carries the requested logical channel),
//! exclusive-priority eviction of other running tuners, capacity-based
//! fallback-driver search, and warm-tuner activation — all keyed off mutable
//! `Session` fields (`current_tuner_path`, `group_driver_paths`,
//! `channel_map_cache`, the warm-tuner handle, etc). Extracting that whole
//! machine into a session-independent function would mean either (a)
//! threading a large parameter/state struct that duplicates `Session`'s
//! fields, or (b) reworking `Session` to hold its state in that struct — both
//! high-risk changes to a hard-to-test, already-tuned hot path, and this
//! environment cannot run the resulting binary against real hardware to
//! verify no regression (see task constraints: b25-sys does not link here).
//!
//! Since every `channels` row already stores a concrete
//! `(bon_driver_id, bon_space, bon_channel)` triple (populated by scanning),
//! an HTTP request for a specific service id does not need virtual-space
//! translation or group-mode driver search at all — those exist to let a
//! *session* pick a channel by an abstract index across possibly-multiple
//! drivers. So this module implements only the direct, single-driver
//! resolution: `channels.id` → that channel's own driver/space/channel,
//! then the exact same `TunerPool`/`SharedTuner` calls session.rs's
//! single-tuner-mode branch uses (`get_or_create` with a no-op factory,
//! followed by `SharedTuner::start_bondriver_reader` if not already
//! running). No eviction, no fallback-driver search, no exclusive-priority
//! preemption — an HTTP preview/stream request never competes for a DLL slot
//! against another *session*; it either finds room or reports 503.

use std::sync::Arc;

use log::info;

use crate::database::{ChannelRecord, Database};
use crate::tuner::channel_key::ChannelKeySpec;
use crate::tuner::pool::TunerPoolError;
use crate::tuner::shared::ReaderStartupConfig;
use crate::tuner::{ChannelKey, SharedTuner, TunerPool};

/// `bondriver_version` passed to `TunerPool::get_or_create`. HTTP streaming
/// always addresses channels via space+channel (never the legacy v1
/// single-channel-number BonDriver API), matching what session.rs uses for
/// every BonDriver opened through the modern path.
const HTTP_BONDRIVER_VERSION: u8 = 2;

/// Errors resolving/starting a service's tuner for HTTP streaming.
#[derive(Debug, thiserror::Error)]
pub enum ChannelResolveError {
    #[error("service {0} not found")]
    NotFound(i64),
    #[error("service {0} is disabled")]
    Disabled(i64),
    #[error("service {0} has no assigned BonDriver")]
    NoDriver(i64),
    #[error("service {0} has no bon_space/bon_channel assigned (not fully scanned?)")]
    NoPhysicalChannel(i64),
    #[error("database error: {0}")]
    Database(#[from] crate::database::DatabaseError),
    #[error("tuner pool error: {0}")]
    Pool(#[from] TunerPoolError),
    #[error("failed to start BonDriver reader: {0}")]
    ReaderStart(#[from] std::io::Error),
}

/// A service resolved to its physical tuning target, before any tuner has
/// been started.
#[derive(Debug, Clone)]
pub struct ResolvedService {
    pub channel: ChannelRecord,
    pub dll_path: String,
    pub channel_key: ChannelKey,
}

/// Look up `sid` (a `channels.id` primary key — see `web/api.rs`'s existing
/// `/api/channels/:id`-style endpoints for the same convention) and resolve
/// it to a physical BonDriver path + space/channel. Does not touch the
/// tuner pool; safe to call while holding the `Database` mutex briefly.
pub fn resolve_service(db: &Database, sid: i64) -> Result<ResolvedService, ChannelResolveError> {
    let channel = db
        .get_channel_by_id(sid)?
        .ok_or(ChannelResolveError::NotFound(sid))?;

    if !channel.is_enabled {
        return Err(ChannelResolveError::Disabled(sid));
    }

    let driver = db
        .get_bon_driver(channel.bon_driver_id)?
        .ok_or(ChannelResolveError::NoDriver(sid))?;

    let (space, bon_channel) = match (channel.bon_space, channel.bon_channel) {
        (Some(s), Some(c)) => (s, c),
        _ => return Err(ChannelResolveError::NoPhysicalChannel(sid)),
    };

    let channel_key = ChannelKey::space_channel(&driver.dll_path, space, bon_channel);

    Ok(ResolvedService {
        channel,
        dll_path: driver.dll_path,
        channel_key,
    })
}

/// Get-or-create the `SharedTuner` for `resolved` and ensure its BonDriver
/// reader is running, starting it if needed.
///
/// Does not take a `Database` lock (all inputs are already resolved) so a
/// caller need not hold the DB mutex across the (potentially several-second)
/// BonDriver open — mirrors how `session.rs` only holds the DB lock for
/// quick lookups and never across `start_bondriver_reader`.
///
/// Does not increment `resolved`'s subscriber count — callers must call
/// `tuner.subscribe()` themselves once they decide to hold a live
/// subscription (see `web/stream.rs`).
pub async fn start_tuner_for_service(
    tuner_pool: &Arc<TunerPool>,
    resolved: &ResolvedService,
) -> Result<Arc<SharedTuner>, ChannelResolveError> {
    let key = resolved.channel_key.clone();

    // No-op factory: mirrors every `get_or_create` call site in session.rs
    // (e.g. `try_fallback_drivers`) — the actual BonDriver open happens via
    // `start_bondriver_reader` below, not inside the pool's factory closure.
    let tuner = tuner_pool
        .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, || async { Ok(()) })
        .await?;

    if !tuner.is_running() {
        let pool_config = tuner_pool.config().await;
        let startup_config = ReaderStartupConfig::from(&pool_config);

        // Serializes CreateBonDriver+OpenTuner+SetChannel against any other
        // task (session or HTTP) opening the same DLL path concurrently —
        // same lock session.rs's `start_reader_with_warm` takes.
        let _dll_guard = tuner_pool.acquire_dll_init_lock(&resolved.dll_path).await;

        // Re-check after acquiring the lock: another task may have started
        // the reader for this exact key while we awaited the guard.
        if !tuner.is_running() {
            let (space, channel) = match key.channel {
                ChannelKeySpec::SpaceChannel { space, channel } => (space, channel),
                ChannelKeySpec::Simple(c) => (0, c as u32),
            };
            info!(
                "[HTTP stream] starting BonDriver reader for {:?} (service id={})",
                key, resolved.channel.id
            );
            tuner
                .start_bondriver_reader(resolved.dll_path.clone(), space, channel, startup_config)
                .await?;
        }
    }

    tuner_pool.cancel_idle_close(&key).await;

    Ok(tuner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, NewBonDriver};
    use recisdb_protocol::ChannelInfo;

    fn setup_db_with_channel(space: Option<u32>, channel: Option<u32>, enabled: bool) -> (Database, i64) {
        let db = Database::open_in_memory().unwrap();
        let driver_id = db.insert_bon_driver(&NewBonDriver::new("/dev/test-tuner")).unwrap();

        let info = ChannelInfo {
            nid: 1,
            sid: 100,
            tsid: 200,
            manual_sheet: None,
            raw_name: Some("raw".to_string()),
            channel_name: Some("Test Channel".to_string()),
            physical_ch: None,
            remote_control_key: None,
            service_type: None,
            network_name: None,
            bon_space: space,
            bon_channel: channel,
            band_type: None,
            terrestrial_region: None,
        };
        let ch_id = db.insert_channel(driver_id, &info).unwrap();
        if !enabled {
            db.disable_channel(ch_id).unwrap();
        }
        (db, ch_id)
    }

    #[test]
    fn resolves_enabled_channel_with_physical_assignment() {
        let (db, ch_id) = setup_db_with_channel(Some(0), Some(13), true);
        let resolved = resolve_service(&db, ch_id).expect("should resolve");
        assert_eq!(resolved.dll_path, "/dev/test-tuner");
        assert_eq!(
            resolved.channel_key,
            ChannelKey::space_channel("/dev/test-tuner", 0, 13)
        );
        assert_eq!(resolved.channel.sid, 100);
    }

    #[test]
    fn missing_service_is_not_found() {
        let db = Database::open_in_memory().unwrap();
        let err = resolve_service(&db, 9999).unwrap_err();
        assert!(matches!(err, ChannelResolveError::NotFound(9999)));
    }

    #[test]
    fn disabled_service_is_rejected() {
        let (db, ch_id) = setup_db_with_channel(Some(0), Some(13), false);
        let err = resolve_service(&db, ch_id).unwrap_err();
        assert!(matches!(err, ChannelResolveError::Disabled(_)));
    }

    #[test]
    fn unscanned_channel_without_physical_assignment_is_rejected() {
        let (db, ch_id) = setup_db_with_channel(None, None, true);
        let err = resolve_service(&db, ch_id).unwrap_err();
        assert!(matches!(err, ChannelResolveError::NoPhysicalChannel(_)));
    }

    #[tokio::test]
    async fn start_tuner_for_service_creates_and_reuses_pool_entry() {
        let (db, ch_id) = setup_db_with_channel(Some(0), Some(13), true);
        let resolved = resolve_service(&db, ch_id).unwrap();

        let pool = Arc::new(TunerPool::new(4));
        // We can't actually open a BonDriver DLL in this test environment, so
        // exercise only the pool bookkeeping half: get_or_create with the
        // same no-op factory `start_tuner_for_service` uses, without calling
        // the function itself (which would try `start_bondriver_reader` and
        // fail/timeout waiting on a real DLL).
        let key = resolved.channel_key.clone();
        let tuner_a = pool
            .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, || async { Ok(()) })
            .await
            .unwrap();
        // `TunerPool::get_or_create` treats a not-running, no-subscriber
        // entry as a stale leftover from an idle-close race and evicts it
        // (see pool.rs) — correct for real usage, where `start_tuner_for_service`
        // always calls `start_bondriver_reader` (setting is_running=true)
        // before anyone else can observe the pool entry. Simulate that here
        // with a subscription rather than a real reader.
        let _sub = tuner_a.subscribe();
        let tuner_b = pool
            .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, || async { Ok(()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&tuner_a, &tuner_b), "same channel key must share one SharedTuner");
    }
}
