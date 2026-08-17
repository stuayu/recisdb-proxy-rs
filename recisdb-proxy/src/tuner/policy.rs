//! Pure tuner-selection decision logic.
//!
//! This module is P0 of `docs/TUNER_PIPELINE_REDESIGN.md` §4: extract the
//! "which tuner/channel do we open for this request" decision out of the
//! async `session.rs` control flow into a pure function
//! ([`decide`]) that takes one immutable snapshot of pool/DB state
//! (`TunerSnapshot`) plus the request (`TuneRequest`) and returns a
//! `Decision` — no I/O, no async, no logging.
//!
//! **This module is not wired in yet.** `session.rs`'s existing helpers
//! (`handle_set_channel_space` and friends) keep making the real decisions
//! for now; replacing their control flow with calls into [`decide`] is P2.
//! Wiring it up requires a second effect layer (`tuner/acquire.rs`, P2) that
//! turns a `Decision` into pool mutations, reader starts, and evictions.
//!
//! The individual pure helpers that used to live in
//! `server::session_driver_selection` and `server::session_capacity` moved
//! here unchanged (see re-exports at the bottom of those modules) since
//! `decide` is built directly on top of them:
//! - candidate ordering: [`sort_candidate_drivers`]
//! - "already running" preference: [`select_running_driver`]
//! - "first with spare capacity" preference: [`select_driver_with_capacity`]
//! - capacity predicates: [`has_capacity`], [`should_stop_reader_for_capacity`]
//! - eviction target choice: [`choose_eviction_target`]
//! - old-reader stop-vs-idle-close: [`should_sync_stop_old_reader`]
//!
//! # Faithfulness, not correctness
//!
//! [`decide`] reproduces the *current* behavior of
//! `session.rs::handle_set_channel_space` (and its v1/group-select
//! siblings) as closely as a single pure function can, **including known
//! inconsistencies** documented in `docs/TUNER_PIPELINE_REDESIGN.md` §2.1-8:
//!
//! - the exclusive-access eviction path may evict a tuner that still has
//!   active subscribers; the capacity-limit eviction path only ever evicts
//!   idle (no-subscriber) tuners.
//! - the capacity-limit priority check uses `>=` (a same-priority request
//!   still evicts the incumbent), not `>`.
//! - if a driver is at capacity and no *idle* tuner exists to evict, the
//!   capacity-limit path does not fall back to another candidate and does
//!   not reject — it proceeds to create the new tuner anyway, pushing the
//!   driver over `max_instances`.
//!
//! These are intentionally preserved and pinned down by tests below. P2
//! unifies the eviction policy (see the redesign doc §4 P2).
//!
//! # What `decide` does *not* model
//!
//! Two follow-up re-checks in the current code happen *after* an
//! async operation has already run (a fresh reader start, or the ~10s ready
//! wait), so they are inherently "decide again against a fresher snapshot"
//! rather than part of a single decision:
//! - `try_start_set_channel_space_new_tuner`'s post-`get_or_create` conflict
//!   check (another session may have started a reader while this one
//!   awaited pool creation).
//! - `finalize_set_channel_space_new_tuner`'s post-start exclusive
//!   re-check/interloper eviction loop.
//!
//! Both are TOCTOU artifacts of counting-based capacity (§2.1-2) that P1's
//! slot-reservation semaphore is meant to remove; modeling them inside
//! `decide` would mean simulating multiple rounds of I/O inside a "pure"
//! function. P2's executor is expected to call `decide` again (against a
//! fresh snapshot) if it detects it lost such a race, rather than `decide`
//! trying to predict it.

use std::collections::HashMap;

use crate::tuner::channel_key::ChannelKeySpec;
use crate::tuner::ChannelKey;

// ---------------------------------------------------------------------
// Moved from `server::session_driver_selection` (P0).
// ---------------------------------------------------------------------

/// `(dll_path, bon_space, bon_channel)` — a driver paired with the physical
/// channel numbers *that driver* uses for a logical (NID, TSID) target.
pub type DriverCandidate = (String, u32, u32);

/// Sort candidates by rarity-aware load balancing: fewer exclusive channels
/// first, then fewer running instances, then higher quality score.
pub fn sort_candidate_drivers(
    candidate_drivers: &mut [DriverCandidate],
    exclusive_map: &HashMap<String, i64>,
    instances_map: &HashMap<String, i32>,
    score_map: &HashMap<String, f64>,
) {
    candidate_drivers.sort_by(|a, b| {
        let excl_a = exclusive_map.get(&a.0).copied().unwrap_or(0);
        let excl_b = exclusive_map.get(&b.0).copied().unwrap_or(0);
        excl_a
            .cmp(&excl_b)
            .then_with(|| {
                let load_a = instances_map.get(&a.0).copied().unwrap_or(0);
                let load_b = instances_map.get(&b.0).copied().unwrap_or(0);
                load_a.cmp(&load_b)
            })
            .then_with(|| {
                let score_a = score_map.get(&a.0).copied().unwrap_or(1.0);
                let score_b = score_map.get(&b.0).copied().unwrap_or(1.0);
                score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
            })
    });
}

/// Prefer a driver already running the requested physical channel.
pub fn select_running_driver(
    candidate_drivers: &[DriverCandidate],
    running_channels: &[(String, ChannelKeySpec)],
) -> Option<DriverCandidate> {
    for (driver_path, driver_space, driver_bon_channel) in candidate_drivers.iter() {
        let wanted = ChannelKeySpec::SpaceChannel {
            space: *driver_space,
            channel: *driver_bon_channel,
        };
        if running_channels
            .iter()
            .any(|(path, key)| path == driver_path && *key == wanted)
        {
            return Some((driver_path.clone(), *driver_space, *driver_bon_channel));
        }
    }
    None
}

/// Otherwise choose the first driver with free capacity.
pub fn select_driver_with_capacity(
    candidate_drivers: &[DriverCandidate],
    instances_map: &HashMap<String, i32>,
    max_instances_map: &HashMap<String, i32>,
) -> Option<DriverCandidate> {
    for (driver_path, driver_space, driver_bon_channel) in candidate_drivers.iter() {
        let driver_instances = instances_map.get(driver_path).copied().unwrap_or(0);
        let max_instances = max_instances_map.get(driver_path).copied().unwrap_or(1);
        if driver_instances < max_instances {
            return Some((driver_path.clone(), *driver_space, *driver_bon_channel));
        }
    }
    None
}

// ---------------------------------------------------------------------
// Moved from `server::session_capacity` (P0).
// ---------------------------------------------------------------------

/// `(key, priority, has_subscribers)` — an eviction candidate.
pub type EvictionCandidate = (ChannelKey, i32, bool);

pub fn has_capacity(running_instances: i32, max_instances: i32) -> bool {
    running_instances < max_instances
}

pub fn should_stop_reader_for_capacity(running_instances: i32, max_instances: i32) -> bool {
    running_instances >= max_instances
}

/// Prefer idle tuners first, then the lowest effective priority.
pub fn choose_eviction_target(candidates: &[EvictionCandidate]) -> Option<EvictionCandidate> {
    let mut best_idle: Option<EvictionCandidate> = None;
    let mut best_any: Option<EvictionCandidate> = None;

    for (key, priority, has_subscribers) in candidates.iter() {
        if !has_subscribers {
            if best_idle.as_ref().map_or(true, |(_, p, _)| priority < p) {
                best_idle = Some((key.clone(), *priority, *has_subscribers));
            }
        }
        if best_any.as_ref().map_or(true, |(_, p, _)| priority < p) {
            best_any = Some((key.clone(), *priority, *has_subscribers));
        }
    }

    best_idle.or(best_any)
}

/// Whether the old reader must be stopped synchronously (vs. scheduled for
/// idle-close) when a session switches away from it.
///
/// Two call sites disagree on whether a same-DLL switch alone is reason
/// enough to force a synchronous stop, so the caller decides via
/// `force_stop_same_dll`:
///   - `SetChannelSpace` / `SetChannel` (v1): a same-DLL switch on a
///     multi-instance DLL is allowed to leave the old reader running so it
///     can idle-close (warm reuse) — only actual capacity pressure forces a
///     synchronous stop.
///   - `SelectLogicalChannel`: group members are assumed to hard-exclusive
///     the underlying hardware, so a same-DLL switch always stops the old
///     reader synchronously, regardless of spare capacity.
/// Capacity pressure (`running >= max`) always forces a synchronous stop
/// either way — that part is not caller-dependent.
pub fn should_sync_stop_old_reader(
    same_dll: bool,
    force_stop_same_dll: bool,
    running: i32,
    max: i32,
) -> bool {
    (force_stop_same_dll && same_dll) || should_stop_reader_for_capacity(running, max)
}

// ---------------------------------------------------------------------
// New in P0: the snapshot/request/decision types and `decide`.
// ---------------------------------------------------------------------

/// Per-driver state needed by the decision. Mirrors `bon_drivers` columns
/// plus the derived quality score and exclusive-channel count that
/// `session.rs` currently fetches from the DB per call.
#[derive(Debug, Clone, PartialEq)]
pub struct DriverState {
    pub dll_path: String,
    pub max_instances: i32,
    /// `db.get_driver_quality_score_by_path`; defaults to `1.0` when unset,
    /// matching `sort_candidate_drivers`'s existing fallback.
    pub quality_score: f64,
    /// `db.get_exclusive_channel_counts`; defaults to `0` when unset.
    pub exclusive_channel_count: i64,
}

/// Per-pool-entry state needed by the decision. Mirrors the subset of
/// `SharedTuner`/`ChannelKey` that the decision reads.
///
/// `state` is P1's [`crate::tuner::shared::ReaderState`] (the redesign doc's
/// P0 sketch called this field `reserved`/left it out pending P1 — see the
/// historical note this comment used to carry, now resolved). `decide()`
/// itself only ever asks "is this entry `Running`" (see
/// `TunerSnapshot`'s helper methods below), so introducing the full 5-state
/// enum here is a mechanical, behavior-preserving generalization of the old
/// `running: bool` field — every existing `decide()` test's expected output
/// is unchanged (`Running` is still the only state any of them construct via
/// `entry(..., running: bool, ...)`'s `true` case).
#[derive(Debug, Clone, PartialEq)]
pub struct EntryState {
    pub key: ChannelKey,
    pub state: crate::tuner::shared::ReaderState,
    pub subscribers: u32,
    /// Effective `db.get_channel_priority` for this entry's physical
    /// channel (already resolved to a plain `i32`, matching every call
    /// site's `.unwrap_or(Some(0)).unwrap_or(0)` pattern).
    pub priority: i32,
    /// Whether a keep-alive (idle-close) timer is counting down on this
    /// entry. Only meaningful when nothing is subscribed, and it is what
    /// distinguishes a keep-alive leftover (takeable) from an entry whose
    /// caller has tuned it but not yet subscribed (not takeable).
    pub idle_close_pending: bool,
}

impl EntryState {
    pub fn has_subscribers(&self) -> bool {
        self.subscribers > 0
    }

    /// Equivalent to the old `running: bool` field / `SharedTuner::is_running()`.
    ///
    /// Only true once TS is actually flowing. Use it to decide whether an
    /// entry is *usable*, never whether its driver has room — for that see
    /// [`Self::occupies_slot`].
    pub fn is_running(&self) -> bool {
        self.state == crate::tuner::shared::ReaderState::Running
    }

    /// Whether this entry is holding a slot on its driver — mirrors
    /// [`crate::tuner::SharedTuner::occupies_slot`].
    ///
    /// Capacity questions must use this, not [`Self::is_running`]. An entry
    /// that is `Reserved`/`Starting` has a driver slot reserved and a
    /// BonDriver open in flight, but is not `Running` yet; counting only
    /// `Running` made a driver look free for the whole (multi-second) open
    /// window. Concurrent group selections then all picked the *same*
    /// driver, and every one but the winner failed — including on `acquire`'s
    /// retries, since each fresh snapshot repeated the same mistake.
    pub fn occupies_slot(&self) -> bool {
        use crate::tuner::shared::ReaderState;
        matches!(
            self.state,
            ReaderState::Reserved
                | ReaderState::Starting
                | ReaderState::Running
                | ReaderState::Stopping
        )
    }
}

/// A single immutable snapshot of the tuner pool + relevant DB state, taken
/// once per decision so every branch of the decision reasons about the same
/// point in time (see redesign doc §2.1-2/§2.1-5 on today's repeated,
/// drifting re-queries).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TunerSnapshot {
    pub drivers: Vec<DriverState>,
    pub entries: Vec<EntryState>,
}

impl TunerSnapshot {
    fn running_count_excluding(&self, dll_path: &str, exclude: Option<&ChannelKey>) -> i32 {
        self.entries
            .iter()
            .filter(|e| e.occupies_slot() && e.key.tuner_path == dll_path && exclude != Some(&e.key))
            .count() as i32
    }

    fn running_channel_specs(&self) -> Vec<(String, ChannelKeySpec)> {
        self.entries
            .iter()
            // `occupies_slot`, not `is_running`: a driver already opening
            // this exact channel is the right one to join rather than open a
            // second instance for.
            .filter(|e| e.occupies_slot())
            .map(|e| (e.key.tuner_path.clone(), e.key.channel.clone()))
            .collect()
    }

}

/// A single logical channel request, expressed as a caller-priority-ordered
/// (irrelevant — `decide` re-sorts) list of concrete physical targets: one
/// entry in single-tuner mode, or one per group driver carrying the same
/// (NID, TSID) in group mode (mirrors
/// `select_group_driver_for_channel`'s `candidate_drivers` /
/// `try_fallback_drivers`'s `fallback_candidates`, unified into one list
/// since `decide` handles both the initial pick and the fallback walk).
#[derive(Debug, Clone)]
pub struct TuneRequest {
    pub candidates: Vec<ChannelKey>,
    /// Effective client/DB priority for the requested channel (already
    /// resolved: client priority if `> 0`, else `i32::MAX` if `exclusive`,
    /// else the DB default — mirrors `handle_set_channel_space`'s
    /// `channel_priority` computation, which stays outside `decide` since
    /// it needs a DB read).
    pub priority: i32,
    pub exclusive: bool,
    /// This session's currently-held key, if any.
    pub own_key: Option<ChannelKey>,
    /// Whether switching away from `own_key` will free its slot on that
    /// driver *before* this decision's new tuner would need it (mirrors
    /// `old_tuner_will_free_slot`: true when this session is the sole
    /// subscriber, so its unsubscribe/switch drops the driver's running
    /// count by one).
    pub own_key_will_free_slot: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    /// The request carried no physical candidates at all (e.g. an empty
    /// group match) — mirrors the "channel NID/TSID not found in any group
    /// driver" early-return in `handle_set_channel_space`.
    NoCandidates,
    /// Every candidate driver is at/over capacity and this request's
    /// priority did not clear the bar to evict an incumbent (or an
    /// incumbent could not be found to evict). `lowest_idle_priority` is
    /// the lowest idle-tuner priority observed on the *primary* candidate
    /// driver, when there was one, for diagnostics/logging by the caller —
    /// mirrors the value `handle_set_channel_space_capacity_limit` logs.
    AtCapacity { lowest_idle_priority: Option<i32> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Reuse the already-running tuner at `key` outright; do not touch
    /// capacity/priority/exclusive logic at all (mirrors
    /// `try_reuse_existing_set_channel_space_tuner`, which fires
    /// unconditionally on a same-channel running-tuner match, and doubles
    /// as the "requested channel already running" skip inside
    /// `handle_set_channel_space_exclusive_access`).
    Reuse { key: ChannelKey },
    /// Create (or `get_or_create`-attach to) a tuner at `key`, first
    /// evicting every key listed in `evict` (0 or 1 in every current
    /// caller, since today's eviction is always "one victim per attempt" —
    /// kept as a `Vec` since P2's slot-reservation model may need to evict
    /// more than one to make room in the same request).
    Create { key: ChannelKey, evict: Vec<ChannelKey> },
    Reject { reason: RejectReason },
}

fn candidate_tuple(key: &ChannelKey) -> DriverCandidate {
    let (space, channel) = match &key.channel {
        ChannelKeySpec::SpaceChannel { space, channel } => (*space, *channel),
        ChannelKeySpec::Simple(c) => (0, *c as u32),
    };
    (key.tuner_path.clone(), space, channel)
}

fn build_exclusive_map(snapshot: &TunerSnapshot) -> HashMap<String, i64> {
    snapshot
        .drivers
        .iter()
        .map(|d| (d.dll_path.clone(), d.exclusive_channel_count))
        .collect()
}

fn build_score_map(snapshot: &TunerSnapshot) -> HashMap<String, f64> {
    snapshot
        .drivers
        .iter()
        .map(|d| (d.dll_path.clone(), d.quality_score))
        .collect()
}

fn build_max_instances_map(snapshot: &TunerSnapshot) -> HashMap<String, i32> {
    snapshot
        .drivers
        .iter()
        .map(|d| (d.dll_path.clone(), d.max_instances))
        .collect()
}

fn build_instances_map(snapshot: &TunerSnapshot, exclude: Option<&ChannelKey>) -> HashMap<String, i32> {
    let mut map: HashMap<String, i32> = HashMap::new();
    for e in &snapshot.entries {
        if !e.occupies_slot() || exclude == Some(&e.key) {
            continue;
        }
        *map.entry(e.key.tuner_path.clone()).or_insert(0) += 1;
    }
    map
}

/// Decide what to do about `req` given `snapshot`. Pure: no I/O, no
/// locking, no logging. See the module doc comment for the overall mapping
/// from `session.rs`'s current helpers to the branches below.
pub fn decide(snapshot: &TunerSnapshot, req: &TuneRequest) -> Decision {
    if req.candidates.is_empty() {
        return Decision::Reject {
            reason: RejectReason::NoCandidates,
        };
    }

    let exclude_own = if req.own_key_will_free_slot {
        req.own_key.as_ref()
    } else {
        None
    };

    let mut candidate_tuples: Vec<DriverCandidate> = req.candidates.iter().map(candidate_tuple).collect();
    let exclusive_map = build_exclusive_map(snapshot);
    let score_map = build_score_map(snapshot);
    let max_instances_map = build_max_instances_map(snapshot);
    let instances_map = build_instances_map(snapshot, exclude_own);

    // Rule 1: sort candidates (fewer exclusive channels, then fewer running
    // instances, then higher quality score).
    sort_candidate_drivers(&mut candidate_tuples, &exclusive_map, &instances_map, &score_map);

    // Rule 2: a candidate already running this exact physical channel wins
    // outright, short-circuiting capacity/priority/exclusive logic —
    // this *is* the "same channel already running" reuse case, so there is
    // no separate step for it later.
    let running_channels = snapshot.running_channel_specs();
    if let Some((path, space, channel)) = select_running_driver(&candidate_tuples, &running_channels) {
        return Decision::Reuse {
            key: ChannelKey::space_channel(path, space, channel),
        };
    }

    // Rule 3: otherwise the first candidate with spare capacity.
    // Rule 4: otherwise the sorted head (every candidate is full; the
    // capacity/priority/exclusive logic below decides what happens next).
    let (path, space, channel) = select_driver_with_capacity(&candidate_tuples, &instances_map, &max_instances_map)
        .unwrap_or_else(|| candidate_tuples[0].clone());
    let primary = ChannelKey::space_channel(path.clone(), space, channel);

    let running = snapshot.running_count_excluding(&path, exclude_own);
    let max = max_instances_map.get(&path).copied().unwrap_or(1);

    if has_capacity(running, max) {
        return Decision::Create { key: primary, evict: vec![] };
    }

    decide_at_capacity(
        snapshot,
        req,
        &candidate_tuples,
        &path,
        primary,
        &max_instances_map,
        exclude_own,
    )
}

/// May `req` take the slot currently held by this incumbent?
///
/// Two different questions, depending on whether anyone is actually watching
/// the incumbent:
///
/// - **idle** (no subscribers) — always yes. It is only alive because of the
///   keep-alive window.
/// - **live viewer** — only if `req` strictly outranks it, or is `exclusive`.
///   Strictly greater, not `>=`: a request that merely ties does not get to
///   displace a working stream (P2b-3; before that `>=` let an
///   equal-priority request bump whoever got there first for no gain). An
///   `exclusive` request is the exception — it is asking for the hardware
///   outright, and wins ties.
fn may_evict(req: &TuneRequest, victim_priority: i32, victim_is_keep_alive: bool) -> bool {
    if victim_is_keep_alive {
        // Nobody is watching it and its keep-alive timer is already running.
        // That window exists to make zapping back cheap — an optimisation,
        // not a claim on the hardware — so it yields even on a tie. On a
        // fully-booked group, letting it win would reject a real viewer.
        //
        // Note this is *not* "zero subscribers": an entry that was just
        // tuned and whose caller has not subscribed yet also has none, and
        // taking that one away would break the request that created it.
        return true;
    }
    // Displacing a live viewer is the case the priority rule is for.
    req.priority > victim_priority || req.exclusive
}

/// Pick a victim on `dll_path`, preferring an idle (subscriber-less) reader
/// and, within each group, the lowest configured priority.
///
/// Returns the idle choice first; the second element is the lowest-priority
/// choice across *all* running readers on the driver, subscribed ones
/// included. Callers try them in that order, so a live viewer is only ever
/// displaced when no idle reader could be taken instead.
fn eviction_options(snapshot: &TunerSnapshot, dll_path: &str) -> (Option<EvictionCandidate>, Option<EvictionCandidate>) {
    let all: Vec<EvictionCandidate> = snapshot
        .entries
        .iter()
        .filter(|e| e.key.tuner_path == dll_path && e.is_running())
        .map(|e| (e.key.clone(), e.priority, e.has_subscribers()))
        .collect();
    let idle: Vec<EvictionCandidate> = all.iter().filter(|(_, _, subs)| !subs).cloned().collect();

    (choose_eviction_target(&idle), choose_eviction_target(&all))
}

/// Is this key a keep-alive leftover — running, unsubscribed, and already
/// counting down to idle-close?
fn is_keep_alive_leftover(snapshot: &TunerSnapshot, key: &ChannelKey) -> bool {
    snapshot
        .entries
        .iter()
        .find(|e| &e.key == key)
        .map(|e| !e.has_subscribers() && e.idle_close_pending)
        .unwrap_or(false)
}

/// Capacity handling for a driver that has no free slot, unified across the
/// exclusive and non-exclusive paths (P2b-3, redesign doc §2.1-8).
///
/// Before P2b-3 these were two different policies: the exclusive path evicted
/// whatever `choose_eviction_target` returned — including a reader with live
/// subscribers — while the non-exclusive path only ever considered idle
/// readers and, finding none, went ahead and created an *extra* reader over
/// `max_instances`. The single rule now is:
///
/// 1. idle reader on this driver, if [`may_evict`] allows it;
/// 2. otherwise the lowest-priority reader on this driver even if it has
///    subscribers, again subject to [`may_evict`] — the driver limit is a
///    hardware fact, so exceeding it is never an option (a client asking for
///    a channel it outranks gets it; the incumbent is stopped);
/// 3. otherwise another candidate driver ([`decide_fallback`]);
/// 4. otherwise reject.
fn decide_at_capacity(
    snapshot: &TunerSnapshot,
    req: &TuneRequest,
    candidate_tuples: &[DriverCandidate],
    dll_path: &str,
    primary: ChannelKey,
    max_instances_map: &HashMap<String, i32>,
    exclude_own: Option<&ChannelKey>,
) -> Decision {
    // A group request must consume an unused instance from a sibling driver
    // before evicting a reader on the primary driver.  Previously the
    // primary driver's eviction branch ran first; an exclusive request could
    // therefore displace a live reader even though another driver in the
    // same group still had capacity.
    if let Some(key) = find_fallback_with_capacity(
        candidate_tuples,
        dll_path,
        snapshot,
        max_instances_map,
        exclude_own,
    ) {
        return Decision::Create { key, evict: vec![] };
    }

    let (idle_victim, any_victim) = eviction_options(snapshot, dll_path);
    let lowest_idle_priority = idle_victim.as_ref().map(|(_, p, _)| *p);

    for option in [idle_victim, any_victim].into_iter().flatten() {
        let (victim, victim_priority, _) = option;
        if may_evict(req, victim_priority, is_keep_alive_leftover(snapshot, &victim)) {
            return Decision::Create {
                key: primary,
                evict: vec![victim],
            };
        }
    }

    decide_fallback(
        snapshot,
        req,
        candidate_tuples,
        dll_path,
        max_instances_map,
        exclude_own,
        lowest_idle_priority,
    )
}

/// Find a sibling group candidate with a free instance, without considering
/// eviction.  This is deliberately separate from `decide_fallback`: an
/// available group instance always wins over evicting a reader elsewhere.
fn find_fallback_with_capacity(
    candidate_tuples: &[DriverCandidate],
    skip_path: &str,
    snapshot: &TunerSnapshot,
    max_instances_map: &HashMap<String, i32>,
    exclude_own: Option<&ChannelKey>,
) -> Option<ChannelKey> {
    candidate_tuples.iter().find_map(|(path, space, channel)| {
        if path == skip_path {
            return None;
        }
        let running = snapshot.running_count_excluding(path, exclude_own);
        let max = max_instances_map.get(path).copied().unwrap_or(1);
        has_capacity(running, max).then(|| ChannelKey::space_channel(path.clone(), *space, *channel))
    })
}

/// Walk the remaining candidate drivers, applying the same rule as
/// [`decide_at_capacity`] to each: free slot, else an evictable victim.
///
/// Before P2b-3 this path picked the *first* idle entry it came across and
/// never consulted priority at all (the old
/// `ensure_driver_capacity_with_idle_eviction`), which meant a fallback
/// driver could lose a higher-priority idle reader that the primary driver
/// would have protected.
fn decide_fallback(
    snapshot: &TunerSnapshot,
    req: &TuneRequest,
    candidate_tuples: &[DriverCandidate],
    skip_path: &str,
    max_instances_map: &HashMap<String, i32>,
    exclude_own: Option<&ChannelKey>,
    lowest_idle_priority_at_primary: Option<i32>,
) -> Decision {
    for (path, space, channel) in candidate_tuples.iter() {
        if path == skip_path {
            continue;
        }
        let key = ChannelKey::space_channel(path.clone(), *space, *channel);
        let running = snapshot.running_count_excluding(path, exclude_own);
        let max = max_instances_map.get(path).copied().unwrap_or(1);

        if has_capacity(running, max) {
            return Decision::Create { key, evict: vec![] };
        }

        let (idle_victim, any_victim) = eviction_options(snapshot, path);
        for option in [idle_victim, any_victim].into_iter().flatten() {
            let (victim, victim_priority, _) = option;
            if victim == key {
                // Never evict the very entry we are about to (re)use.
                continue;
            }
            if may_evict(req, victim_priority, is_keep_alive_leftover(snapshot, &victim)) {
                return Decision::Create { key, evict: vec![victim] };
            }
        }
    }

    Decision::Reject {
        reason: RejectReason::AtCapacity {
            lowest_idle_priority: lowest_idle_priority_at_primary,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Moved from `server::session_driver_selection` (unchanged).
    // -----------------------------------------------------------------

    /// Basic priority order: fewer exclusive channels wins first; ties
    /// broken by fewer running instances; ties broken by higher quality
    /// score (descending).
    #[test]
    fn sort_candidate_drivers_orders_by_exclusive_then_load_then_score() {
        let mut candidates: Vec<DriverCandidate> = vec![
            ("Busy.dll".to_string(), 0, 1),
            ("Exclusive.dll".to_string(), 0, 2),
            ("Idle.dll".to_string(), 0, 3),
        ];
        let mut exclusive_map = HashMap::new();
        exclusive_map.insert("Exclusive.dll".to_string(), 3i64);
        exclusive_map.insert("Busy.dll".to_string(), 0i64);
        exclusive_map.insert("Idle.dll".to_string(), 0i64);

        let mut instances_map = HashMap::new();
        instances_map.insert("Busy.dll".to_string(), 2i32);
        instances_map.insert("Idle.dll".to_string(), 0i32);

        let score_map = HashMap::new();

        sort_candidate_drivers(&mut candidates, &exclusive_map, &instances_map, &score_map);

        assert_eq!(
            candidates.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            vec!["Idle.dll", "Busy.dll", "Exclusive.dll"]
        );
    }

    #[test]
    fn sort_candidate_drivers_breaks_ties_by_higher_quality_score() {
        let mut candidates: Vec<DriverCandidate> = vec![
            ("Low.dll".to_string(), 0, 1),
            ("High.dll".to_string(), 0, 2),
        ];
        let exclusive_map = HashMap::new();
        let instances_map = HashMap::new();
        let mut score_map = HashMap::new();
        score_map.insert("Low.dll".to_string(), 0.5);
        score_map.insert("High.dll".to_string(), 0.9);

        sort_candidate_drivers(&mut candidates, &exclusive_map, &instances_map, &score_map);

        assert_eq!(
            candidates.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            vec!["High.dll", "Low.dll"]
        );
    }

    #[test]
    fn select_running_driver_prefers_same_physical_channel_already_streaming() {
        let candidates: Vec<DriverCandidate> = vec![
            ("A.dll".to_string(), 0, 27),
            ("B.dll".to_string(), 0, 5),
        ];
        let running = vec![(
            "B.dll".to_string(),
            ChannelKeySpec::SpaceChannel { space: 0, channel: 5 },
        )];

        let selected = select_running_driver(&candidates, &running);
        assert_eq!(selected, Some(("B.dll".to_string(), 0, 5)));
    }

    #[test]
    fn select_running_driver_returns_none_when_nothing_matches() {
        let candidates: Vec<DriverCandidate> = vec![("A.dll".to_string(), 0, 27)];
        let running = vec![(
            "A.dll".to_string(),
            ChannelKeySpec::SpaceChannel { space: 1, channel: 99 },
        )];
        assert_eq!(select_running_driver(&candidates, &running), None);
    }

    // -----------------------------------------------------------------
    // Moved from `server::session_capacity` (unchanged).
    // -----------------------------------------------------------------

    // (a) Same DLL, spare capacity, no forcing (SetChannelSpace/SetChannel
    // caller): allowed to idle-close instead of a synchronous stop.
    #[test]
    fn same_dll_with_spare_capacity_and_no_force_schedules_idle_close() {
        assert!(!should_sync_stop_old_reader(true, false, 1, 4));
    }

    // (b) Same DLL, forced (SelectLogicalChannel caller): stop
    // synchronously even with spare capacity.
    #[test]
    fn same_dll_with_force_stops_synchronously_even_with_spare_capacity() {
        assert!(should_sync_stop_old_reader(true, true, 1, 4));
    }

    // (c) At/over capacity always forces a synchronous stop.
    #[test]
    fn over_capacity_stops_synchronously_regardless_of_force_flag() {
        assert!(should_sync_stop_old_reader(true, false, 4, 4));
        assert!(should_sync_stop_old_reader(false, false, 4, 4));
        assert!(should_sync_stop_old_reader(false, true, 4, 4));
    }

    // (d) Different DLL, spare capacity: never forced.
    #[test]
    fn different_dll_with_spare_capacity_schedules_idle_close() {
        assert!(!should_sync_stop_old_reader(false, false, 1, 4));
        assert!(!should_sync_stop_old_reader(false, true, 1, 4));
    }

    // -----------------------------------------------------------------
    // New in P0: `decide` tests.
    // -----------------------------------------------------------------

    fn driver(path: &str, max_instances: i32) -> DriverState {
        DriverState {
            dll_path: path.to_string(),
            max_instances,
            quality_score: 1.0,
            exclusive_channel_count: 0,
        }
    }

    fn entry(path: &str, space: u32, channel: u32, running: bool, subscribers: u32, priority: i32) -> EntryState {
        use crate::tuner::shared::ReaderState;
        EntryState {
            key: ChannelKey::space_channel(path, space, channel),
            // `running: bool` maps onto the two states every existing test
            // cares about; `decide()` only ever distinguishes `Running` from
            // "anything else", so `Stopped` is an arbitrary-but-equivalent
            // stand-in for every `false` case.
            state: if running { ReaderState::Running } else { ReaderState::Stopped },
            subscribers,
            priority,
            // Most tests are about live-vs-idle and priority; a
            // subscriber-less entry stands in for a keep-alive leftover
            // unless a test says otherwise.
            idle_close_pending: subscribers == 0,
        }
    }

    fn base_request(candidates: Vec<ChannelKey>) -> TuneRequest {
        TuneRequest {
            candidates,
            priority: 0,
            exclusive: false,
            own_key: None,
            own_key_will_free_slot: false,
        }
    }

    #[test]
    fn reject_when_no_candidates() {
        let snapshot = TunerSnapshot::default();
        let req = base_request(vec![]);
        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reject { reason: RejectReason::NoCandidates }
        );
    }

    /// Rule: reuse is selected over everything else — even when the
    /// snapshot has spare capacity elsewhere that would otherwise "win" the
    /// sort, and even for an exclusive request.
    #[test]
    fn reuse_is_selected_first_over_capacity_and_exclusive() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 4), driver("B.dll", 4)],
            entries: vec![
                // A.dll is already running the exact requested channel,
                // with an active subscriber — even so, this must win.
                entry("A.dll", 0, 5, true, 1, 0),
            ],
        };
        let candidates = vec![
            ChannelKey::space_channel("A.dll", 0, 5),
            ChannelKey::space_channel("B.dll", 0, 5),
        ];
        let mut req = base_request(candidates);
        req.exclusive = true;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reuse { key: ChannelKey::space_channel("A.dll", 0, 5) }
        );
    }

    /// Rule: with spare capacity, create with no eviction.
    #[test]
    fn creates_with_no_eviction_when_capacity_available() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 4)],
            entries: vec![entry("A.dll", 0, 1, true, 1, 0)],
        };
        let req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 5)]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("A.dll", 0, 5), evict: vec![] }
        );
    }

    /// At capacity, non-exclusive: a strictly higher priority evicts the
    /// lowest-priority *idle* tuner, leaving the subscribed one alone even
    /// though it also outranks the request's own priority.
    #[test]
    fn capacity_limit_evicts_lowest_priority_idle_tuner_when_priority_is_higher() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 0, 5), // idle, priority 5 (lowest)
                entry("A.dll", 0, 2, true, 1, 10), // has a subscriber
            ],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 6;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// P2b-3: a tie does **not** displace a *live viewer*. Previously `>=`
    /// let an equally-ranked request bump whoever got there first, which
    /// gains nothing and interrupts a working stream.
    #[test]
    fn equal_priority_does_not_evict_a_live_viewer() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 1, 7)],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 7; // exactly equal, not strictly greater.

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reject { reason: RejectReason::AtCapacity { lowest_idle_priority: None } }
        );
    }

    /// ...but an *idle* incumbent yields even on a tie. It is only still
    /// running because of the keep-alive window, and letting that block a
    /// real viewer would reject the request outright on a fully-booked
    /// group. (Found by the concurrent-session matrix on real hardware: a
    /// keep-alive reader left over from a previous run kept turning away a
    /// new viewer.)
    #[test]
    fn an_idle_incumbent_yields_even_on_a_priority_tie() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 0, 7)],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 7;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// ...but an `exclusive` request wins ties: it is asking for the
    /// hardware outright (P2b-3).
    #[test]
    fn exclusive_request_wins_a_priority_tie() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 1, 7)],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 7;
        req.exclusive = true;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// Rule: insufficient priority against a live viewer, with no fallback
    /// candidate, rejects.
    #[test]
    fn capacity_limit_rejects_when_priority_insufficient_and_no_fallback() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 2, 100)],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 1;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reject { reason: RejectReason::AtCapacity { lowest_idle_priority: None } }
        );
    }

    /// Rule: insufficient priority on the primary driver, but a fallback
    /// candidate has spare capacity — falls over to it with no eviction.
    #[test]
    fn falls_back_to_candidate_with_spare_capacity_when_primary_priority_insufficient() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 3, 100), // live viewers, outranks us
                entry("B.dll", 0, 1, true, 1, 0),
            ],
        };
        let mut req = base_request(vec![
            ChannelKey::space_channel("A.dll", 0, 9),
            ChannelKey::space_channel("B.dll", 0, 9),
        ]);
        req.priority = 1;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("B.dll", 0, 9), evict: vec![] }
        );
    }

    /// A free sibling instance is preferred over evicting a live viewer on
    /// the primary driver, including for an exclusive request.
    #[test]
    fn group_spare_capacity_precedes_exclusive_eviction_on_primary() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 1, 100), // primary is full
                entry("B.dll", 0, 1, true, 1, 0),   // sibling still has a slot
            ],
        };
        let mut req = base_request(vec![
            ChannelKey::space_channel("A.dll", 0, 9),
            ChannelKey::space_channel("B.dll", 0, 9),
        ]);
        req.exclusive = true;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("B.dll", 0, 9), evict: vec![] }
        );
    }

    /// The same ordering applies to non-exclusive requests: use group
    /// capacity before displacing a live primary reader that we outrank.
    #[test]
    fn group_spare_capacity_precedes_nonexclusive_eviction_on_primary() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 1, 1), // primary is full
                entry("B.dll", 0, 1, true, 1, 0), // sibling still has a slot
            ],
        };
        let mut req = base_request(vec![
            ChannelKey::space_channel("A.dll", 0, 9),
            ChannelKey::space_channel("B.dll", 0, 9),
        ]);
        req.priority = 2;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("B.dll", 0, 9), evict: vec![] }
        );
    }

    /// Exclusive access at capacity evicts to make room even though the
    /// incumbent has an active subscriber. Since P2b-3 the non-exclusive
    /// path can do this too (subject to a strictly higher priority) — what
    /// stays exclusive-only is winning a tie.
    #[test]
    fn exclusive_access_evicts_a_subscribed_tuner_when_nothing_idle_is_left() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 3, 0)], // has 3 subscribers
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.exclusive = true;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// Rule 7 corollary: exclusive access prefers an idle incumbent over a
    /// subscribed one when both exist (matches `choose_eviction_target`'s
    /// "idle first" preference).
    #[test]
    fn exclusive_access_prefers_idle_incumbent_over_subscribed_one() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 0, 50), // idle
                entry("A.dll", 0, 2, true, 1, 0),  // subscribed, lower priority
            ],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.exclusive = true;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// P2b-3: `max_instances` is a hardware fact, so a driver is never
    /// pushed past it. With no idle victim and nothing this request
    /// outranks, the answer is a rejection — previously it silently created
    /// an extra reader over the limit (§2.1-8).
    #[test]
    fn never_creates_over_capacity_when_nothing_can_be_evicted() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 1, 0)], // only entry has a subscriber
        };
        let req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reject { reason: RejectReason::AtCapacity { lowest_idle_priority: None } }
        );
    }

    /// P2b-3: with no idle reader left, a higher-priority request stops a
    /// *subscribed* one rather than exceed the driver limit. This is the
    /// deliberate consequence of "never over capacity": a live viewer can be
    /// displaced by a recording-grade request.
    #[test]
    fn higher_priority_evicts_a_subscribed_reader_rather_than_exceed_capacity() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 2, 10)], // two live viewers, priority 10
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 200; // recording-grade

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// Idle first: a subscribed reader is only displaced when there is no
    /// idle one to take instead, even if the subscribed one ranks lower.
    #[test]
    fn an_idle_reader_is_preferred_over_a_lower_priority_subscribed_one() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 0, 50), // idle, higher priority
                entry("A.dll", 0, 2, true, 1, 1),  // subscribed, lower priority
            ],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 200;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// A driver whose only entry is still *opening* is not free. Counting
    /// just `Running` made concurrent group selections all pick the same
    /// driver — every one of them saw it idle for the whole BonDriver-open
    /// window, and all but the winner failed (including on retry, since each
    /// fresh snapshot repeated the mistake).
    #[test]
    fn a_starting_reader_makes_its_driver_count_as_occupied() {
        let mut starting = entry("A.dll", 0, 1, true, 0, 0);
        starting.state = crate::tuner::shared::ReaderState::Starting;
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 1)],
            entries: vec![starting],
        };
        let req = base_request(vec![
            ChannelKey::space_channel("A.dll", 0, 9),
            ChannelKey::space_channel("B.dll", 0, 9),
        ]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("B.dll", 0, 9), evict: vec![] },
            "the second requester must move to the free driver, not pile onto the one mid-open"
        );
    }

    /// The same goes for `Reserved`: the pool has handed the entry out and a
    /// caller owes it a reader start.
    #[test]
    fn a_reserved_entry_makes_its_driver_count_as_occupied() {
        let mut reserved = entry("A.dll", 0, 1, true, 0, 0);
        reserved.state = crate::tuner::shared::ReaderState::Reserved;
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 1)],
            entries: vec![reserved],
        };
        let req = base_request(vec![
            ChannelKey::space_channel("A.dll", 0, 9),
            ChannelKey::space_channel("B.dll", 0, 9),
        ]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("B.dll", 0, 9), evict: vec![] }
        );
    }

    /// Joining wins over opening a second instance even while the first is
    /// still coming up: a request for the channel a driver is *opening*
    /// reuses that entry.
    #[test]
    fn a_request_for_a_channel_being_opened_joins_it() {
        let mut starting = entry("A.dll", 0, 9, true, 0, 0);
        starting.state = crate::tuner::shared::ReaderState::Starting;
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 1)],
            entries: vec![starting],
        };
        let req = base_request(vec![
            ChannelKey::space_channel("A.dll", 0, 9),
            ChannelKey::space_channel("B.dll", 0, 9),
        ]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reuse { key: ChannelKey::space_channel("A.dll", 0, 9) }
        );
    }

    /// A tuner that has been tuned but not yet subscribed to must not be
    /// taken away: its caller is between `SetChannelSpace` and
    /// `StartStream`. Only a *keep-alive leftover* (idle-close already
    /// counting down) yields on a tie.
    ///
    /// Found on hardware: with "no subscribers" alone as the test, five
    /// sessions starting at once evicted each other's freshly-tuned readers
    /// and every one of them ended up with zero bytes.
    #[test]
    fn a_tuned_but_not_yet_subscribed_reader_is_not_taken_over() {
        let mut fresh = entry("A.dll", 0, 1, true, 0, 0);
        fresh.idle_close_pending = false; // nobody scheduled a keep-alive: it is still being set up
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![fresh],
        };
        let req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reject { reason: RejectReason::AtCapacity { lowest_idle_priority: Some(0) } },
            "a reader whose subscriber has not attached yet must survive"
        );
    }

    /// Rule 8: this session's own slot, when it will be freed by the
    /// switch, is excluded from the driver's running count — so a driver
    /// that looks "full" by raw count still has capacity once the caller's
    /// own about-to-be-released tuner is subtracted.
    #[test]
    fn own_freed_slot_is_excluded_from_capacity_count() {
        let own_key = ChannelKey::space_channel("A.dll", 0, 1);
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![EntryState {
                key: own_key.clone(),
                state: crate::tuner::shared::ReaderState::Running,
                subscribers: 0,
                priority: 0,
                idle_close_pending: true,
            }],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.own_key = Some(own_key);
        req.own_key_will_free_slot = true;

        // Without the exclusion this would be at capacity (1/1 running);
        // with it, the driver has room and no eviction is needed.
        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("A.dll", 0, 9), evict: vec![] }
        );
    }

    /// Rule 8 corollary: when the session's slot will NOT be freed (e.g.
    /// it's a different driver, or it still has other subscribers), it
    /// counts normally and capacity logic proceeds as usual.
    #[test]
    fn own_slot_counts_normally_when_not_marked_as_freed() {
        let own_key = ChannelKey::space_channel("A.dll", 0, 1);
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![EntryState {
                key: own_key.clone(),
                state: crate::tuner::shared::ReaderState::Running,
                subscribers: 0,
                priority: 42,
                idle_close_pending: true,
            }],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.own_key = Some(own_key.clone());
        req.own_key_will_free_slot = false;
        req.priority = 100; // sufficient to evict if capacity logic runs

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![own_key],
            }
        );
    }

    /// Rule 1/3/4: candidate ordering feeds directly into which driver is
    /// tried first — a driver with fewer running instances is preferred
    /// even when listed second in the request.
    #[test]
    fn candidate_ordering_prefers_less_loaded_driver_regardless_of_request_order() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("Busy.dll", 4), driver("Idle.dll", 4)],
            entries: vec![
                entry("Busy.dll", 0, 1, true, 1, 0),
                entry("Busy.dll", 0, 2, true, 1, 0),
            ],
        };
        // Busy.dll listed first in the request, but Idle.dll has fewer
        // running instances and should be picked (and, being empty, has
        // capacity outright).
        let req = base_request(vec![
            ChannelKey::space_channel("Busy.dll", 0, 9),
            ChannelKey::space_channel("Idle.dll", 0, 9),
        ]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("Idle.dll", 0, 9), evict: vec![] }
        );
    }
}
