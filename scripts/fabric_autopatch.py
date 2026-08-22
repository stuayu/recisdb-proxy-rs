#!/usr/bin/env python3
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


def write(path, text):
    (ROOT / path).write_text(text, encoding="utf-8")


def replace_once(path, old, new):
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected exactly one literal match, got {count}: {old[:100]!r}")
    write(path, text.replace(old, new, 1))


def regex_once(path, pattern, repl, flags=0):
    text = read(path)
    new, count = re.subn(pattern, repl, text, count=1, flags=flags)
    if count != 1:
        raise RuntimeError(f"{path}: expected exactly one regex match, got {count}: {pattern[:120]!r}")
    write(path, new)


def replace_in_region(path, start, end, old, new):
    text = read(path)
    a = text.index(start)
    b = text.index(end, a)
    region = text[a:b]
    count = region.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: region {start[:40]!r}: expected one match, got {count}")
    region = region.replace(old, new, 1)
    write(path, text[:a] + region + text[b:])


# ---------------------------------------------------------------------------
# policy.rs: occupancy != joinability. Stopping still consumes a slot but may
# never be returned as a reusable reader.
# ---------------------------------------------------------------------------
POLICY = "recisdb-proxy/src/tuner/policy.rs"
replace_once(
    POLICY,
    """    pub fn is_running(&self) -> bool {\n        self.state == crate::tuner::shared::ReaderState::Running\n    }\n\n    /// Whether this entry is holding a slot on its driver — mirrors\n""",
    """    pub fn is_running(&self) -> bool {\n        self.state == crate::tuner::shared::ReaderState::Running\n    }\n\n    /// Whether a new client may join this reader. `Starting` is joinable: the\n    /// reader start is already owned by another request and subscribers may\n    /// wait for it to become ready. `Stopping` is deliberately *not* joinable\n    /// even though it still occupies a driver slot.\n    pub fn is_joinable(&self) -> bool {\n        use crate::tuner::shared::ReaderState;\n        matches!(self.state, ReaderState::Starting | ReaderState::Running)\n    }\n\n    /// Whether this entry is holding a slot on its driver — mirrors\n""",
)
replace_once(
    POLICY,
    """    fn running_channel_specs(&self) -> Vec<(String, ChannelKeySpec)> {\n        self.entries\n            .iter()\n            // `occupies_slot`, not `is_running`: a driver already opening\n            // this exact channel is the right one to join rather than open a\n            // second instance for.\n            .filter(|e| e.occupies_slot())\n            .map(|e| (e.key.tuner_path.clone(), e.key.channel.clone()))\n            .collect()\n    }\n""",
    """    fn running_channel_specs(&self) -> Vec<(String, ChannelKeySpec)> {\n        self.entries\n            .iter()\n            // Starting/Running can be joined. Stopping still occupies a slot\n            // for capacity accounting but must never be resurrected by Reuse.\n            .filter(|e| e.is_joinable())\n            .map(|e| (e.key.tuner_path.clone(), e.key.channel.clone()))\n            .collect()\n    }\n""",
)
replace_once(
    POLICY,
    """    /// Effective client/DB priority for the requested channel (already\n    /// resolved: client priority if `> 0`, else `i32::MAX` if `exclusive`,\n    /// else the DB default — mirrors `handle_set_channel_space`'s\n    /// `channel_priority` computation, which stays outside `decide` since\n    /// it needs a DB read).\n""",
    """    /// Effective client/DB priority for the requested channel. Priority\n    /// and exclusivity are independent rank components: callers use an\n    /// explicit client priority when `> 0`, otherwise the DB default, and\n    /// pass `exclusive` separately as the tie-breaker.\n""",
)
replace_once(
    POLICY,
    """//! **This module is not wired in yet.** `session.rs`'s existing helpers\n//! (`handle_set_channel_space` and friends) keep making the real decisions\n//! for now; replacing their control flow with calls into [`decide`] is P2.\n//! Wiring it up requires a second effect layer (`tuner/acquire.rs`, P2) that\n//! turns a `Decision` into pool mutations, reader starts, and evictions.\n""",
    """//! **This module is live.** All modern tuning paths feed their physical\n//! candidate set through `tuner/acquire.rs`, which snapshots state, calls\n//! [`decide`], and executes the returned decision. Keep selection policy here\n//! rather than reintroducing caller-specific preselection.\n""",
)
# Append regression tests inside the existing test module.
text = read(POLICY)
insert = r'''

    #[test]
    fn stopping_reader_occupies_capacity_but_is_never_reused() {
        use crate::tuner::shared::ReaderState;
        let key = ChannelKey::space_channel("A.dll", 0, 7);
        let stopping = EntryState {
            key: key.clone(),
            state: ReaderState::Stopping,
            subscribers: 0,
            priority: 0,
            incumbent_exclusive: false,
            held_for: Duration::from_secs(30),
            idle_close_pending: false,
        };
        assert!(stopping.occupies_slot());
        assert!(!stopping.is_joinable());

        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![stopping],
        };
        let req = TuneRequest {
            candidates: vec![key],
            priority: 0,
            exclusive: false,
            min_hold: Duration::ZERO,
            own_key: None,
            own_key_will_free_slot: false,
        };
        assert!(!matches!(decide(&snapshot, &req), Decision::Reuse { .. }));
    }

    #[test]
    fn starting_reader_is_joinable_without_opening_a_duplicate() {
        use crate::tuner::shared::ReaderState;
        let key = ChannelKey::space_channel("A.dll", 0, 7);
        let starting = EntryState {
            key: key.clone(),
            state: ReaderState::Starting,
            subscribers: 0,
            priority: 0,
            incumbent_exclusive: false,
            held_for: Duration::ZERO,
            idle_close_pending: false,
        };
        let snapshot = TunerSnapshot {
            drivers: vec![driver("A.dll", 1)],
            entries: vec![starting],
        };
        let req = TuneRequest {
            candidates: vec![key.clone()],
            priority: 0,
            exclusive: false,
            min_hold: Duration::ZERO,
            own_key: None,
            own_key_will_free_slot: false,
        };
        assert_eq!(decide(&snapshot, &req), Decision::Reuse { key });
    }
'''
pos = text.rfind("\n}")
if pos < 0:
    raise RuntimeError("policy.rs: test module closing brace not found")
write(POLICY, text[:pos] + insert + text[pos:])


# ---------------------------------------------------------------------------
# acquire.rs: take one incumbent claim snapshot and revalidate Reuse after the
# async policy snapshot so a Stopping reader cannot win a TOCTOU race.
# ---------------------------------------------------------------------------
ACQUIRE = "recisdb-proxy/src/tuner/acquire.rs"
replace_once(
    ACQUIRE,
    "use crate::tuner::shared::{ReaderStartupConfig, StopReason};",
    "use crate::tuner::shared::{ReaderStartupConfig, ReaderState, StopReason};",
)
replace_once(
    ACQUIRE,
    """        raw_entries.push(RawEntry {\n            key,\n            state: tuner.state(),\n            subscribers: tuner.subscriber_count(),\n            priority: tuner.incumbent_claim().map(|c| c.priority).unwrap_or(0),\n            incumbent_exclusive: tuner\n                .incumbent_claim()\n                .map(|c| c.exclusive)\n                .unwrap_or(false),\n            held_for: tuner.held_for(),\n            space,\n            channel,\n        });\n""",
    """        // Read the incumbent rank atomically with respect to the claims\n        // mutex. Calling incumbent_claim() twice can combine priority from one\n        // subscription set with exclusive from a later one.\n        let incumbent = tuner.incumbent_claim();\n        raw_entries.push(RawEntry {\n            key,\n            state: tuner.state(),\n            subscribers: tuner.subscriber_count(),\n            priority: incumbent.map(|c| c.priority).unwrap_or(0),\n            incumbent_exclusive: incumbent.map(|c| c.exclusive).unwrap_or(false),\n            held_for: tuner.held_for(),\n            space,\n            channel,\n        });\n""",
)
replace_once(
    ACQUIRE,
    """                match pool.get(&key).await {\n                    Some(tuner) => {\n                        pool.reject_gate().clear(&gate_key);\n                        return Ok(AcquireOutcome {\n                            tuner,\n                            key,\n                            reused: true,\n                            unused_permit: carried_permit,\n                            unused_warm: warm,\n                        });\n                    }\n                    None => {\n                        // The entry `decide` saw as running vanished (raced\n                        // stop/evict elsewhere) between snapshot and now —\n                        // stale snapshot, try again.\n                        continue;\n                    }\n                }\n""",
    """                match pool.get(&key).await {\n                    Some(tuner)\n                        if matches!(tuner.state(), ReaderState::Starting | ReaderState::Running) =>\n                    {\n                        pool.reject_gate().clear(&gate_key);\n                        return Ok(AcquireOutcome {\n                            tuner,\n                            key,\n                            reused: true,\n                            unused_permit: carried_permit,\n                            unused_warm: warm,\n                        });\n                    }\n                    Some(tuner) => {\n                        info!(\n                            "[acquire] stale reuse decision for {:?}: reader moved to {:?}; retrying",\n                            key,\n                            tuner.state()\n                        );\n                        continue;\n                    }\n                    None => {\n                        // The entry `decide` saw as joinable vanished between\n                        // snapshot and execution — stale snapshot, try again.\n                        continue;\n                    }\n                }\n""",
)


# ---------------------------------------------------------------------------
# session.rs: canonical priority/exclusive rank, remove preselection, and send
# all SelectLogicalChannel physical routes to acquire() in one request.
# ---------------------------------------------------------------------------
SESSION = "recisdb-proxy/src/server/session.rs"
replace_once(
    SESSION,
    "use crate::server::session_driver_selection::select_group_driver_for_channel;\n",
    "",
)
# v1: exclusive is independent; use DB default when no explicit priority.
replace_once(
    SESSION,
    """        // STREAMING_DESIGN.md §2: mirror the v2 SetChannelSpace priority\n        // resolution (client priority > exclusive-max > DB default) so v1\n        // SetChannel sessions get the same auto-promotion behavior.\n        let channel_priority_for_class = if effective_priority > 0 {\n            effective_priority\n        } else if effective_exclusive {\n            i32::MAX\n        } else {\n            let db = self.database.lock().await;\n            db.get_channel_priority(&tuner_path, 0, channel as u32)\n                .unwrap_or(Some(0))\n                .unwrap_or(0)\n        };\n        self.maybe_promote_stream_class(channel_priority_for_class).await;\n""",
    """        // Priority and exclusivity are independent rank components. An\n        // exclusive request with no explicit priority inherits the DB default\n        // instead of being rewritten to i32::MAX. The same canonical values\n        // are stored for the eventual incumbent subscription below.\n        let channel_priority_for_class = if effective_priority > 0 {\n            effective_priority\n        } else {\n            let db = self.database.lock().await;\n            db.get_channel_priority(&tuner_path, 0, channel as u32)\n                .unwrap_or(Some(0))\n                .unwrap_or(0)\n        };\n        self.tuner_claim_priority = channel_priority_for_class;\n        self.tuner_claim_exclusive = effective_exclusive;\n        self.maybe_promote_stream_class(channel_priority_for_class).await;\n""",
)
# v2: same rule and overwrite the early raw fields with the canonical claim.
replace_once(
    SESSION,
    """        // ★ Use client-provided priority, or database default if priority <= 0\n        let channel_priority = if priority > 0 {\n            priority\n        } else if exclusive {\n            // If exclusive is requested, use maximum priority\n            i32::MAX\n        } else {\n            // Database default for the *primary* candidate. In group mode the\n            // other candidates are the same logical channel on sibling\n            // drivers and normally carry the same configured priority, so\n            // looking it up once here (rather than per candidate, which would\n            // need the winner before `decide` has picked one) matches what\n            // this path did before P2b-2.\n            let db = self.database.lock().await;\n            db.get_channel_priority(&tuner_path, actual_space, actual_bon_channel)\n                .unwrap_or(Some(0))\n                .unwrap_or(0)\n        };\n\n        // STREAMING_DESIGN.md §2: high-priority (recording-grade) selection\n""",
    """        // Use explicit client priority, otherwise the DB default. Exclusive\n        // remains a separate tie-breaker and must never be encoded into the\n        // numeric priority. Store exactly the rank handed to acquire so the\n        // incumbent subscription cannot later appear weaker than its request.\n        let channel_priority = if _priority > 0 {\n            _priority\n        } else {\n            let db = self.database.lock().await;\n            db.get_channel_priority(&tuner_path, actual_space, actual_bon_channel)\n                .unwrap_or(Some(0))\n                .unwrap_or(0)\n        };\n        self.tuner_claim_priority = channel_priority;\n        self.tuner_claim_exclusive = _exclusive;\n\n        // STREAMING_DESIGN.md §2: high-priority (recording-grade) selection\n""",
)
# v2 request must use the effective exclusive control, not the raw wire value.
replace_once(
    SESSION,
    """        let request = AcquireRequest {\n            candidates,\n            priority: channel_priority,\n            exclusive,\n            bondriver_version: 2,\n""",
    """        let request = AcquireRequest {\n            candidates,\n            priority: channel_priority,\n            exclusive: _exclusive,\n            bondriver_version: 2,\n""",
)
# SetChannelSpace group preselection is data-only now: policy/acquire chooses.
regex_once(
    SESSION,
    r"        let \(tuner_path, actual_space, actual_bon_channel\) = if !self\.group_driver_paths\.is_empty\(\) \{.*?        \};\n\n        info!\(\n            \"\[Session \{\}\] SetChannelSpace:",
    """        let (tuner_path, actual_space, actual_bon_channel) = if !self.group_driver_paths.is_empty() {\n            let Some((path, driver_space, driver_bon_channel)) = group_candidates.first().cloned() else {\n                error!(\n                    \"[Session {}] SetChannelSpace: Channel NID=0x{:04X} TSID=0x{:04X} not found in any group driver\",\n                    self.id, entry.nid, entry.tsid\n                );\n                return self.send_message(ServerMessage::SetChannelSpaceAck {\n                    success: false,\n                    error_code: ErrorCode::InvalidParameter.into(),\n                }).await;\n            };\n            // This is only a representative used for DB-default priority and\n            // same-DLL permit handoff. The actual winner is chosen once, by\n            // acquire()/policy, from the complete candidate set below.\n            (path, driver_space, driver_bon_channel)\n        } else {\n            match &self.current_tuner_path {\n                Some(p) => (p.clone(), entry.bon_space, entry.bon_channel),\n                None => {\n                    error!(\"[Session {}] SetChannelSpace: current_tuner_path is None\", self.id);\n                    return self.send_message(ServerMessage::SetChannelSpaceAck {\n                        success: false,\n                        error_code: ErrorCode::InvalidState.into(),\n                    }).await;\n                }\n            }\n        };\n\n        info!(\n            \"[Session {}] SetChannelSpace:""",
    flags=re.S,
)
# Remove the one-candidate SelectLogicalChannel helper completely.
regex_once(
    SESSION,
    r"\n    /// Try one candidate driver for a logical \(NID, TSID\) selection\..*?\n    async fn finish_logical_channel_selection_success\(",
    "\n    async fn finish_logical_channel_selection_success(",
    flags=re.S,
)
# Replace outer candidate loop with one all-candidate acquire transaction.
regex_once(
    SESSION,
    r"    /// Handle SelectLogicalChannel message\.\n    async fn handle_select_logical_channel\(.*?\n    /// Handle GetChannelList message\.",
    r'''    /// Handle SelectLogicalChannel message.
    ///
    /// One logical request is one acquire transaction. Every physical route
    /// for the requested (NID, TSID) is presented together so policy can pick
    /// a free/healthy sibling before it ever considers eviction.
    async fn handle_select_logical_channel(
        &mut self,
        nid: u16,
        tsid: u16,
        sid: Option<u16>,
    ) -> std::io::Result<()> {
        if self.state != SessionState::Ready
            && self.state != SessionState::TunerOpen
            && self.state != SessionState::Streaming
        {
            return self
                .send_error(ErrorCode::InvalidState, "Not in ready state")
                .await;
        }

        info!(
            "[Session {}] SelectLogicalChannel: nid={}, tsid={}, sid={:?}",
            self.id, nid, tsid, sid
        );

        let channels = {
            let db = self.database.lock().await;
            match db.get_channels_by_nid_tsid_ordered(nid, tsid, sid) {
                Ok(chs) => chs,
                Err(e) => {
                    drop(db);
                    error!("[Session {}] Failed to query channels: {}", self.id, e);
                    return self.fail_logical_channel_selection().await;
                }
            }
        };
        if channels.is_empty() {
            return self.fail_logical_channel_selection().await;
        }

        let mut candidates = Vec::new();
        for channel_with_driver in &channels {
            let record = &channel_with_driver.channel;
            let key = ChannelKey::space_channel(
                &channel_with_driver.bon_driver_path,
                record.bon_space.unwrap_or(0),
                record.bon_channel.unwrap_or(0),
            );
            if !candidates.contains(&key) {
                candidates.push(key);
            }
        }
        if candidates.is_empty() {
            return self.fail_logical_channel_selection().await;
        }

        // SelectLogicalChannel has no wire-level priority/exclusive fields, so
        // its canonical claim is the configured DB priority of the logical
        // channel and non-exclusive. This same claim is used by arbitration
        // and by the subscription installed after handoff.
        let logical_priority = channels
            .first()
            .map(|c| c.channel.priority)
            .unwrap_or(0);
        self.tuner_claim_priority = logical_priority;
        self.tuner_claim_exclusive = false;
        self.maybe_promote_stream_class(logical_priority).await;

        let old_tuner_key = self.current_tuner.as_ref().map(|t| t.key.clone());
        let old_tuner_for_permit = self.current_tuner.clone();
        let carried_permit: Option<SlotPermit> = old_tuner_for_permit.as_ref().and_then(|old| {
            let sub_count = old.subscriber_count();
            let will_free =
                (sub_count == 1 && self.ts_receiver.is_some()) ||
                (sub_count == 0 && self.ts_receiver.is_none());
            if will_free { old.take_slot_permit() } else { None }
        });
        let own_key_will_free_slot = carried_permit.is_some();
        let warm = self.warm_tuner.take();

        let request = AcquireRequest {
            candidates,
            priority: logical_priority,
            exclusive: false,
            bondriver_version: 2,
            carried_permit,
            warm,
            own_key: old_tuner_key,
            own_key_will_free_slot,
            client_host: self.addr.ip().to_string(),
        };

        let outcome = match acquire(&self.tuner_pool, &self.database, request).await {
            Ok(outcome) => outcome,
            Err(e) => {
                if let Some(old) = old_tuner_for_permit.as_ref() {
                    // acquire returns an unused carried permit only on success;
                    // on failure it is dropped by the executor. The old reader
                    // still owns its physical slot, so if it remains active its
                    // permit must already be retained there (same invariant as
                    // SetChannelSpace's pre-switch handoff path).
                    debug!(
                        "[Session {}] SelectLogicalChannel: acquire failed while old tuner {:?} remains: {}",
                        self.id, old.key, e
                    );
                } else {
                    debug!("[Session {}] SelectLogicalChannel: acquire failed: {}", self.id, e);
                }
                return self.fail_logical_channel_selection().await;
            }
        };

        self.absorb_acquire_leftovers(outcome.unused_permit.is_some(), outcome.unused_warm);
        if let Some(permit) = outcome.unused_permit {
            self.return_unused_permit(permit).await;
        }

        let chosen_path = outcome.key.tuner_path.clone();
        let (chosen_space, chosen_channel) = match outcome.key.channel {
            ChannelKeySpec::SpaceChannel { space, channel } => (space, channel),
            ChannelKeySpec::Simple(c) => (0, c as u32),
        };
        let candidate_idx = channels
            .iter()
            .position(|c| {
                c.bon_driver_path == chosen_path
                    && c.channel.bon_space.unwrap_or(0) == chosen_space
                    && c.channel.bon_channel.unwrap_or(0) == chosen_channel
            })
            .unwrap_or(0);

        self.tuner_pool.cancel_idle_close(&outcome.key).await;
        let cleanup_old = handoff_current_tuner(
            self.id,
            &mut self.ts_receiver,
            &mut self.current_tuner,
            outcome.tuner.clone(),
            self.tuner_claim_priority,
            self.tuner_claim_exclusive,
            self.state == SessionState::Streaming,
            "SelectLogicalChannel:",
        ).await;
        if let Some(old) = cleanup_old {
            cleanup_unused_tuner_after_switch(
                &self.database,
                &self.tuner_pool,
                self.id,
                old,
                Some(chosen_path.as_str()),
                true,
                "SelectLogicalChannel cleanup:",
            ).await;
        }

        self.finish_logical_channel_selection_success(
            &outcome.tuner,
            candidate_idx,
            &chosen_path,
            chosen_space,
            chosen_channel,
        ).await
    }

    /// Handle GetChannelList message.''',
    flags=re.S,
)


# ---------------------------------------------------------------------------
# channel_resolve.rs: request claim is explicit; HTTP-specific subscriberless
# eviction is removed because it can race the initial subscriber attach.
# ---------------------------------------------------------------------------
RESOLVE = "recisdb-proxy/src/server/channel_resolve.rs"
replace_once(RESOLVE, "use log::{info, warn};", "use log::info;")
replace_once(RESOLVE, "use crate::tuner::timing;\n", "")
replace_once(
    RESOLVE,
    "use crate::tuner::{ChannelKey, SharedTuner, TunerPool};",
    "use crate::tuner::{ChannelKey, EffectiveClaim, SharedTuner, TunerPool};",
)
# Remove EALREADY parser and replace the start/idle-eviction block up to finish_outcome.
regex_once(
    RESOLVE,
    r"/// Detect `EALREADY`.*?/// Log and unpack a successful \[`AcquireOutcome`\]\.",
    r'''/// Get-or-create the `SharedTuner` for `resolved` using the channel's
/// configured DB priority and a non-exclusive claim. Stateless dashboard HTTP
/// callers use this compatibility entry point; Mirakurun can supply its
/// request priority via [`start_tuner_for_service_with_claim`].
pub async fn start_tuner_for_service(
    tuner_pool: &Arc<TunerPool>,
    database: &DatabaseHandle,
    resolved: &ResolvedService,
) -> Result<Arc<SharedTuner>, ChannelResolveError> {
    start_tuner_for_service_with_claim(
        tuner_pool,
        database,
        resolved,
        EffectiveClaim::new(resolved.channel.priority, false),
    )
    .await
}

/// Same resolver with an explicit canonical contention claim.
///
/// Reclamation is intentionally left to `tuner::policy`: the former HTTP
/// EALREADY workaround stopped any Running+0-subscriber reader, which also
/// describes a freshly-started reader in the short window before its first
/// subscriber attaches. That race could kill another request's new tuner.
pub async fn start_tuner_for_service_with_claim(
    tuner_pool: &Arc<TunerPool>,
    database: &DatabaseHandle,
    resolved: &ResolvedService,
    claim: EffectiveClaim,
) -> Result<Arc<SharedTuner>, ChannelResolveError> {
    match try_acquire(tuner_pool, database, resolved, claim).await {
        Ok(outcome) => {
            tuner_pool.cancel_idle_close(&outcome.key).await;
            Ok(finish_outcome(outcome, resolved))
        }
        Err(e) => Err(map_acquire_error(tuner_pool, resolved, e).await),
    }
}

/// Log and unpack a successful [`AcquireOutcome`].''',
    flags=re.S,
)
replace_once(
    RESOLVE,
    """async fn try_acquire(\n    tuner_pool: &Arc<TunerPool>,\n    database: &DatabaseHandle,\n    resolved: &ResolvedService,\n) -> Result<acquire::AcquireOutcome, AcquireError> {\n""",
    """async fn try_acquire(\n    tuner_pool: &Arc<TunerPool>,\n    database: &DatabaseHandle,\n    resolved: &ResolvedService,\n    claim: EffectiveClaim,\n) -> Result<acquire::AcquireOutcome, AcquireError> {\n""",
)
replace_once(
    RESOLVE,
    """            priority: resolved.channel.priority,\n            exclusive: false,\n            client_host: \"http\".to_string(),\n""",
    """            priority: claim.priority,\n            exclusive: claim.exclusive,\n            client_host: \"http\".to_string(),\n""",
)


# ---------------------------------------------------------------------------
# Mirakurun: X-Mirakurun-Priority now participates in contention for service
# and program streams. Channel-by-type (no priority header) keeps DB default.
# ---------------------------------------------------------------------------
MIRAKURUN = "recisdb-proxy/src/web/mirakurun.rs"
replace_once(
    MIRAKURUN,
    "use recisdb_protocol::{BandType, StreamClass};",
    "use recisdb_protocol::{BandType, StreamClass};\nuse crate::tuner::EffectiveClaim;",
)
call_old = "channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &web_state.database, &resolved).await"
call_new = "channel_resolve::start_tuner_for_service_with_claim(&web_state.tuner_pool, &web_state.database, &resolved, EffectiveClaim::new(priority, false)).await"
replace_in_region(
    MIRAKURUN,
    "pub async fn stream_service_by_mirakurun_id(",
    "pub async fn stream_channel_by_type(",
    call_old,
    call_new,
)
replace_in_region(
    MIRAKURUN,
    "pub async fn stream_program_by_mirakurun_id(",
    "#[cfg(test)]",
    call_old,
    call_new,
)
text = read(MIRAKURUN)
text = text.replace(
    "is accepted and parsed but **not** fed into tuner-contention decisions\n//!   (`tuner/policy.rs::decide()`) — that requires a design decision outside\n//!   this pass's scope. See [`stream_service_by_mirakurun_id`].",
    "is parsed and propagated as the request's contention priority while\n//!   exclusivity remains a separate rank component. See\n//!   [`stream_service_by_mirakurun_id`].",
)
text = text.replace(
    "`X-Mirakurun-Priority` is parsed and logged (see\n/// [`parse_mirakurun_priority`]) but not otherwise acted on.",
    "`X-Mirakurun-Priority` is the contention priority passed to the central\n/// tuner policy (see [`parse_mirakurun_priority`]).",
)
text = text.replace(
    "`X-Mirakurun-Priority` is parsed and logged (see\n/// [`parse_mirakurun_priority`]) but not otherwise acted on, same as\n/// [`stream_service_by_mirakurun_id`].",
    "`X-Mirakurun-Priority` is propagated to tuner contention, same as\n/// [`stream_service_by_mirakurun_id`].",
)
write(MIRAKURUN, text)


# Clean warnings introduced by this branch where safe and mechanical.
TRANSPORT = "recisdb-proxy/src/node/transport.rs"
text = read(TRANSPORT)
text = text.replace("use std::convert::Infallible;\n", "")
text = text.replace("use bytes::Bytes;\n", "")
text = text.replace("use futures::{Stream, StreamExt};", "use futures::StreamExt;")
text = text.replace("let mut live = lease.subscribe_live();", "let live = lease.subscribe_live();")
write(TRANSPORT, text)

PROGRAM_STREAM = "recisdb-proxy/src/web/mirakurun_program_stream.rs"
text = read(PROGRAM_STREAM)
text = text.replace("use std::sync::Arc;\n", "")
write(PROGRAM_STREAM, text)

print("fabric autopatch applied successfully")
