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
//! drivers. So this module still does not do virtual-space-index
//! translation or group-mode abstract-index resolution. It does, however,
//! account for one thing group-mode has always had to: the same `(nid, sid)`
//! service can be scanned into more than one BonDriver's `channels` rows
//! (the same multiplex reachable from several tuners). When a caller
//! resolves by `(nid, sid)` or by broadcast `sid` alone, every enabled row
//! for that service becomes a physical candidate
//! ([`CandidateTarget`]), and all of them are handed to
//! `tuner::acquire::acquire` together — so a request for a service that one
//! candidate driver can't currently serve (full/busy) but another candidate
//! driver is *already streaming* still succeeds for free via `decide`'s
//! `Reuse` rule, instead of failing 503 just because the first candidate
//! happened to be busy. Driver selection and joining an already-running
//! reader are entirely `decide`'s job (see module doc comment further down);
//! this module only assembles the candidate list. No fallback-driver
//! *search* beyond that (no capacity-based ranking, no exclusive-priority
//! preemption of *other sessions'* actively-subscribed readers) — an HTTP
//! preview/stream request never competes for a DLL slot against another
//! *session's live viewer*; it either finds room on some candidate or
//! reports 503 (`ChannelResolveError::Busy`).
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
//! then reactively (as a last-resort retry) if `start_reader`
//! still fails with `EALREADY`. This only ever touches readers with zero
//! current subscribers; a reader another session/request is actively
//! viewing is never stopped by this path — that distinction is what keeps
//! this different from session.rs's priority-based eviction, which *can*
//! preempt a live low-priority viewer.

use std::sync::Arc;

use log::{info, warn};

use crate::database::{ChannelRecord, Database};
use crate::server::listener::DatabaseHandle;
use crate::server::session_capacity::count_running_instances_on_driver;
use crate::tuner::acquire::{self, AcquireError, AcquireRequest};
use crate::tuner::pool::TunerPoolError;
use crate::tuner::timing;
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
    #[error("service {id}: all {drivers} candidate driver(s) are full ({running}/{max} on {path})")]
    Busy { id: i64, running: i32, max: i32, drivers: usize, path: String },
}

/// One physical selection target for a service: a driver + space/channel a
/// `channels` row resolved to, plus that driver's configured capacity.
#[derive(Debug, Clone)]
pub struct CandidateTarget {
    pub dll_path: String,
    pub channel_key: ChannelKey,
    /// `max_instances` configured for this candidate's driver (DB
    /// `bon_drivers.max_instances`, defaulting to 1 when unset — same
    /// fallback semantics as `session_capacity::driver_max_instances`).
    pub max_instances: i32,
}

/// A service resolved to its physical tuning target(s), before any tuner has
/// been started.
#[derive(Debug, Clone)]
pub struct ResolvedService {
    /// The row `candidates[0]` was built from — used for metadata
    /// (sid/nid/channel_name/priority/bon_space/bon_channel) that does not
    /// vary meaningfully across candidates. Not necessarily the row that
    /// ends up serving the request; see [`start_tuner_for_service`].
    pub channel: ChannelRecord,
    /// Every physical target that can serve this (nid, sid), in
    /// priority DESC, id ASC order (matching `channel`). Never empty:
    /// construction fails with a `ChannelResolveError` instead of producing
    /// an empty list, so [`Self::primary`] can index `[0]` unconditionally.
    pub candidates: Vec<CandidateTarget>,
}

impl ResolvedService {
    /// The first (highest-priority) candidate. `candidates` is guaranteed
    /// non-empty by construction, so this never panics.
    pub fn primary(&self) -> &CandidateTarget {
        &self.candidates[0]
    }

    pub fn dll_path(&self) -> &str {
        &self.primary().dll_path
    }

    pub fn channel_key(&self) -> &ChannelKey {
        &self.primary().channel_key
    }

    pub fn max_instances(&self) -> i32 {
        self.primary().max_instances
    }
}

/// Look up `sid` (a `channels.id` primary key — see `web/api.rs`'s existing
/// `/api/channels/:id`-style endpoints for the same convention) and resolve
/// it to a physical BonDriver path + space/channel. Does not touch the
/// tuner pool; safe to call while holding the `Database` mutex briefly.
///
/// Unlike [`resolve_service_by_sid`]/[`resolve_service_by_nid_sid`], this
/// always resolves to exactly one candidate: a `channels.id` names one
/// specific row on one specific driver by construction (that is the whole
/// point of addressing by primary key rather than by service identity), so
/// there is no other row to widen the search to.
pub fn resolve_service(db: &Database, sid: i64) -> Result<ResolvedService, ChannelResolveError> {
    let channel = db
        .get_channel_by_id(sid)?
        .ok_or(ChannelResolveError::NotFound(sid))?;
    resolve_single_channel_record(db, channel)
}

/// Same as [`resolve_service`] but looks the channel up by broadcast
/// service_id alone — the identity the dashboard UI naturally has at hand
/// (client sessions, EPG programs, channel rows all carry the real SID,
/// not a `channels` primary key). Unlike `resolve_service`, this resolves to
/// every enabled row sharing that `sid` (see module doc comment): the same
/// service can be scanned into more than one BonDriver.
pub fn resolve_service_by_sid(
    db: &Database,
    sid: u16,
) -> Result<ResolvedService, ChannelResolveError> {
    let channels = db.get_channels_by_sid(sid)?;
    resolve_candidate_rows(db, channels, ChannelResolveError::NotFound(sid as i64))
}

/// Same as [`resolve_service`] but looks the channel up by `(nid, sid)`
/// instead of `channels.id` — the identity the Mirakurun-compatible API
/// (`web/mirakurun.rs`, STREAMING_DESIGN.md §7.1) uses, since Mirakurun's
/// service id (`networkId * 100000 + serviceId`) decodes to `(nid, sid)`,
/// not a `channels` primary key. Like [`resolve_service_by_sid`], resolves
/// to every enabled row sharing that `(nid, sid)`.
pub fn resolve_service_by_nid_sid(
    db: &Database,
    nid: u16,
    sid: u16,
) -> Result<ResolvedService, ChannelResolveError> {
    let channels = db.get_channels_by_nid_sid(nid, sid)?;
    resolve_candidate_rows(db, channels, ChannelResolveError::NotFoundNidSid(nid, sid))
}

/// Single-row resolution body for [`resolve_service`]. Errors reference the
/// row's own `channels.id` (not the lookup key), matching what
/// `web/stream.rs`'s existing `channels.id`-keyed error messages already
/// convey.
fn resolve_single_channel_record(
    db: &Database,
    channel: ChannelRecord,
) -> Result<ResolvedService, ChannelResolveError> {
    let id = channel.id;

    if !channel.is_enabled {
        return Err(ChannelResolveError::Disabled(id));
    }

    let target = channel_record_to_candidate(db, &channel)?;

    Ok(ResolvedService { channel, candidates: vec![target] })
}

/// Resolve `driver_id`/`bon_space`/`bon_channel` on an already-enabled row
/// into a [`CandidateTarget`], or the appropriate error keyed on that row's
/// `channels.id`.
fn channel_record_to_candidate(
    db: &Database,
    channel: &ChannelRecord,
) -> Result<CandidateTarget, ChannelResolveError> {
    let id = channel.id;

    let driver = db
        .get_bon_driver(channel.bon_driver_id)?
        .ok_or(ChannelResolveError::NoDriver(id))?;

    let (space, bon_channel) = match (channel.bon_space, channel.bon_channel) {
        (Some(s), Some(c)) => (s, c),
        _ => return Err(ChannelResolveError::NoPhysicalChannel(id)),
    };

    let channel_key = ChannelKey::space_channel(&driver.dll_path, space, bon_channel);
    let max_instances = db.get_max_instances_for_path(&driver.dll_path).unwrap_or(1);

    Ok(CandidateTarget { dll_path: driver.dll_path, channel_key, max_instances })
}

/// Shared multi-row resolution body for [`resolve_service_by_sid`] and
/// [`resolve_service_by_nid_sid`]: turn every row returned by the DB's
/// `ORDER BY is_enabled DESC, priority DESC, id ASC` query into a candidate
/// list. `not_found` is the error to return when `rows` is empty.
fn resolve_candidate_rows(
    db: &Database,
    rows: Vec<ChannelRecord>,
    not_found: ChannelResolveError,
) -> Result<ResolvedService, ChannelResolveError> {
    let Some(first) = rows.first().cloned() else {
        return Err(not_found);
    };

    let enabled: Vec<ChannelRecord> = rows.into_iter().filter(|c| c.is_enabled).collect();
    let Some(representative) = enabled.first().cloned() else {
        // No enabled row at all: report Disabled against the highest-ranked
        // (first) row, same as the old single-row lookup did.
        return Err(ChannelResolveError::Disabled(first.id));
    };

    let mut candidates: Vec<CandidateTarget> = Vec::new();
    let mut channel: Option<ChannelRecord> = None;
    for row in &enabled {
        let Ok(target) = channel_record_to_candidate(db, row) else {
            continue;
        };
        // De-dup by physical target: distinct `channels` rows (different
        // sid/tsid bookkeeping) can still resolve to the same
        // (dll_path, space, channel) — acquire() only needs one entry per
        // actual physical tuning target. Keep first-seen (highest priority).
        if candidates.iter().any(|c: &CandidateTarget| c.channel_key == target.channel_key) {
            continue;
        }
        if channel.is_none() {
            channel = Some(row.clone());
        }
        candidates.push(target);
    }

    if candidates.is_empty() {
        // Every enabled row failed to resolve to a physical target (no
        // driver, or no bon_space/bon_channel yet) — surface the same error
        // the single-row path would have, keyed on the top-ranked enabled
        // row so the message points at something the operator can look up.
        return Err(match channel_record_to_candidate(db, &representative) {
            Err(e) => e,
            Ok(_) => unreachable!("channel_record_to_candidate succeeded but candidates is empty"),
        });
    }

    Ok(ResolvedService { channel: channel.unwrap_or(representative), candidates })
}

/// Detect `EALREADY` from `start_reader`, which does NOT preserve
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
/// docs/TUNER_PIPELINE_REDESIGN.md P2b-1: the actual selection/eviction/
/// start sequence now goes entirely through `tuner::acquire::acquire` (the
/// single executor also used by every other selection path once P2b-2/-3
/// land) — this function is left with exactly the two things that are
/// genuinely specific to HTTP/Mirakurun streaming and are *not* part of
/// `decide`'s policy (see module doc comment above):
///
/// 1. The Unix single-open-per-device-path idle eviction
///    (`evict_idle_on_path`), both proactively (here, before ever calling
///    `acquire`) and reactively (on `EALREADY`, after `acquire` fails).
/// 2. Translating [`crate::tuner::acquire::AcquireError`] into this module's
///    own [`ChannelResolveError`] (in particular, `Busy`'s `running`/`max`
///    diagnostic fields, which `AcquireError` has no reason to know about).
///
/// The request built for `acquire` carries every candidate in `resolved`
/// (module doc comment: the same `(nid, sid)` can be scanned into more than
/// one BonDriver's rows), `exclusive: false` (an HTTP viewer never preempts
/// another session's live reader — same guarantee as before this refactor,
/// now enforced structurally by `decide`'s exclusive-eviction branch simply
/// never being reachable with `exclusive: false`), and
/// `priority: resolved.channel.priority` (this channel's own DB-configured
/// priority — the same value a session's `SetChannelSpace` uses when the
/// client did not explicitly override it; HTTP has no equivalent of a
/// client-supplied priority to plumb through). No `carried_permit`/`warm`:
/// an HTTP request never already holds either. Which candidate `decide`
/// actually settles on is reported back via `AcquireOutcome::key` /
/// `finish_outcome`'s return value (`SharedTuner::key`) — callers must use
/// that, not `resolved.channel_key()`, since they can differ.
pub async fn start_tuner_for_service(
    tuner_pool: &Arc<TunerPool>,
    database: &DatabaseHandle,
    resolved: &ResolvedService,
) -> Result<Arc<SharedTuner>, ChannelResolveError> {
    // Proactive idle eviction: only when EVERY candidate driver already
    // looks at/over its own `max_instances` is there no point trying
    // `acquire` without first freeing something up — if even one candidate
    // has room, `decide` will simply pick it (or join an existing reader on
    // it) without needing any eviction at all, so evicting here would only
    // risk killing an idle reader on a driver this request may not even end
    // up using. This is the Unix single-open-per-device-path workaround
    // (module doc comment) and is independent of `decide`'s own
    // (priority-gated) capacity-limit eviction below `acquire` — a request
    // for a channel that is already running still joins it for free via
    // `acquire`'s `Reuse` path regardless of what happens here.
    let mut all_full = true;
    for c in &resolved.candidates {
        let running = count_running_instances_on_driver(tuner_pool, &c.dll_path, Some(&c.channel_key)).await;
        if running < c.max_instances {
            all_full = false;
            break;
        }
    }
    if all_full {
        evict_idle_on_all_candidates(tuner_pool, resolved, "service").await;
    }

    match try_acquire(tuner_pool, database, resolved).await {
        Ok(outcome) => {
            tuner_pool.cancel_idle_close(&outcome.key).await;
            Ok(finish_outcome(outcome, resolved))
        }
        #[cfg(unix)]
        Err(AcquireError::ReaderStart(e)) if is_ealready(&e) => {
            // Last-resort insurance: the capacity check above is keyed off
            // `max_instances`, a *configured* slot count independent of the
            // physical single-open-per-device-path constraint some Unix
            // character-device BonDrivers enforce (module doc comment). If
            // the driver open still raced against an idle reader that
            // hadn't been counted as "at capacity" (e.g. `max_instances` > 1
            // but the device itself only tolerates one open), evict any
            // idle reader on every candidate path unconditionally and retry
            // once.
            warn!(
                "[HTTP stream] BonDriver open failed with EALREADY for service id={} \
                 ({} candidate(s)); evicting idle readers and retrying once",
                resolved.channel.id, resolved.candidates.len()
            );
            evict_idle_on_all_candidates(tuner_pool, resolved, "service").await;
            tokio::time::sleep(std::time::Duration::from_millis(timing::EALREADY_RETRY_SLEEP_MS)).await;
            match try_acquire(tuner_pool, database, resolved).await {
                Ok(outcome) => {
                    tuner_pool.cancel_idle_close(&outcome.key).await;
                    Ok(finish_outcome(outcome, resolved))
                }
                Err(e) => Err(map_acquire_error(tuner_pool, resolved, e).await),
            }
        }
        Err(e) => Err(map_acquire_error(tuner_pool, resolved, e).await),
    }
}

/// Evict idle (subscriber-less) readers on every distinct `dll_path` among
/// `resolved`'s candidates. `label` is only used for the log line.
async fn evict_idle_on_all_candidates(tuner_pool: &Arc<TunerPool>, resolved: &ResolvedService, label: &str) {
    let mut seen_paths: Vec<&str> = Vec::new();
    for c in &resolved.candidates {
        if seen_paths.contains(&c.dll_path.as_str()) {
            continue;
        }
        seen_paths.push(&c.dll_path);
        let evicted = tuner_pool.evict_idle_on_path(&c.dll_path, Some(&c.channel_key)).await;
        if evicted > 0 {
            info!(
                "[HTTP stream] evicted {} idle reader(s) on {} to free capacity for {} id={}",
                evicted, c.dll_path, label, resolved.channel.id
            );
        }
    }
}

/// Log and unpack a successful [`AcquireOutcome`].
///
/// HTTP requests never pass `acquire` a `carried_permit`/`warm` handle (see
/// [`start_tuner_for_service`]'s doc comment), so both are always `None`
/// here — asserted rather than silently dropped via `outcome.tuner` alone,
/// so a future change that starts threading either of them through this
/// module is forced to notice and decide what to do with the "unused" case
/// instead of it leaking unnoticed.
fn finish_outcome(outcome: acquire::AcquireOutcome, resolved: &ResolvedService) -> Arc<SharedTuner> {
    if outcome.reused {
        info!("[HTTP stream] joined existing reader for {:?} (service id={}, {} candidate(s))", outcome.key, resolved.channel.id, resolved.candidates.len());
    } else {
        info!("[HTTP stream] started BonDriver reader for {:?} (service id={}, {} candidate(s))", outcome.key, resolved.channel.id, resolved.candidates.len());
    }
    debug_assert!(outcome.unused_permit.is_none(), "HTTP requests never carry a permit into acquire()");
    debug_assert!(outcome.unused_warm.is_none(), "HTTP requests never carry a warm handle into acquire()");
    outcome.tuner
}

/// Build and run the `acquire` request for `resolved` (see
/// [`start_tuner_for_service`]'s doc comment for the field choices).
async fn try_acquire(
    tuner_pool: &Arc<TunerPool>,
    database: &DatabaseHandle,
    resolved: &ResolvedService,
) -> Result<acquire::AcquireOutcome, AcquireError> {
    acquire::acquire(
        tuner_pool,
        database,
        AcquireRequest {
            candidates: resolved.candidates.iter().map(|c| c.channel_key.clone()).collect(),
            priority: resolved.channel.priority,
            exclusive: false,
            client_host: "http".to_string(),
            // HTTP requests are stateless: there is no "the tuner this
            // caller was already on" to exclude from capacity, and nothing
            // to hand a permit down from.
            own_key: None,
            own_key_will_free_slot: false,
            bondriver_version: HTTP_BONDRIVER_VERSION,
            carried_permit: None,
            warm: None,
        },
    )
    .await
}

/// Translate an [`AcquireError`] into this module's own error type, adding
/// the `running`/`max` diagnostic context `Busy` carries (which `acquire`
/// itself has no reason to compute, since it is generic over every
/// selection path, not just this HTTP-specific error shape). `running`/`max`/
/// `path` are reported against `resolved.primary()` — the highest-priority
/// candidate — since `acquire` failing means *none* of the candidates had
/// room, and the primary is the most representative single one to name.
async fn map_acquire_error(
    tuner_pool: &Arc<TunerPool>,
    resolved: &ResolvedService,
    e: AcquireError,
) -> ChannelResolveError {
    match e {
        AcquireError::ReaderStart(io_err) => ChannelResolveError::ReaderStart(io_err),
        AcquireError::Pool(pool_err) => ChannelResolveError::Pool(pool_err),
        // Never actually reached from this module — `try_acquire` always
        // supplies at least one candidate (`ResolvedService::candidates` is
        // never empty by construction) — but handled rather than
        // `unreachable!()` since `AcquireError` is a shared, generic type
        // whose variants this match must stay exhaustive over.
        AcquireError::NoCandidates => {
            ChannelResolveError::Pool(TunerPoolError::OpenFailed("no candidates supplied".to_string()))
        }
        AcquireError::AtCapacity { .. } | AcquireError::Warming { .. } | AcquireError::Conflict(_) => {
            let primary = resolved.primary();
            let running = count_running_instances_on_driver(tuner_pool, &primary.dll_path, Some(&primary.channel_key)).await;
            ChannelResolveError::Busy {
                id: resolved.channel.id,
                running,
                max: primary.max_instances,
                drivers: resolved.candidates.len(),
                path: primary.dll_path.clone(),
            }
        }
        // Same treatment as `ReaderStart`: the DLL open is what actually
        // failed here too, just refused pre-emptively instead of attempted
        // again. `ConnectionRefused` communicates "the resource is not
        // currently reachable" to HTTP/Mirakurun callers the same way an
        // outright open failure would.
        AcquireError::OpenCooldown { tuner_path, consecutive, retry_in } => {
            ChannelResolveError::ReaderStart(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!(
                    "driver {tuner_path} is in an open-failure cooldown for another {}ms after {consecutive} consecutive failure(s)",
                    retry_in.as_millis()
                ),
            ))
        }
    }
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
        assert_eq!(resolved.dll_path(), "/dev/test-tuner");
        assert_eq!(
            *resolved.channel_key(),
            ChannelKey::space_channel("/dev/test-tuner", 0, 13)
        );
        assert_eq!(resolved.channel.sid, 100);
        assert_eq!(resolved.candidates.len(), 1, "resolve_service (by channels.id) never widens to multiple candidates");
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
        assert_eq!(by_id.channel_key(), by_nid_sid.channel_key());
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

    /// `start_reader` loses `raw_os_error` (the open error is
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
        // the function itself (which would try `start_reader` and
        // fail/timeout waiting on a real DLL).
        let key = resolved.channel_key().clone();
        let permit_a = pool
            .acquire_slot(resolved.dll_path(), resolved.max_instances())
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
            pool.acquire_slot(resolved.dll_path(), resolved.max_instances()).await.is_none(),
            "precondition: the driver is saturated by tuner_a's permit"
        );

        // ...yet the pool still hands back the very same `SharedTuner` when a
        // permit is offered for the same key (`get_or_create` releases the
        // surplus permit itself on the reuse path).
        let permit_b = pool
            .acquire_slot(resolved.dll_path(), resolved.max_instances() + 1)
            .await
            .expect("widened capacity for the sake of constructing a spare permit");
        let tuner_b = pool
            .get_or_create(key.clone(), HTTP_BONDRIVER_VERSION, permit_b, || async { Ok(()) })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&tuner_a, &tuner_b), "same channel key must share one SharedTuner");
    }

    // ------------------------------------------------------------------
    // Multi-candidate resolution (resolve_service_by_sid /
    // resolve_service_by_nid_sid): the same (nid, sid) scanned into more
    // than one BonDriver's rows.
    // ------------------------------------------------------------------

    /// Insert a channel row for `(nid, sid)` on a fresh driver named
    /// `driver_name`, at the given priority/enabled state. Returns the new
    /// channel id.
    fn insert_channel_on_driver(
        db: &Database,
        driver_name: &str,
        nid: u16,
        sid: u16,
        space: Option<u32>,
        channel: Option<u32>,
        priority: i32,
        enabled: bool,
    ) -> i64 {
        let driver_id = db.insert_bon_driver(&NewBonDriver::new(driver_name)).unwrap();
        let info = ChannelInfo {
            nid,
            sid,
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
        db.update_channel_fields(ch_id, None, Some(priority), Some(enabled)).unwrap();
        ch_id
    }

    #[test]
    fn resolve_by_nid_sid_returns_all_enabled_candidates_priority_ordered() {
        let db = Database::open_in_memory().unwrap();
        // Lower priority inserted first, higher priority second — the
        // returned candidate order must still be priority DESC, id ASC.
        insert_channel_on_driver(&db, "driver-low.dll", 1, 100, Some(0), Some(13), 0, true);
        insert_channel_on_driver(&db, "driver-high.dll", 1, 100, Some(0), Some(14), 10, true);

        let resolved = resolve_service_by_nid_sid(&db, 1, 100).expect("should resolve");
        assert_eq!(resolved.candidates.len(), 2);
        assert_eq!(resolved.candidates[0].dll_path, "driver-high.dll");
        assert_eq!(resolved.candidates[1].dll_path, "driver-low.dll");
        // The representative `channel` row corresponds to the first candidate.
        assert_eq!(resolved.dll_path(), "driver-high.dll");
    }

    #[test]
    fn resolve_by_sid_excludes_disabled_rows_from_candidates() {
        let db = Database::open_in_memory().unwrap();
        insert_channel_on_driver(&db, "driver-a.dll", 1, 100, Some(0), Some(13), 10, true);
        insert_channel_on_driver(&db, "driver-b.dll", 1, 100, Some(0), Some(14), 5, false);

        let resolved = resolve_service_by_sid(&db, 100).expect("should resolve via the enabled row");
        assert_eq!(resolved.candidates.len(), 1);
        assert_eq!(resolved.candidates[0].dll_path, "driver-a.dll");
    }

    #[test]
    fn resolve_by_nid_sid_dedups_identical_physical_targets() {
        let db = Database::open_in_memory().unwrap();
        // Two distinct `channels` rows (different sid bookkeeping isn't
        // possible here since sid is the lookup key, but two rows can still
        // land on the very same driver/space/channel e.g. via a stray
        // duplicate scan) resolving to the same physical target must
        // collapse to one candidate.
        let driver_id = db.insert_bon_driver(&NewBonDriver::new("driver.dll")).unwrap();
        let mut info = ChannelInfo::new(1, 100, 200);
        info.bon_space = Some(0);
        info.bon_channel = Some(13);
        db.insert_channel(driver_id, &info).unwrap();
        // A second row, different tsid, same (nid, sid) and same physical
        // target — the DB schema doesn't forbid this (tsid isn't part of the
        // (nid,sid) lookup key), and it happens after certain rescans.
        let mut info2 = ChannelInfo::new(1, 100, 201);
        info2.bon_space = Some(0);
        info2.bon_channel = Some(13);
        db.insert_channel(driver_id, &info2).unwrap();

        let resolved = resolve_service_by_nid_sid(&db, 1, 100).expect("should resolve");
        assert_eq!(resolved.candidates.len(), 1, "identical physical targets must collapse to one candidate");
    }

    #[test]
    fn resolve_by_nid_sid_all_disabled_reports_disabled() {
        let db = Database::open_in_memory().unwrap();
        insert_channel_on_driver(&db, "driver-a.dll", 1, 100, Some(0), Some(13), 10, false);
        insert_channel_on_driver(&db, "driver-b.dll", 1, 100, Some(0), Some(14), 5, false);

        let err = resolve_service_by_nid_sid(&db, 1, 100).unwrap_err();
        assert!(matches!(err, ChannelResolveError::Disabled(_)));
    }

    #[test]
    fn resolve_by_nid_sid_enabled_without_physical_channel_reports_no_physical_channel() {
        let db = Database::open_in_memory().unwrap();
        insert_channel_on_driver(&db, "driver-a.dll", 1, 100, None, None, 0, true);

        let err = resolve_service_by_nid_sid(&db, 1, 100).unwrap_err();
        assert!(matches!(err, ChannelResolveError::NoPhysicalChannel(_)));
    }
}
