#!/usr/bin/env python3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def p(path):
    return ROOT / path


def replace_once(path, old, new):
    text = p(path).read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, got {count}: {old[:120]!r}")
    p(path).write_text(text.replace(old, new, 1), encoding="utf-8")


ACQUIRE = "recisdb-proxy/src/tuner/acquire.rs"

# A detached same-DLL slot permit must not silently vanish when acquire()
# rejects before consuming it. Keep the old tuner as a synchronous Drop-time
# return target so every error path is covered, including newly-added ones.
replace_once(
    ACQUIRE,
    """fn max_attempts(candidates: usize) -> usize {
    candidates + 2
}

/// Build a [`TunerSnapshot`] of `dll_paths`' driver rows plus every pool
""",
    """fn max_attempts(candidates: usize) -> usize {
    candidates + 2
}

/// RAII carrier for a slot permit detached from the caller's current tuner.
/// If acquire exits with an error before consuming/returning it, Drop puts the
/// permit back on the still-active owner. This closes the failure path where
/// a channel switch left a live BonDriver open while its semaphore permit had
/// already been dropped.
struct CarriedPermitGuard {
    permit: Option<SlotPermit>,
    owner: Option<Arc<SharedTuner>>,
}

impl CarriedPermitGuard {
    fn new(permit: Option<SlotPermit>, owner: Option<Arc<SharedTuner>>) -> Self {
        Self { permit, owner }
    }

    fn is_on_path(&self, path: &str) -> bool {
        self.permit.as_ref().is_some_and(|p| p.dll_path() == path)
    }

    /// Successful acquire hands an unused permit back to the caller; disable
    /// automatic restoration in that case because the caller owns it again.
    fn take_unused(&mut self) -> Option<SlotPermit> {
        self.owner = None;
        self.permit.take()
    }
}

impl CarriedSlotPermit for CarriedPermitGuard {
    fn take_if_on_path(&mut self, dll_path: &str) -> Option<SlotPermit> {
        if self.is_on_path(dll_path) {
            self.permit.take()
        } else {
            None
        }
    }
}

impl Drop for CarriedPermitGuard {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else { return; };
        if let Some(owner) = self.owner.as_ref() {
            if owner.occupies_slot() && owner.key.tuner_path == permit.dll_path() {
                owner.set_slot_permit(permit);
                return;
            }
        }
        // No live owner remains; dropping the permit is the correct release.
        drop(permit);
    }
}

/// Build a [`TunerSnapshot`] of `dll_paths`' driver rows plus every pool
""",
)
replace_once(
    ACQUIRE,
    """async fn take_permit_for_path(
    dll_path: &str,
    carried_permit: &mut Option<SlotPermit>,
    warm: &mut Option<WarmTunerHandle>,
) -> Option<(SlotPermit, Option<WarmTunerHandle>)> {
""",
    """async fn take_permit_for_path<P: CarriedSlotPermit>(
    dll_path: &str,
    carried_permit: &mut P,
    warm: &mut Option<WarmTunerHandle>,
) -> Option<(SlotPermit, Option<WarmTunerHandle>)> {
""",
)
replace_once(
    ACQUIRE,
    """    let mut carried_permit = request.carried_permit;
    let mut warm = request.warm;
""",
    """    let carried_owner = if request.carried_permit.is_some() {
        match request.own_key.as_ref() {
            Some(key) => pool.get(key).await,
            None => None,
        }
    } else {
        None
    };
    let mut carried_permit = CarriedPermitGuard::new(request.carried_permit, carried_owner);
    let mut warm = request.warm;
""",
)
# Both success paths return the unused resource explicitly.
text = p(ACQUIRE).read_text(encoding="utf-8")
old = "unused_permit: carried_permit,"
count = text.count(old)
if count != 2:
    raise RuntimeError(f"{ACQUIRE}: expected two AcquireOutcome unused_permit sites, got {count}")
text = text.replace(old, "unused_permit: carried_permit.take_unused(),")
p(ACQUIRE).write_text(text, encoding="utf-8")

# When the winning Create reuses the caller's own same-DLL slot, stop the old
# reader before opening the replacement. One permit may never back two live
# native readers at once. Different-path winners leave the old reader intact.
replace_once(
    ACQUIRE,
    """                let (permit, warm_to_use) =
                    match take_permit_for_path(&key.tuner_path, &mut carried_permit, &mut warm).await {
""",
    """                let transferring_own_slot = carried_permit.is_on_path(&key.tuner_path)
                    && request.own_key.as_ref().is_some_and(|own| {
                        own != &key && own.tuner_path == key.tuner_path
                    });
                if transferring_own_slot {
                    if let Some(own_key) = request.own_key.as_ref() {
                        if let Some(old) = pool.get(own_key).await {
                            info!(
                                "[acquire] stopping caller's old reader {:?} before same-DLL slot transfer to {:?}",
                                own_key, key
                            );
                            pool.cancel_idle_close(own_key).await;
                            old.set_stop_reason(StopReason::Released);
                            old.stop_reader().await;
                            pool.remove(own_key).await;
                        }
                    }
                }

                let (permit, warm_to_use) =
                    match take_permit_for_path(&key.tuner_path, &mut carried_permit, &mut warm).await {
""",
)

# The new one-shot SelectLogicalChannel acquire may stop an old same-DLL
# reader when transferring its slot. On failure, use the existing restore path
# rather than leaving the session's current_tuner pointing at a stopped reader.
SESSION = "recisdb-proxy/src/server/session.rs"
replace_once(
    SESSION,
    """            own_key: old_tuner_key,
            own_key_will_free_slot,
""",
    """            own_key: old_tuner_key.clone(),
            own_key_will_free_slot,
""",
)
replace_once(
    SESSION,
    """                if let Some(old) = old_tuner_for_permit.as_ref() {
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
""",
    """                if let Some(old) = old_tuner_for_permit.as_ref() {
                    debug!(
                        "[Session {}] SelectLogicalChannel: acquire failed with previous tuner {:?}: {}",
                        self.id, old.key, e
                    );
                } else {
                    debug!("[Session {}] SelectLogicalChannel: acquire failed: {}", self.id, e);
                }
                self.try_restore_previous_channel(&old_tuner_key).await;
                return self.fail_logical_channel_selection().await;
""",
)

print("carried-slot safety patch applied successfully")
