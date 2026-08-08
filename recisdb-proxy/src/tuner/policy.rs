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
}

impl EntryState {
    pub fn has_subscribers(&self) -> bool {
        self.subscribers > 0
    }

    /// Equivalent to the old `running: bool` field / `SharedTuner::is_running()`.
    pub fn is_running(&self) -> bool {
        self.state == crate::tuner::shared::ReaderState::Running
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
            .filter(|e| e.is_running() && e.key.tuner_path == dll_path && exclude != Some(&e.key))
            .count() as i32
    }

    fn running_channel_specs(&self) -> Vec<(String, ChannelKeySpec)> {
        self.entries
            .iter()
            .filter(|e| e.is_running())
            .map(|e| (e.key.tuner_path.clone(), e.key.channel.clone()))
            .collect()
    }

    fn first_idle_entry(&self, dll_path: &str, exclude: Option<&ChannelKey>) -> Option<ChannelKey> {
        self.entries
            .iter()
            .find(|e| {
                e.key.tuner_path == dll_path
                    && e.is_running()
                    && !e.has_subscribers()
                    && exclude != Some(&e.key)
            })
            .map(|e| e.key.clone())
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
        if !e.is_running() || exclude == Some(&e.key) {
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

    if req.exclusive {
        decide_exclusive_at_capacity(snapshot, &path, primary)
    } else {
        decide_capacity_at_limit(snapshot, req, &candidate_tuples, &path, primary, &max_instances_map, exclude_own)
    }
}

/// Rule 7: exclusive-access eviction. Only reached once already-at-capacity
/// (checked by the caller) and once the requested channel is confirmed not
/// already running (checked by the caller via the global reuse
/// short-circuit — this is what "skip eviction, requested already running"
/// collapses into). Evicts a tuner on `dll_path` even if it currently has
/// subscribers — see the module doc comment's "faithfulness" note; this is
/// current behavior, fixed in P2 (redesign doc §2.1-8).
fn decide_exclusive_at_capacity(snapshot: &TunerSnapshot, dll_path: &str, primary: ChannelKey) -> Decision {
    let candidates: Vec<EvictionCandidate> = snapshot
        .entries
        .iter()
        .filter(|e| e.key.tuner_path == dll_path && e.is_running())
        .map(|e| (e.key.clone(), e.priority, e.has_subscribers()))
        .collect();

    match choose_eviction_target(&candidates) {
        Some((victim, _, _)) => Decision::Create {
            key: primary,
            evict: vec![victim],
        },
        // Over capacity per the running count but nothing found to evict —
        // shouldn't happen in practice (running >= max implies at least one
        // running entry), but mirrors `handle_set_channel_space_exclusive_access`
        // which has no other fallback in this branch either.
        None => Decision::Create { key: primary, evict: vec![] },
    }
}

/// Rule 6: non-exclusive capacity-limit handling. Only ever considers idle
/// (no-subscriber) tuners for eviction — deliberately narrower than the
/// exclusive path above (current behavior, see module doc comment).
fn decide_capacity_at_limit(
    snapshot: &TunerSnapshot,
    req: &TuneRequest,
    candidate_tuples: &[DriverCandidate],
    dll_path: &str,
    primary: ChannelKey,
    max_instances_map: &HashMap<String, i32>,
    exclude_own: Option<&ChannelKey>,
) -> Decision {
    let idle_candidates: Vec<EvictionCandidate> = snapshot
        .entries
        .iter()
        .filter(|e| e.key.tuner_path == dll_path && e.is_running() && !e.has_subscribers())
        .map(|e| (e.key.clone(), e.priority, false))
        .collect();

    match choose_eviction_target(&idle_candidates) {
        Some((victim, lowest_priority, _)) => {
            // `>=`, not `>`: a same-priority request still evicts the
            // incumbent. Current behavior, preserved as-is (§2.1-8).
            if req.priority >= lowest_priority {
                Decision::Create {
                    key: primary,
                    evict: vec![victim],
                }
            } else {
                decide_fallback(
                    snapshot,
                    candidate_tuples,
                    dll_path,
                    max_instances_map,
                    exclude_own,
                    lowest_priority,
                )
            }
        }
        // At/over capacity but no idle tuner exists to evict: current code
        // does not try another candidate here and does not reject — it
        // proceeds to create the tuner anyway, over capacity. Documented
        // quirk, preserved as-is (§2.1-8 / module doc comment).
        None => Decision::Create { key: primary, evict: vec![] },
    }
}

/// Walk the remaining candidates (mirrors `try_fallback_drivers` /
/// `ensure_driver_capacity_with_idle_eviction`): each fallback candidate is
/// used if it has spare capacity, or if an idle tuner can be evicted from
/// it — note this eviction picks the *first* idle entry found, not the
/// lowest-priority one, unlike the primary-driver path above. That
/// asymmetry is current behavior (`ensure_driver_capacity_with_idle_eviction`
/// picks the first idle candidate encountered while scanning pool keys, and
/// never consults DB priority at all).
fn decide_fallback(
    snapshot: &TunerSnapshot,
    candidate_tuples: &[DriverCandidate],
    skip_path: &str,
    max_instances_map: &HashMap<String, i32>,
    exclude_own: Option<&ChannelKey>,
    lowest_priority_at_primary: i32,
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
        if let Some(victim) = snapshot.first_idle_entry(path, Some(&key)) {
            return Decision::Create {
                key,
                evict: vec![victim],
            };
        }
    }

    Decision::Reject {
        reason: RejectReason::AtCapacity {
            lowest_idle_priority: Some(lowest_priority_at_primary),
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

    /// Rule 6: at capacity, non-exclusive, sufficient priority evicts the
    /// lowest-priority *idle* tuner.
    #[test]
    fn capacity_limit_evicts_lowest_priority_idle_tuner_when_priority_sufficient() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 0, 5), // idle, priority 5 (lowest)
                entry("A.dll", 0, 2, true, 1, 10), // has a subscriber
            ],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 5; // equal to the lowest — see the `>=` test below too.

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// Current-behavior fixed. P2 will change `>=` to `>` (redesign doc
    /// §2.1-8 / §4 P2): a request whose priority exactly *equals* the
    /// lowest incumbent's priority still evicts it.
    #[test]
    fn capacity_limit_priority_comparison_uses_gte_current_behavior_fixed_for_p2() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 0, 7)],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 7; // exactly equal, not strictly greater.

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create {
                key: ChannelKey::space_channel("A.dll", 0, 9),
                evict: vec![ChannelKey::space_channel("A.dll", 0, 1)],
            }
        );
    }

    /// Rule: insufficient priority with no fallback candidate rejects.
    #[test]
    fn capacity_limit_rejects_when_priority_insufficient_and_no_fallback() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 0, 100)],
        };
        let mut req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);
        req.priority = 1;

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Reject { reason: RejectReason::AtCapacity { lowest_idle_priority: Some(100) } }
        );
    }

    /// Rule: insufficient priority on the primary driver, but a fallback
    /// candidate has spare capacity — falls over to it with no eviction.
    #[test]
    fn falls_back_to_candidate_with_spare_capacity_when_primary_priority_insufficient() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1), driver("B.dll", 2)],
            entries: vec![
                entry("A.dll", 0, 1, true, 0, 100),
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

    /// Rule 7: exclusive access at capacity evicts to make room even though
    /// the incumbent has an active subscriber (current behavior — the
    /// capacity-limit path, tested above, would never touch a subscribed
    /// tuner). Fixed for now; P2 unifies this (redesign doc §2.1-8).
    #[test]
    fn exclusive_access_evicts_subscribed_tuner_current_behavior_fixed_for_p2() {
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

    /// Current-behavior fixed. When at capacity and no *idle* tuner exists
    /// on the primary driver, the non-exclusive path does not evict, does
    /// not fall back, and does not reject — it just creates over capacity
    /// (§2.1-8 / module doc comment).
    #[test]
    fn capacity_limit_creates_over_capacity_when_no_idle_victim_current_behavior_fixed_for_p2() {
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![entry("A.dll", 0, 1, true, 1, 0)], // only entry has a subscriber
        };
        let req = base_request(vec![ChannelKey::space_channel("A.dll", 0, 9)]);

        assert_eq!(
            decide(&snapshot, &req),
            Decision::Create { key: ChannelKey::space_channel("A.dll", 0, 9), evict: vec![] }
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
