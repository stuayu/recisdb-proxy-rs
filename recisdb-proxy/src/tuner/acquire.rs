//! The single executor for tuner-selection side effects
//! (docs/TUNER_PIPELINE_REDESIGN.md §3/§4 P2b-1).
//!
//! [`snapshot`] builds a [`crate::tuner::policy::TunerSnapshot`] from the
//! live `TunerPool` + `Database`, [`acquire`] feeds it (and a request) into
//! [`crate::tuner::policy::decide`], and carries out whatever [`Decision`]
//! comes back: joining an existing reader, evicting incumbents and starting
//! a new one, or rejecting. This is the only place besides `decide` itself
//! that is allowed to reason about "which tuner/channel do we open" — every
//! call site (`server::channel_resolve::start_tuner_for_service` and
//! `server::session.rs`'s `SetChannelSpace`, whose eight selection helpers
//! this replaced in P2b-2) is a thin translation from its own inputs to
//! [`AcquireRequest`] and back from [`AcquireOutcome`]/[`AcquireError`] to
//! its own error type.
//!
//! # Why `acquire` sometimes calls `decide` more than once
//!
//! `decide` is pure and synchronous, but the [`TunerSnapshot`] it reasons
//! about is only a point-in-time snapshot: another task can create, evict,
//! or finish starting a reader between the moment `snapshot` was taken and
//! the moment this function gets around to acting on the resulting
//! [`Decision`] (`docs/TUNER_PIPELINE_REDESIGN.md`'s policy module doc calls
//! these TOCTOU artifacts "what `decide` does not model" — P1's slot
//! semaphore removes the classic capacity-counting race, but a `Decision`
//! computed from a stale snapshot can still be acted on after the world has
//! moved on). Two concrete ways that shows up here:
//!
//! - `Decision::Create`'s permit can fail to materialize: the driver looked
//!   like it had room (or `evict` was going to make room) in the snapshot,
//!   but [`crate::tuner::pool::TunerPool::acquire_slot`] still returns
//!   `None` when actually asked.
//! - [`crate::tuner::pool::TunerPool::get_or_create`] can hand back an
//!   entry that is already `Starting`/`Running` even though `decide` chose
//!   `Create` — some other task created and started it in the interim.
//!
//! Rather than have `decide` try to predict or simulate multiple rounds of
//! I/O (which would stop it being a pure function reasoning about one
//! instant), `acquire` detects these two situations and reruns the whole
//! `snapshot` → `decide` → act sequence against fresh state, up to
//! a candidate-count-derived number of times (see `max_attempts`) before
//! giving up with
//! [`AcquireError::Conflict`].

use log::{info, warn};

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::server::listener::DatabaseHandle;
use crate::tuner::channel_key::ChannelKeySpec;
use crate::tuner::pool::TunerPoolError;
use crate::tuner::policy::{self, Decision, DriverState, EntryState, RejectReason, TunerSnapshot};
use crate::tuner::shared::{ReaderStartupConfig, StopReason};
use crate::tuner::{CarriedSlotPermit, ChannelKey, SharedTuner, SlotPermit, TunerPool, WarmTunerHandle};

/// Whether libaribb25 should run for a source, given the driver's manual
/// override and the band the channel was scanned as.
///
/// 4K is switched off automatically: a MMT/TLV→TS converter has already
/// descrambled (ACAS) by the time we see the stream, yet the PMT still
/// advertises a CA descriptor with `CA_system_id` 0x0005 — the same id our
/// B-CAS shim reports — so libaribb25 latches the declared ECM PID and waits
/// for keys that never arrive.
///
/// An unscanned channel has no band yet, so it stays on: leaving B25 enabled
/// on a source that did not need it wastes a little work, while disabling it
/// on one that did need it is a black screen.
fn b25_enabled_for(driver_disables_b25: bool, band_type: Option<i64>) -> bool {
    if driver_disables_b25 {
        return false;
    }
    band_type != Some(recisdb_protocol::BandType::FourK as i64)
}

/// A caller's request to have some physical channel tuned in and its
/// `SharedTuner` handed back, expressed candidate-first the same way
/// [`policy::TuneRequest`] is (see that type's doc comment) plus whatever
/// resources the caller already holds that could satisfy it without going
/// back to the pool.
///
/// Stateless callers (the HTTP/Mirakurun path) leave `carried_permit`,
/// `warm` and `own_key` empty; a session switching channels fills all three.
pub(crate) struct AcquireRequest {
    /// Physical candidates, unordered — `decide` re-sorts them.
    pub candidates: Vec<ChannelKey>,
    pub priority: i32,
    pub exclusive: bool,
    pub bondriver_version: u8,
    /// A slot permit the caller already holds, usable only if `decide`
    /// settles on a `Create` whose key lands on the same DLL path (see
    /// [`crate::tuner::pool::CarriedSlotPermit`]). Returned unconsumed via
    /// [`AcquireOutcome::unused_permit`] otherwise.
    pub carried_permit: Option<SlotPermit>,
    /// A pre-opened warm handle the caller already holds, usable the same
    /// way (only if its `path()` matches the winning `Create` key's DLL).
    /// Returned unconsumed via [`AcquireOutcome::unused_warm`] otherwise.
    pub warm: Option<WarmTunerHandle>,
    /// The tuner this caller is currently on, if it is switching away from
    /// one, and whether that switch will actually free the slot (this caller
    /// is its only subscriber).
    ///
    /// Both are needed even though P1b made slot permits the real capacity
    /// gate: enforcement is the permit, but the choice between `Create` and
    /// `Reject` is `decide`'s, and `decide` reasons off the snapshot's
    /// *counts*. Without excluding the caller's own about-to-be-vacated
    /// entry, a switch on a `max_instances = 1` driver is rejected before
    /// `carried_permit` — the very permit that makes the switch possible —
    /// ever gets a chance to be used.
    pub own_key: Option<ChannelKey>,
    pub own_key_will_free_slot: bool,
}

/// What `acquire` did and handed back.
pub(crate) struct AcquireOutcome {
    pub tuner: Arc<SharedTuner>,
    pub key: ChannelKey,
    /// Whether this was a join onto an already-running reader
    /// (`Decision::Reuse`) rather than a fresh `Create` — callers use this
    /// to decide whether to log/attribute a "started" vs. "joined" event.
    pub reused: bool,
    /// The caller's `carried_permit`, if `acquire` never needed it (wrong
    /// path, or the winning decision was `Reuse`). The caller decides
    /// whether to keep holding it (e.g. still-relevant for its own old
    /// tuner) or drop it.
    pub unused_permit: Option<SlotPermit>,
    /// Same as `unused_permit` but for the caller's warm handle.
    pub unused_warm: Option<WarmTunerHandle>,
}

/// Failure modes of [`acquire`]. Callers with their own error type
/// (`server::channel_resolve::ChannelResolveError`) convert via `#[from]` or
/// an explicit `match`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AcquireError {
    /// The request carried no physical candidates at all — mirrors
    /// [`policy::RejectReason::NoCandidates`].
    #[error("no candidate channels supplied")]
    NoCandidates,
    /// `decide` rejected the request as over capacity with no acceptable
    /// eviction — mirrors [`policy::RejectReason::AtCapacity`].
    #[error("all tuner slot(s) on the requested driver are in use (lowest idle priority observed: {lowest_idle_priority:?})")]
    AtCapacity { lowest_idle_priority: Option<i32> },
    /// Every snapshot→decide→act round lost a race
    /// against concurrent pool activity (see this module's doc comment).
    #[error("gave up after {0} attempt(s) racing a concurrent tuner-pool change")]
    Conflict(u32),
    /// `SharedTuner::start_reader` failed (BonDriver open error, SetChannel
    /// error, or the existing-reader-stop precheck timing out).
    #[error("failed to start BonDriver reader: {0}")]
    ReaderStart(#[from] std::io::Error),
    /// A `TunerPool` bookkeeping error (e.g. pool-wide `max_tuners` cap;
    /// distinct from the per-DLL `max_instances` semaphore, which surfaces
    /// as `AtCapacity`/`Conflict` instead).
    #[error("tuner pool error: {0}")]
    Pool(#[from] TunerPoolError),
}

impl From<RejectReason> for AcquireError {
    fn from(reason: RejectReason) -> Self {
        match reason {
            RejectReason::NoCandidates => AcquireError::NoCandidates,
            RejectReason::AtCapacity { lowest_idle_priority } => {
                AcquireError::AtCapacity { lowest_idle_priority }
            }
        }
    }
}

/// Upper bound on `snapshot` → `decide` → act rounds within one [`acquire`].
///
/// Scales with the candidate count rather than being a small constant.
/// Concurrent requesters are perfectly synchronised: they all snapshot the
/// same state, all rank the same driver first, and exactly one wins its
/// permit — so N simultaneous requests need up to N rounds to spread across
/// N drivers. A fixed budget of 3 silently capped a five-receiver group at
/// three concurrent viewers. The `+ 2` covers races that are not
/// self-inflicted (another process taking a slot, an entry vanishing).
fn max_attempts(candidates: usize) -> usize {
    candidates + 2
}

/// Build a [`TunerSnapshot`] of `dll_paths`' driver rows plus every pool
/// entry currently sitting on one of them.
///
/// `dll_paths` should be exactly the driver paths that appear in the
/// request's candidates — not every driver in the system — since that is
/// all `decide` ever looks at (see `policy::TunerSnapshot`'s doc comment).
///
/// # Lock ordering
///
/// All `tuner_pool` calls happen first (each individually `.await`-ed, none
/// while holding any lock of ours), and `database.lock()` is taken exactly
/// once, at the very end, for a block of plain synchronous `rusqlite` calls
/// with no `.await` anywhere inside the guard's scope. This is deliberate:
/// SYSTEM_REVIEW_2026-07.md H3 flags holding a `Database` lock across a
/// `tuner_pool` await as a hazard (a slow pool operation would then also
/// block every other task waiting on the DB mutex), and structuring the
/// function as "gather pool state, *then* take the DB lock for a
/// non-yielding block" makes that impossible by construction rather than by
/// convention.
pub(crate) async fn snapshot(
    pool: &Arc<TunerPool>,
    database: &DatabaseHandle,
    dll_paths: &[String],
) -> TunerSnapshot {
    let dll_paths: Vec<String> = dll_paths.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect();

    // --- Pool state first (async, no locks of ours held). ---------------
    struct RawEntry {
        key: ChannelKey,
        state: crate::tuner::shared::ReaderState,
        subscribers: u32,
        space: u32,
        channel: u32,
    }

    let pending_idle_close = pool.keys_pending_idle_close().await;

    let mut raw_entries = Vec::new();
    for key in pool.keys().await {
        if !dll_paths.iter().any(|p| p == &key.tuner_path) {
            continue;
        }
        let Some(tuner) = pool.get(&key).await else { continue };
        let (space, channel) = match &key.channel {
            ChannelKeySpec::SpaceChannel { space, channel } => (*space, *channel),
            ChannelKeySpec::Simple(c) => (0, *c as u32),
        };
        raw_entries.push(RawEntry {
            key,
            state: tuner.state(),
            subscribers: tuner.subscriber_count(),
            space,
            channel,
        });
    }

    // --- Single DB lock, synchronous-only critical section. --------------
    let (drivers, priorities): (Vec<DriverState>, Vec<i32>) = {
        let db = database.lock().await;

        let exclusive_counts = db.get_exclusive_channel_counts(&dll_paths).unwrap_or_default();
        let drivers = dll_paths
            .iter()
            .map(|path| DriverState {
                dll_path: path.clone(),
                max_instances: db.get_max_instances_for_path(path).unwrap_or(1),
                quality_score: db.get_driver_quality_score_by_path(path).unwrap_or(1.0),
                exclusive_channel_count: exclusive_counts.get(path).copied().unwrap_or(0),
            })
            .collect();

        let priorities = raw_entries
            .iter()
            .map(|e| {
                db.get_channel_priority(&e.key.tuner_path, e.space, e.channel)
                    .unwrap_or(Some(0))
                    .unwrap_or(0)
            })
            .collect();

        (drivers, priorities)
    };

    let entries = raw_entries
        .into_iter()
        .zip(priorities)
        .map(|(e, priority)| EntryState {
            idle_close_pending: pending_idle_close.contains(&e.key),
            key: e.key,
            state: e.state,
            subscribers: e.subscribers,
            priority,
        })
        .collect();

    TunerSnapshot { drivers, entries }
}

/// Pick which of `carried_permit`/`warm` (if either) satisfies a `Create`
/// decision's slot requirement on `dll_path`, consuming whichever one is
/// used (and, when `carried_permit` wins over a same-path `warm`, shutting
/// the now-superseded `warm` down so its own reservation is not stranded —
/// see `server/session.rs::acquire_slot_preferring_warm`, whose priority
/// order this mirrors: a session never needs both an outgoing tuner's own
/// permit *and* a warm handle honored on the very same DLL at once, so
/// whichever is not chosen has nothing left to do but release cleanly).
///
/// Returns `(permit, warm_to_activate)` — `warm_to_activate` is `Some` only
/// when the permit came from that same warm handle, since that is the only
/// case `SharedTuner::start_reader` can actually activate (a mismatched or
/// permit-donating-only warm handle has nothing left for `start_reader` to
/// do with it).
async fn take_permit_for_path(
    dll_path: &str,
    carried_permit: &mut Option<SlotPermit>,
    warm: &mut Option<WarmTunerHandle>,
) -> Option<(SlotPermit, Option<WarmTunerHandle>)> {
    if let Some(permit) = carried_permit.take_if_on_path(dll_path) {
        info!(
            "[acquire] using the caller's own slot permit for {} (same-DLL handoff)",
            dll_path
        );
        if let Some(w) = warm.take() {
            if w.path() == dll_path {
                w.shutdown().await;
            } else {
                *warm = Some(w);
            }
        }
        return Some((permit, None));
    }

    if warm.as_ref().is_some_and(|w| w.path() == dll_path) {
        let mut w = warm.take().expect("just checked Some above");
        if let Some(permit) = w.take_permit() {
            return Some((permit, Some(w)));
        }
        // Warm handle matched the path but already gave up its permit to
        // someone else (should not happen in practice — nothing else ever
        // takes it — but a handle with no permit left is useless here
        // either way, so release it rather than hold a dangling reference).
        w.shutdown().await;
    }

    None
}

/// Stop and remove `key` from the pool as an eviction target (mirrors
/// `server::session_capacity::stop_and_remove_tuner`, which is
/// `pub(super)`-scoped to `server` and so not reachable from here).
///
/// `SharedTuner::stop_reader` already blocks until the reader is confirmed
/// `Stopped` (or `STOP_READER_TIMEOUT_MS` elapses) and releases the slot
/// permit deterministically before returning — see that method's doc
/// comment — so, unlike the pre-P1b version of this helper, there is
/// nothing left for a caller-side wait-for-slot-release poll loop to do.
async fn evict_tuner(pool: &Arc<TunerPool>, key: &ChannelKey) {
    let Some(tuner) = pool.get(key).await else { return };
    // Record *why* before stopping: a session watching this tuner's state
    // wakes on the transition to `Stopped` and reads the reason to report a
    // displacement rather than a bare disconnect
    // (docs/TUNER_PIPELINE_REDESIGN.md §2.1-7 / P4).
    tuner.set_stop_reason(StopReason::Evicted);
    if tuner.has_subscribers() {
        warn!(
            "[acquire] evicting {:?} with {} live subscriber(s) to free a slot",
            key,
            tuner.subscriber_count()
        );
    } else {
        info!("[acquire] evicting idle {:?} to free a slot", key);
    }
    pool.cancel_idle_close(key).await;
    tuner.stop_reader().await;
    pool.remove(key).await;
}

/// Run `request` through `decide` and carry out the resulting [`Decision`]
/// (see the module doc comment for the retry behavior when the snapshot
/// `decide` used turns out to have been stale).
pub(crate) async fn acquire(
    pool: &Arc<TunerPool>,
    database: &DatabaseHandle,
    request: AcquireRequest,
) -> Result<AcquireOutcome, AcquireError> {
    if request.candidates.is_empty() {
        return Err(AcquireError::NoCandidates);
    }

    let dll_paths: Vec<String> = request.candidates.iter().map(|k| k.tuner_path.clone()).collect();

    // Serialize identical requests so a burst of viewers for one channel
    // shares a reader instead of opening one each — see
    // `TunerPool::acquire_channel_lock`. Held for the whole
    // snapshot→decide→act sequence, including the reader start.
    let _channel_guard = pool.acquire_channel_lock(&request.candidates).await;

    let mut carried_permit = request.carried_permit;
    let mut warm = request.warm;

    let attempts = max_attempts(request.candidates.len());
    let mut exhausted: Vec<String> = Vec::new();

    for attempt in 0..attempts {
        let snap = snapshot(pool, database, &dll_paths).await;

        // Drop drivers whose permit we already failed to get in this call.
        // Without this the retry is pointless under contention: every loser
        // re-derives the same ranking from the same fresh state and picks the
        // same driver again, so each round admits exactly one requester.
        // Excluding what we just found full turns the retry into "try the
        // next driver" and bounds progress at one round per candidate.
        let candidates: Vec<ChannelKey> = request
            .candidates
            .iter()
            .filter(|k| !exhausted.iter().any(|p| p == &k.tuner_path))
            .cloned()
            .collect();
        if candidates.is_empty() {
            // Every candidate driver turned out to be full when actually
            // asked. Logged here because it is a decision like any other and
            // otherwise leaves no trace at all (P4).
            info!(
                "[acquire] decision=reject reason=all {} candidate driver(s) full attempt={} priority={} exclusive={}",
                request.candidates.len(),
                attempt + 1,
                request.priority,
                request.exclusive
            );
            return Err(AcquireError::AtCapacity { lowest_idle_priority: None });
        }

        let tune_req = policy::TuneRequest {
            candidates,
            priority: request.priority,
            exclusive: request.exclusive,
            own_key: request.own_key.clone(),
            own_key_will_free_slot: request.own_key_will_free_slot,
        };

        let decision = policy::decide(&snap, &tune_req);

        // One line per decision, with the inputs that produced it. This is
        // the trace that used to be impossible to reconstruct: the old
        // selection helpers logged fragments from eight different places,
        // none of which knew the whole picture (P4).
        match &decision {
            Decision::Reuse { key } => info!(
                "[acquire] decision=reuse key={:?} attempt={} priority={} exclusive={}",
                key, attempt + 1, request.priority, request.exclusive
            ),
            Decision::Create { key, evict } => info!(
                "[acquire] decision=create key={:?} evict={:?} attempt={} priority={} exclusive={} candidates={}",
                key, evict, attempt + 1, request.priority, request.exclusive, request.candidates.len()
            ),
            Decision::Reject { reason } => info!(
                "[acquire] decision=reject reason={:?} attempt={} priority={} exclusive={} candidates={}",
                reason, attempt + 1, request.priority, request.exclusive, request.candidates.len()
            ),
        }

        match decision {
            Decision::Reuse { key } => {
                // `decide` returning `Reuse` is itself the guarantee that no
                // permit may be taken here (docs/TUNER_PIPELINE_REDESIGN.md
                // P1b §6) — a permit is never even asked for on this branch.
                match pool.get(&key).await {
                    Some(tuner) => {
                        return Ok(AcquireOutcome {
                            tuner,
                            key,
                            reused: true,
                            unused_permit: carried_permit,
                            unused_warm: warm,
                        });
                    }
                    None => {
                        // The entry `decide` saw as running vanished (raced
                        // stop/evict elsewhere) between snapshot and now —
                        // stale snapshot, try again.
                        continue;
                    }
                }
            }
            Decision::Create { key, evict } => {
                for victim in &evict {
                    evict_tuner(pool, victim).await;
                }

                let max_instances = snap
                    .drivers
                    .iter()
                    .find(|d| d.dll_path == key.tuner_path)
                    .map(|d| d.max_instances)
                    .unwrap_or(1);

                let (permit, warm_to_use) =
                    match take_permit_for_path(&key.tuner_path, &mut carried_permit, &mut warm).await {
                        Some((permit, warm_to_use)) => (permit, warm_to_use),
                        None => match pool.acquire_slot(&key.tuner_path, max_instances).await {
                            Some(permit) => (permit, None),
                            None => {
                                // The snapshot said this driver would have
                                // (or would gain, via `evict`) a free slot,
                                // but a fresh ask still failed — someone took
                                // it first. Remember that so the next round
                                // moves on instead of choosing it again.
                                exhausted.push(key.tuner_path.clone());
                                continue;
                            }
                        },
                    };

                let tuner = match pool
                    .get_or_create(key.clone(), request.bondriver_version, permit, || async { Ok(()) })
                    .await
                {
                    Ok(tuner) => tuner,
                    Err(e) => {
                        if let Some(w) = warm_to_use {
                            w.shutdown().await;
                        }
                        return Err(AcquireError::Pool(e));
                    }
                };

                if !tuner.needs_reader_start() {
                    // `get_or_create` handed back an entry that is already
                    // `Starting`/`Running` even though `decide` chose
                    // `Create` — another task created (and is starting, or
                    // has finished starting) this exact entry in the
                    // interim. `get_or_create`'s own reuse path already
                    // dropped our surplus permit; only `warm_to_use`, if
                    // any, is still ours to release. Stale snapshot, try
                    // again — see the module doc comment.
                    if let Some(w) = warm_to_use {
                        w.shutdown().await;
                    }
                    continue;
                }

                let Some(start_permit) = tuner.take_slot_permit() else {
                    // Cannot happen in practice — `get_or_create` just
                    // stored one on this freshly `Reserved` entry, and
                    // nothing else has had a chance to run since — but fail
                    // safe rather than starting a reader with no
                    // reservation backing it.
                    if tuner.is_orphanable() {
                        pool.remove(&key).await;
                    }
                    if let Some(w) = warm_to_use {
                        w.shutdown().await;
                    }
                    return Err(AcquireError::Pool(TunerPoolError::OpenFailed(
                        "missing slot permit after get_or_create".to_string(),
                    )));
                };

                let pool_config = pool.config().await;
                let mut startup_config = ReaderStartupConfig::from(&pool_config);
                let (space, channel) = match &key.channel {
                    ChannelKeySpec::SpaceChannel { space, channel } => (*space, *channel),
                    ChannelKeySpec::Simple(c) => (0, *c as u32),
                };

                // Decided here rather than at each call site: `acquire` is the
                // single choke point every tuning path goes through, and it is
                // the only one that already holds the database handle.
                startup_config.b25_enabled = {
                    let db = database.lock().await;
                    b25_enabled_for(
                        db.driver_disables_b25(&key.tuner_path),
                        db.band_type_for_bon_channel(&key.tuner_path, space, channel),
                    )
                };

                if let Err(e) = tuner
                    .start_reader(pool, key.tuner_path.clone(), space, channel, startup_config, start_permit, warm_to_use)
                    .await
                {
                    if tuner.is_orphanable() {
                        pool.remove(&key).await;
                    }
                    return Err(AcquireError::ReaderStart(e));
                }

                return Ok(AcquireOutcome {
                    tuner,
                    key,
                    reused: false,
                    unused_permit: carried_permit,
                    unused_warm: warm,
                });
            }
            Decision::Reject { reason } => return Err(AcquireError::from(reason)),
        }
    }

    Err(AcquireError::Conflict(attempts as u32))
}

// Test-only: neither `SharedTuner` nor `WarmTunerHandle` implement `Debug`
// (they wrap OS/FFI resources with no meaningful textual form), so
// `AcquireOutcome` cannot `#[derive(Debug)]`. This manual impl exists solely
// so test assertions below can embed a useful `{outcome:?}`/`{result:?}` in
// failure messages; production code never formats an `AcquireOutcome`.
#[cfg(test)]
impl std::fmt::Debug for AcquireOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcquireOutcome")
            .field("key", &self.key)
            .field("reused", &self.reused)
            .field("has_unused_permit", &self.unused_permit.is_some())
            .field("has_unused_warm", &self.unused_warm.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, NewBonDriver};
    use crate::tuner::ts_source::FakeTsSource;

    #[test]
    fn b25_is_switched_off_for_four_k_and_by_manual_override() {
        use recisdb_protocol::BandType;

        // Ordinary bands keep descrambling.
        assert!(b25_enabled_for(false, Some(BandType::Terrestrial as i64)));
        assert!(b25_enabled_for(false, Some(BandType::BS as i64)));
        assert!(b25_enabled_for(false, Some(BandType::CS as i64)));

        // 4K arrives already descrambled; running B25 over it makes
        // libaribb25 chase an ECM PID that never delivers.
        assert!(!b25_enabled_for(false, Some(BandType::FourK as i64)));

        // Manual override wins for pre-descrambled sources that are not 4K,
        // where the stream gives nothing away.
        assert!(!b25_enabled_for(true, Some(BandType::BS as i64)));
        assert!(!b25_enabled_for(true, None));

        // Not scanned yet: stay on. Descrambling needlessly is cheap;
        // not descrambling when we should is a black screen.
        assert!(b25_enabled_for(false, None));
    }

    fn fast_startup_config() -> ReaderStartupConfig {
        ReaderStartupConfig {
            set_channel_retry_interval_ms: 5,
            set_channel_retry_timeout_ms: 50,
            signal_poll_interval_ms: 5,
            signal_wait_timeout_ms: 50,
            b25_enabled: true,
        }
    }

    fn db_handle_with_driver(path: &str, max_instances: i32) -> DatabaseHandle {
        let db = Database::open_in_memory().unwrap();
        db.insert_bon_driver(&NewBonDriver::new(path).with_max_instances(max_instances)).unwrap();
        Arc::new(tokio::sync::Mutex::new(db))
    }

    fn empty_request(candidates: Vec<ChannelKey>) -> AcquireRequest {
        AcquireRequest {
            candidates,
            priority: 0,
            exclusive: false,
            bondriver_version: 2,
            carried_permit: None,
            warm: None,
            own_key: None,
            own_key_will_free_slot: false,
        }
    }

    // -----------------------------------------------------------------
    // snapshot()
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn snapshot_reads_driver_row_and_running_entry_priority() {
        let pool = Arc::new(TunerPool::new(10));
        let database = db_handle_with_driver("/dev/test", 3);

        {
            let db = database.lock().await;
            db.insert_bon_driver(&NewBonDriver::new("/dev/other")).unwrap();
        }

        let key = ChannelKey::space_channel("/dev/test", 0, 5);
        let permit = pool.acquire_slot("/dev/test", 3).await.unwrap();
        let tuner = pool.get_or_create(key.clone(), 2, permit, || async { Ok(()) }).await.unwrap();
        let ready_rx = tuner.spawn_fake_reader(FakeTsSource::new(), 0, 5, fast_startup_config()).await;
        ready_rx.await.unwrap().unwrap();
        let _sub = tuner.subscribe();

        let snap = snapshot(&pool, &database, &["/dev/test".to_string()]).await;

        assert_eq!(snap.drivers.len(), 1, "only the requested dll_path's driver row is included");
        assert_eq!(snap.drivers[0].dll_path, "/dev/test");
        assert_eq!(snap.drivers[0].max_instances, 3);

        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].key, key);
        assert!(snap.entries[0].is_running());
        assert!(snap.entries[0].has_subscribers());

        tuner.stop_reader().await;
    }

    #[tokio::test]
    async fn snapshot_excludes_entries_on_paths_outside_the_request() {
        let pool = Arc::new(TunerPool::new(10));
        let database = db_handle_with_driver("/dev/a", 1);
        {
            let db = database.lock().await;
            db.insert_bon_driver(&NewBonDriver::new("/dev/b")).unwrap();
        }

        let key_b = ChannelKey::space_channel("/dev/b", 0, 1);
        let permit_b = pool.acquire_slot("/dev/b", 1).await.unwrap();
        let tuner_b = pool.get_or_create(key_b.clone(), 2, permit_b, || async { Ok(()) }).await.unwrap();
        let _sub = tuner_b.subscribe(); // keep it alive past get_or_create's stale sweep

        // Only "/dev/a" is requested; "/dev/b"'s entry must not appear.
        let snap = snapshot(&pool, &database, &["/dev/a".to_string()]).await;
        assert_eq!(snap.drivers.len(), 1);
        assert_eq!(snap.drivers[0].dll_path, "/dev/a");
        assert!(snap.entries.is_empty());
    }

    // -----------------------------------------------------------------
    // acquire(): Reuse never touches the slot semaphore.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn reuse_joins_an_existing_reader_without_acquiring_a_permit() {
        let pool = Arc::new(TunerPool::new(10));
        let database = db_handle_with_driver("/dev/test", 1);

        let key = ChannelKey::space_channel("/dev/test", 0, 5);
        let permit = pool.acquire_slot("/dev/test", 1).await.unwrap();
        let tuner = pool.get_or_create(key.clone(), 2, permit, || async { Ok(()) }).await.unwrap();
        let ready_rx = tuner.spawn_fake_reader(FakeTsSource::new(), 0, 5, fast_startup_config()).await;
        ready_rx.await.unwrap().unwrap();

        // The driver's only slot is now held by `tuner` — a second
        // `acquire_slot` must fail, proving the assertion below could only
        // have succeeded via the `Reuse` fast path, never by taking (and
        // somehow still getting) a permit.
        assert!(pool.acquire_slot("/dev/test", 1).await.is_none());

        let outcome = acquire(&pool, &database, empty_request(vec![key.clone()])).await.unwrap();
        assert!(outcome.reused);
        assert!(Arc::ptr_eq(&outcome.tuner, &tuner));
        assert_eq!(outcome.key, key);

        tuner.stop_reader().await;
    }

    // -----------------------------------------------------------------
    // acquire(): Create acquires a permit and cleans up fully on failure.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn create_failure_leaves_no_pool_entry_and_releases_its_permit() {
        let pool = Arc::new(TunerPool::new(10));
        let path = "/nonexistent/recisdb-proxy-test-device";
        let database = db_handle_with_driver(path, 1);

        let key = ChannelKey::space_channel(path, 0, 5);
        let result = acquire(&pool, &database, empty_request(vec![key.clone()])).await;

        assert!(
            matches!(&result, Err(AcquireError::ReaderStart(_))),
            "opening a nonexistent device must fail at the reader-start step, not earlier: {result:?}"
        );
        assert_eq!(pool.count().await, 0, "a failed Create must not leave an orphaned entry");
        assert!(
            pool.acquire_slot(path, 1).await.is_some(),
            "the permit taken for the failed attempt must have been released"
        );
    }

    // -----------------------------------------------------------------
    // acquire(): carried_permit is consumed only on a path match.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn carried_permit_on_the_winning_path_is_used_instead_of_a_fresh_acquire() {
        let pool = Arc::new(TunerPool::new(10));
        let path = "/nonexistent/recisdb-proxy-test-device";
        let database = db_handle_with_driver(path, 1);

        // Take the driver's only slot ourselves and hand it in as
        // `carried_permit`. If `acquire` ignored it and asked
        // `TunerPool::acquire_slot` for a fresh one instead, that ask would
        // fail (the semaphore has nothing left) and `acquire` would retry
        // until `Conflict` — so reaching the reader-start step (and its
        // expected failure against a nonexistent device) is exactly the
        // proof that the carried permit, not a fresh one, was used.
        let carried = pool.acquire_slot(path, 1).await.unwrap();
        let key = ChannelKey::space_channel(path, 0, 5);

        let mut request = empty_request(vec![key]);
        request.carried_permit = Some(carried);

        let result = acquire(&pool, &database, request).await;
        assert!(
            matches!(&result, Err(AcquireError::ReaderStart(_))),
            "expected the carried permit to be used, reaching (and failing) the reader-start step: {result:?}"
        );
    }

    #[tokio::test]
    async fn carried_permit_on_a_different_path_is_returned_unused() {
        let pool = Arc::new(TunerPool::new(10));
        let winning_path = "/dev/winning";
        let carried_path = "/dev/carried-elsewhere";
        let database = db_handle_with_driver(winning_path, 1);

        let winning_key = ChannelKey::space_channel(winning_path, 0, 1);
        let permit = pool.acquire_slot(winning_path, 1).await.unwrap();
        let tuner = pool.get_or_create(winning_key.clone(), 2, permit, || async { Ok(()) }).await.unwrap();
        let ready_rx = tuner.spawn_fake_reader(FakeTsSource::new(), 0, 1, fast_startup_config()).await;
        ready_rx.await.unwrap().unwrap();

        // `winning_key` is already running, so `decide` picks `Reuse` — the
        // simplest way to observe "carried_permit came back untouched"
        // without needing a real BonDriver to actually start.
        let carried = pool.acquire_slot(carried_path, 1).await.unwrap();
        let mut request = empty_request(vec![winning_key.clone()]);
        request.carried_permit = Some(carried);

        let outcome = acquire(&pool, &database, request).await.unwrap();
        assert!(outcome.reused);
        let unused = outcome.unused_permit.expect("carried permit for an unrelated path must be returned");
        assert_eq!(unused.dll_path(), carried_path);

        tuner.stop_reader().await;
    }

    // -----------------------------------------------------------------
    // take_permit_for_path(): the priority order in isolation.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn take_permit_for_path_prefers_carried_permit_and_shuts_down_a_same_path_warm() {
        let pool = TunerPool::new(10);
        let path = "/nonexistent/recisdb-proxy-test-device";

        let mut carried = Some(pool.acquire_slot(path, 2).await.unwrap());
        let warm_permit = pool.acquire_slot(path, 2).await.unwrap();
        let mut warm = Some(WarmTunerHandle::spawn(path.to_string(), 1, warm_permit));

        let (permit, warm_to_use) = take_permit_for_path(path, &mut carried, &mut warm).await.unwrap();
        assert_eq!(permit.dll_path(), path);
        assert!(warm_to_use.is_none(), "the carried permit wins; the warm handle is superseded, not activated");
        assert!(carried.is_none(), "the carried permit must be consumed");
        assert!(warm.is_none(), "the superseded warm handle must be shut down and dropped, not left dangling");
    }

    #[tokio::test]
    async fn take_permit_for_path_falls_back_to_a_same_path_warm_handle() {
        let pool = TunerPool::new(10);
        let path = "/nonexistent/recisdb-proxy-test-device";

        let mut carried: Option<SlotPermit> = None;
        let warm_permit = pool.acquire_slot(path, 1).await.unwrap();
        let mut warm = Some(WarmTunerHandle::spawn(path.to_string(), 1, warm_permit));

        let (permit, warm_to_use) = take_permit_for_path(path, &mut carried, &mut warm).await.unwrap();
        assert_eq!(permit.dll_path(), path);
        assert!(warm_to_use.is_some(), "the warm handle's own permit is the one taken, so it is still activatable");

        warm_to_use.unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn take_permit_for_path_ignores_permits_on_other_paths() {
        let pool = TunerPool::new(10);

        let mut carried = Some(pool.acquire_slot("/dev/other", 1).await.unwrap());
        let mut warm = None;

        let result = take_permit_for_path("/dev/target", &mut carried, &mut warm).await;
        assert!(result.is_none());
        assert!(carried.is_some(), "a permit for a different path must not be consumed");
    }

    // -----------------------------------------------------------------
    // acquire(): retry cap on a snapshot that never stops being stale.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn create_gives_up_after_the_retry_cap_when_the_slot_never_becomes_available() {
        let pool = Arc::new(TunerPool::new(10));
        let path = "/dev/perpetually-stale";
        let database = db_handle_with_driver(path, 1);

        // Hold the driver's only slot *outside* the pool entirely (no
        // `SharedTuner`/pool entry backs it) — `decide` will see zero
        // running entries in the snapshot (since nothing is in the pool) and
        // choose `Create` with no eviction needed, but
        // `TunerPool::acquire_slot` fails for real. This is exactly the
        // "snapshot says there's room, reality disagrees" condition the
        // retry loop exists for, and here reality never catches up.
        let _held_forever = pool.acquire_slot(path, 1).await.unwrap();

        let key = ChannelKey::space_channel(path, 0, 1);
        let result = acquire(&pool, &database, empty_request(vec![key])).await;

        // With a single candidate, the first failed permit exhausts the only
        // driver there is, so the honest answer is "at capacity" rather than
        // "gave up racing" — and it is reached without burning the whole
        // retry budget on a driver already known to be full.
        assert!(
            matches!(&result, Err(AcquireError::AtCapacity { .. })),
            "expected AtCapacity once the only candidate is known full, got {result:?}"
        );
        assert_eq!(pool.count().await, 0, "no entry should have been left behind by the abandoned attempts");
    }

    // -----------------------------------------------------------------
    // acquire(): empty candidates reject immediately, no snapshot needed.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn no_candidates_rejects_without_touching_the_pool_or_database() {
        let pool = Arc::new(TunerPool::new(10));
        let database = db_handle_with_driver("/dev/unused", 1);

        let result = acquire(&pool, &database, empty_request(vec![])).await;
        assert!(matches!(result, Err(AcquireError::NoCandidates)));
    }
}
