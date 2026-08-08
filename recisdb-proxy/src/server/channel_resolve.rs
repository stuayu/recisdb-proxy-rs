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
//! running). No fallback-driver search, no exclusive-priority preemption of
//! *other sessions'* actively-subscribed readers — an HTTP preview/stream
//! request never competes for a DLL slot against another *session's live
//! viewer*; it either finds room or reports 503 (`ChannelResolveError::Busy`).
//!
//! # Idle eviction on the same device path (Unix only)
//!
//! One exception to "no eviction": some Unix character-device BonDrivers
//! (px4-drv-family tuners exposing `/dev/px4videoN` etc.) allow only a
//! single concurrent `open()` per device path and return `EALREADY`
//! (errno 114) on a second one — a stricter, purely physical constraint
//! that is independent of the DB's `max_instances` setting. A `SharedTuner`
//! kept warm by `TunerPool`'s keep-alive window (`schedule_idle_close`,
//! default 60s) after its last subscriber left still holds that fd open,
//! which would otherwise make every subsequent request for a *different*
//! channel on the same device fail with 503 until the keep-alive timer
//! happens to expire on its own. `start_tuner_for_service` therefore evicts
//! idle (`is_running() && !has_subscribers()`) readers on the same
//! `dll_path` before opening — via `TunerPool::evict_idle_on_path` — first
//! proactively when the driver is at/over `max_instances` capacity, and
//! then reactively (as a last-resort retry) if `start_bondriver_reader`
//! still fails with `EALREADY`. This only ever touches readers with zero
//! current subscribers; a reader another session/request is actively
//! viewing is never stopped by this path — that distinction is what keeps
//! this different from session.rs's priority-based eviction, which *can*
//! preempt a live low-priority viewer.

use std::sync::Arc;

use log::{error, info, warn};

use crate::database::{ChannelRecord, Database};
use crate::server::session_capacity::count_running_instances_on_driver;
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
    /// Mirakurun-compatible lookup (`web/mirakurun.rs`) by `(nid, sid)`
    /// instead of `channels.id`.
    #[error("service (nid={0}, sid={1}) not found")]
    NotFoundNidSid(u16, u16),
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
    /// All of the driver's tuner slots (`max_instances`, or the physical
    /// single-open-per-device-path limit some Unix character-device drivers
    /// enforce — see `start_tuner_for_service`) are occupied by readers that
    /// are still actively subscribed to, so there was nothing idle left to
    /// evict to make room.
    #[error("service {id}: all {max} tuner slot(s) on driver are in use ({running} running)")]
    Busy { id: i64, running: i32, max: i32 },
}

/// A service resolved to its physical tuning target, before any tuner has
/// been started.
#[derive(Debug, Clone)]
pub struct ResolvedService {
    pub channel: ChannelRecord,
    pub dll_path: String,
    pub channel_key: ChannelKey,
    /// `max_instances` configured for this channel's driver (DB
    /// `bon_drivers.max_instances`, defaulting to 1 when unset — same
    /// fallback semantics as `session_capacity::driver_max_instances`).
    pub max_instances: i32,
}

/// Look up `sid` (a `channels.id` primary key — see `web/api.rs`'s existing
/// `/api/channels/:id`-style endpoints for the same convention) and resolve
/// it to a physical BonDriver path + space/channel. Does not touch the
/// tuner pool; safe to call while holding the `Database` mutex briefly.
pub fn resolve_service(db: &Database, sid: i64) -> Result<ResolvedService, ChannelResolveError> {
    let channel = db
        .get_channel_by_id(sid)?
        .ok_or(ChannelResolveError::NotFound(sid))?;
    resolve_channel_record(db, channel)
}

/// Same as [`resolve_service`] but looks the channel up by broadcast
/// service_id alone — the identity the dashboard UI naturally has at hand
/// (client sessions, EPG programs, channel rows all carry the real SID,
/// not a `channels` primary key).
pub fn resolve_service_by_sid(
    db: &Database,
    sid: u16,
) -> Result<ResolvedService, ChannelResolveError> {
    let channel = db
        .get_channel_by_sid(sid)?
        .ok_or(ChannelResolveError::NotFound(sid as i64))?;
    resolve_channel_record(db, channel)
}

/// Same as [`resolve_service`] but looks the channel up by `(nid, sid)`
/// instead of `channels.id` — the identity the Mirakurun-compatible API
/// (`web/mirakurun.rs`, STREAMING_DESIGN.md §7.1) uses, since Mirakurun's
/// service id (`networkId * 100000 + serviceId`) decodes to `(nid, sid)`,
/// not a `channels` primary key.
pub fn resolve_service_by_nid_sid(
    db: &Database,
    nid: u16,
    sid: u16,
) -> Result<ResolvedService, ChannelResolveError> {
    let channel = db
        .get_channel_by_nid_sid(nid, sid)?
        .ok_or(ChannelResolveError::NotFoundNidSid(nid, sid))?;
    resolve_channel_record(db, channel)
}

/// Shared validation/resolution body for [`resolve_service`] and
/// [`resolve_service_by_nid_sid`], once a candidate `ChannelRecord` has been
/// fetched by whichever key. Errors reference the row's own `channels.id`
/// (not the lookup key), matching what `web/stream.rs`'s existing
/// `channels.id`-keyed error messages already convey.
fn resolve_channel_record(
    db: &Database,
    channel: ChannelRecord,
) -> Result<ResolvedService, ChannelResolveError> {
    let id = channel.id;

    if !channel.is_enabled {
        return Err(ChannelResolveError::Disabled(id));
    }

    let driver = db
        .get_bon_driver(channel.bon_driver_id)?
        .ok_or(ChannelResolveError::NoDriver(id))?;

    let (space, bon_channel) = match (channel.bon_space, channel.bon_channel) {
        (Some(s), Some(c)) => (s, c),
        _ => return Err(ChannelResolveError::NoPhysicalChannel(id)),
    };

    let channel_key = ChannelKey::space_channel(&driver.dll_path, space, bon_channel);
    let max_instances = db.get_max_instances_for_path(&driver.dll_path).unwrap_or(1);

    Ok(ResolvedService {
        channel,
        dll_path: driver.dll_path,
        channel_key,
        max_instances,
    })
}

/// Detect `EALREADY` from `start_bondriver_reader`, which does NOT preserve
/// `raw_os_error`: the blocking open thread stringifies the original
/// `io::Error` into the `ready_tx` channel and the awaiting side rebuilds it
/// as `io::Error::new(kind, String)` (see `tuner/shared.rs`), so only the
/// Display text ("... (os error 114)") survives. Check both the raw code
/// (in case that path ever starts preserving it) and the stringified form.
#[cfg(unix)]
fn is_ealready(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EALREADY)
        || e.to_string().contains(&format!("(os error {})", libc::EALREADY))
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

    // docs/TUNER_PIPELINE_REDESIGN.md P1b §6: check reuse *before* touching
    // the slot semaphore — an exact-key entry that already occupies its slot
    // (another session/request's in-flight or live reader on this exact
    // channel) is joinable via `get_or_create`'s own fast path without a
    // permit at all; asking for one first could wrongly 503 a request that
    // only wants to join an already-open stream on a
    // `max_instances`-constrained driver.
    let already_occupied = tuner_pool.get(&key).await
        .map(|t| !t.is_reclaimable())
        .unwrap_or(false);

    let tuner = if already_occupied {
        tuner_pool.get(&key).await.ok_or_else(|| {
            ChannelResolveError::Pool(TunerPoolError::OpenFailed(
                "raced with a concurrent removal".to_string(),
            ))
        })?
    } else {
        // §7: `acquire_slot`'s success/failure is the `max_instances`
        // enforcement now, in place of the old post-creation
        // `count_running_instances_on_driver`/`evict_idle_on_path` proactive
        // check. If the driver is full, try the same idle-eviction this
        // used to do proactively, then retry once before giving up as `Busy`
        // — evicting only ever touches subscriber-less readers (module doc
        // comment above), so this never preempts another *viewer*.
        let permit = match tuner_pool.acquire_slot(&resolved.dll_path, resolved.max_instances).await {
            Some(p) => p,
            None => {
                let evicted = tuner_pool
                    .evict_idle_on_path(&resolved.dll_path, Some(&key))
                    .await;
                if evicted > 0 {
                    info!(
                        "[HTTP stream] evicted {} idle reader(s) on {} to free capacity for service id={}",
                        evicted, resolved.dll_path, resolved.channel.id
                    );
                }
                match tuner_pool.acquire_slot(&resolved.dll_path, resolved.max_instances).await {
                    Some(p) => p,
                    None => {
                        let running = count_running_instances_on_driver(tuner_pool, &resolved.dll_path, Some(&key)).await;
                        return Err(ChannelResolveError::Busy {
                            id: resolved.channel.id,
                            running,
                            max: resolved.max_instances,
                        });
                    }
                }
            }
        };

        // No-op factory: mirrors every `get_or_create` call site in session.rs
        // (e.g. `try_fallback_drivers`) — the actual BonDriver open happens via
        // `start_bondriver_reader` below, not inside the pool's factory closure.
        tuner_pool
            .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, permit, || async { Ok(()) })
            .await?
    };

    // `needs_reader_start()`, not `!is_running()`: a `Starting` entry has
    // another task's BonDriver open + SetChannel already in flight, and a
    // `Reserved` one is the entry `get_or_create` just handed us.
    if tuner.needs_reader_start() {
        let pool_config = tuner_pool.config().await;
        let startup_config = ReaderStartupConfig::from(&pool_config);

        // Serializes CreateBonDriver+OpenTuner+SetChannel against any other
        // task (session or HTTP) opening the same DLL path concurrently —
        // same lock session.rs's `start_reader_with_warm` takes.
        let _dll_guard = tuner_pool.acquire_dll_init_lock(&resolved.dll_path).await;

        // Re-check after acquiring the lock: another task may have started
        // the reader for this exact key while we awaited the guard.
        if tuner.needs_reader_start() {
            let (space, channel) = match key.channel {
                ChannelKeySpec::SpaceChannel { space, channel } => (space, channel),
                ChannelKeySpec::Simple(c) => (0, c as u32),
            };

            let Some(start_permit) = tuner.take_slot_permit() else {
                error!(
                    "[HTTP stream] {:?} needs a reader start but holds no slot permit (service id={})",
                    key, resolved.channel.id
                );
                if tuner.is_orphanable() {
                    tuner_pool.remove(&key).await;
                }
                return Err(ChannelResolveError::Pool(TunerPoolError::OpenFailed(
                    "missing slot permit".to_string(),
                )));
            };

            info!(
                "[HTTP stream] starting BonDriver reader for {:?} (service id={})",
                key, resolved.channel.id
            );
            let start_result = tuner
                .start_bondriver_reader(resolved.dll_path.clone(), space, channel, startup_config, start_permit)
                .await;

            match start_result {
                Ok(()) => {}
                #[cfg(unix)]
                Err(e) if is_ealready(&e) => {
                    // Last-resort insurance: the capacity check above is
                    // keyed off `max_instances`, which is a *configured*
                    // slot count independent of the physical
                    // single-open-per-device-path constraint some Unix
                    // character-device BonDrivers enforce (see module doc
                    // comment). If the driver open still raced against an
                    // idle reader that hadn't been counted as "at capacity"
                    // (e.g. max_instances > 1 but the device itself only
                    // tolerates one open), evict any idle reader on this
                    // path unconditionally and retry once.
                    //
                    // The failed `start_bondriver_reader` above already
                    // released `tuner`'s slot permit (`SharedTuner::stop_and_release_slot`
                    // runs on every failure path), so a retry needs a fresh one.
                    warn!(
                        "[HTTP stream] BonDriver open for {:?} failed with EALREADY (service id={}); \
                         evicting idle readers on {} and retrying once",
                        key, resolved.channel.id, resolved.dll_path
                    );
                    tuner_pool
                        .evict_idle_on_path(&resolved.dll_path, Some(&key))
                        .await;
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    let Some(retry_permit) = tuner_pool.acquire_slot(&resolved.dll_path, resolved.max_instances).await else {
                        if tuner.is_orphanable() {
                            tuner_pool.remove(&key).await;
                        }
                        return Err(ChannelResolveError::Busy {
                            id: resolved.channel.id,
                            running: resolved.max_instances,
                            max: resolved.max_instances,
                        });
                    };
                    tuner
                        .start_bondriver_reader(
                            resolved.dll_path.clone(),
                            space,
                            channel,
                            startup_config,
                            retry_permit,
                        )
                        .await?;
                }
                Err(e) => return Err(e.into()),
            }
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

    #[test]
    fn resolves_by_nid_sid_same_as_by_id() {
        let (db, ch_id) = setup_db_with_channel(Some(0), Some(13), true);
        let by_id = resolve_service(&db, ch_id).expect("should resolve by id");
        let by_nid_sid = resolve_service_by_nid_sid(&db, 1, 100).expect("should resolve by (nid, sid)");
        assert_eq!(by_id.channel_key, by_nid_sid.channel_key);
        assert_eq!(by_id.channel.id, by_nid_sid.channel.id);
    }

    #[test]
    fn unknown_nid_sid_is_not_found() {
        let db = Database::open_in_memory().unwrap();
        let err = resolve_service_by_nid_sid(&db, 1, 999).unwrap_err();
        assert!(matches!(err, ChannelResolveError::NotFoundNidSid(1, 999)));
    }

    #[test]
    fn disabled_service_is_rejected_via_nid_sid_lookup_too() {
        let (db, _ch_id) = setup_db_with_channel(Some(0), Some(13), false);
        let err = resolve_service_by_nid_sid(&db, 1, 100).unwrap_err();
        assert!(matches!(err, ChannelResolveError::Disabled(_)));
    }

    /// `start_bondriver_reader` loses `raw_os_error` (the open error is
    /// stringified through the ready channel — see `is_ealready`'s doc), so
    /// the retry guard must match the stringified form too.
    #[cfg(unix)]
    #[test]
    fn is_ealready_matches_both_raw_and_stringified_errors() {
        let raw = std::io::Error::from_raw_os_error(libc::EALREADY);
        assert!(is_ealready(&raw));

        let stringified = std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("BonDriver error: {}", raw),
        );
        assert!(stringified.raw_os_error().is_none(), "precondition: stringification drops the raw code");
        assert!(is_ealready(&stringified));

        let other = std::io::Error::new(std::io::ErrorKind::NotFound, "BonDriver not found");
        assert!(!is_ealready(&other));
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
        let permit_a = pool
            .acquire_slot(&resolved.dll_path, resolved.max_instances)
            .await
            .expect("first slot on an empty driver must be available");
        let tuner_a = pool
            .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, permit_a, || async { Ok(()) })
            .await
            .unwrap();
        let _sub = tuner_a.subscribe();

        // Rejoining the *same* channel must not need a free slot of its own:
        // `start_tuner_for_service` checks for a reusable entry before it ever
        // asks for a permit (docs/TUNER_PIPELINE_REDESIGN.md P1b §6), which is
        // what lets a second viewer join a `max_instances = 1` driver instead
        // of getting a 503. Here the driver is already saturated by
        // `permit_a`, so a permit is genuinely unavailable...
        assert!(
            pool.acquire_slot(&resolved.dll_path, resolved.max_instances).await.is_none(),
            "precondition: the driver is saturated by tuner_a's permit"
        );

        // ...yet the pool still hands back the very same `SharedTuner` when a
        // permit is offered for the same key (`get_or_create` releases the
        // surplus permit itself on the reuse path).
        let permit_b = pool
            .acquire_slot(&resolved.dll_path, resolved.max_instances + 1)
            .await
            .expect("widened capacity for the sake of constructing a spare permit");
        let tuner_b = pool
            .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, permit_b, || async { Ok(()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&tuner_a, &tuner_b), "same channel key must share one SharedTuner");
    }
}
