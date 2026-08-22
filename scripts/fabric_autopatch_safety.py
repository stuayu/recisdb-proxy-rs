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
    """fn max_attempts(candidates: usize) -> usize {\n    candidates + 2\n}\n\n/// Build a [`TunerSnapshot`] of `dll_paths`' driver rows plus every pool\n""",
    """fn max_attempts(candidates: usize) -> usize {\n    candidates + 2\n}\n\n/// RAII carrier for a slot permit detached from the caller's current tuner.\n/// If acquire exits with an error before consuming/returning it, Drop puts the\n/// permit back on the still-active owner. This closes the failure path where\n/// a channel switch left a live BonDriver open while its semaphore permit had\n/// already been dropped.\nstruct CarriedPermitGuard {\n    permit: Option<SlotPermit>,\n    owner: Option<Arc<SharedTuner>>,\n}\n\nimpl CarriedPermitGuard {\n    fn new(permit: Option<SlotPermit>, owner: Option<Arc<SharedTuner>>) -> Self {\n        Self { permit, owner }\n    }\n\n    fn is_on_path(&self, path: &str) -> bool {\n        self.permit.as_ref().is_some_and(|p| p.dll_path() == path)\n    }\n\n    /// Successful acquire hands an unused permit back to the caller; disable\n    /// automatic restoration in that case because the caller owns it again.\n    fn take_unused(&mut self) -> Option<SlotPermit> {\n        self.owner = None;\n        self.permit.take()\n    }\n}\n\nimpl CarriedSlotPermit for CarriedPermitGuard {\n    fn take_if_on_path(&mut self, dll_path: &str) -> Option<SlotPermit> {\n        if self.is_on_path(dll_path) {\n            self.permit.take()\n        } else {\n            None\n        }\n    }\n}\n\nimpl Drop for CarriedPermitGuard {\n    fn drop(&mut self) {\n        let Some(permit) = self.permit.take() else { return; };\n        if let Some(owner) = self.owner.as_ref() {\n            if owner.occupies_slot() && owner.key.tuner_path == permit.dll_path() {\n                owner.set_slot_permit(permit);\n                return;\n            }\n        }\n        // No live owner remains; dropping the permit is the correct release.\n        drop(permit);\n    }\n}\n\n/// Build a [`TunerSnapshot`] of `dll_paths`' driver rows plus every pool\n""",
)
replace_once(
    ACQUIRE,
    """async fn take_permit_for_path(\n    dll_path: &str,\n    carried_permit: &mut Option<SlotPermit>,\n    warm: &mut Option<WarmTunerHandle>,\n) -> Option<(SlotPermit, Option<WarmTunerHandle>)> {\n""",
    """async fn take_permit_for_path<P: CarriedSlotPermit>(\n    dll_path: &str,\n    carried_permit: &mut P,\n    warm: &mut Option<WarmTunerHandle>,\n) -> Option<(SlotPermit, Option<WarmTunerHandle>)> {\n""",
)
replace_once(
    ACQUIRE,
    """    let mut carried_permit = request.carried_permit;\n    let mut warm = request.warm;\n""",
    """    let carried_owner = if request.carried_permit.is_some() {\n        match request.own_key.as_ref() {\n            Some(key) => pool.get(key).await,\n            None => None,\n        }\n    } else {\n        None\n    };\n    let mut carried_permit = CarriedPermitGuard::new(request.carried_permit, carried_owner);\n    let mut warm = request.warm;\n""",
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
    """                let (permit, warm_to_use) =\n                    match take_permit_for_path(&key.tuner_path, &mut carried_permit, &mut warm).await {\n""",
    """                let transferring_own_slot = carried_permit.is_on_path(&key.tuner_path)\n                    && request.own_key.as_ref().is_some_and(|own| {\n                        own != &key && own.tuner_path == key.tuner_path\n                    });\n                if transferring_own_slot {\n                    if let Some(own_key) = request.own_key.as_ref() {\n                        if let Some(old) = pool.get(own_key).await {\n                            info!(\n                                \"[acquire] stopping caller's old reader {:?} before same-DLL slot transfer to {:?}\",\n                                own_key, key\n                            );\n                            pool.cancel_idle_close(own_key).await;\n                            old.set_stop_reason(StopReason::Released);\n                            old.stop_reader().await;\n                            pool.remove(own_key).await;\n                        }\n                    }\n                }\n\n                let (permit, warm_to_use) =\n                    match take_permit_for_path(&key.tuner_path, &mut carried_permit, &mut warm).await {\n""",
)

# The new one-shot SelectLogicalChannel acquire may stop an old same-DLL
# reader when transferring its slot. On failure, use the existing restore path
# rather than leaving the session's current_tuner pointing at a stopped reader.
SESSION = "recisdb-proxy/src/server/session.rs"
replace_once(
    SESSION,
    """                if let Some(old) = old_tuner_for_permit.as_ref() {\n                    // acquire returns an unused carried permit only on success;\n                    // on failure it is dropped by the executor. The old reader\n                    // still owns its physical slot, so if it remains active its\n                    // permit must already be retained there (same invariant as\n                    // SetChannelSpace's pre-switch handoff path).\n                    debug!(\n                        \"[Session {}] SelectLogicalChannel: acquire failed while old tuner {:?} remains: {}\",\n                        self.id, old.key, e\n                    );\n                } else {\n                    debug!(\"[Session {}] SelectLogicalChannel: acquire failed: {}\", self.id, e);\n                }\n                return self.fail_logical_channel_selection().await;\n""",
    """                if let Some(old) = old_tuner_for_permit.as_ref() {\n                    debug!(\n                        \"[Session {}] SelectLogicalChannel: acquire failed with previous tuner {:?}: {}\",\n                        self.id, old.key, e\n                    );\n                } else {\n                    debug!(\"[Session {}] SelectLogicalChannel: acquire failed: {}\", self.id, e);\n                }\n                self.try_restore_previous_channel(&old_tuner_key).await;\n                return self.fail_logical_channel_selection().await;\n""",
)

print("carried-slot safety patch applied successfully")
