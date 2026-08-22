//! Shared tuner handoff helpers for BNDP sessions.
//!
//! These helpers encapsulate the repetitive subscription / current-tuner
//! replacement flow used when a session reuses an already-running tuner or
//! switches to a newly selected one.

use std::sync::Arc;

use log::debug;

use crate::tuner::{EffectiveClaim, SharedTuner, TunerSubscription};

/// Backwards-compatible adapter for existing session call sites. New code
/// should compute one [`EffectiveClaim`] at request ingress and call
/// [`handoff_current_tuner_with_claim`] so the exact same claim used by the
/// acquire policy becomes the incumbent subscription claim.
pub(super) async fn handoff_current_tuner(
    session_id: u64,
    ts_receiver: &mut Option<TunerSubscription>,
    current_tuner: &mut Option<Arc<SharedTuner>>,
    next_tuner: Arc<SharedTuner>,
    claim_priority: i32,
    claim_exclusive: bool,
    is_streaming: bool,
    log_prefix: &str,
) -> Option<Arc<SharedTuner>> {
    handoff_current_tuner_with_claim(
        session_id,
        ts_receiver,
        current_tuner,
        next_tuner,
        EffectiveClaim::new(claim_priority, claim_exclusive),
        is_streaming,
        log_prefix,
    )
    .await
}

/// Replace the session's current tuner with `next_tuner` while preserving the
/// active stream subscription if needed.
///
/// `claim` must be the canonical request claim already used by tuner
/// arbitration. Recomputing priority/exclusivity here reintroduces the
/// requester/incumbent asymmetry that caused the 2026-08 tuner livelock.
///
/// Returns the previous tuner when it became subscriber-less and needs caller
/// cleanup (idle close or capacity-aware stop). Same-tuner reuse never returns
/// a cleanup target.
pub(super) async fn handoff_current_tuner_with_claim(
    session_id: u64,
    ts_receiver: &mut Option<TunerSubscription>,
    current_tuner: &mut Option<Arc<SharedTuner>>,
    next_tuner: Arc<SharedTuner>,
    claim: EffectiveClaim,
    is_streaming: bool,
    log_prefix: &str,
) -> Option<Arc<SharedTuner>> {
    let old_tuner = current_tuner.take();

    if let Some(old) = old_tuner {
        let same_tuner = Arc::ptr_eq(&old, &next_tuner);
        if same_tuner {
            debug!("[Session {}] {} reusing same tuner", session_id, log_prefix);
            if is_streaming {
                // Re-subscribe FIRST (count N→N+1), *then* let the
                // assignment below drop the old `TunerSubscription` still
                // held in `*ts_receiver` (count N+1→N). Rust evaluates the
                // right-hand side of an assignment (the subscribe call)
                // before dropping the place's previous value, so there is
                // never a transient subscriber_count==0 on an active tuner.
                let new_sub = next_tuner.subscribe_with_claim(claim.priority, claim.exclusive);
                *ts_receiver = Some(new_sub);
            }
            *current_tuner = Some(next_tuner);
            return None;
        }

        if ts_receiver.is_some() {
            *ts_receiver = None; // drops the old TunerSubscription: decrements old's count
            debug!(
                "[Session {}] {} unsubscribed from old tuner",
                session_id, log_prefix
            );
        }

        let needs_cleanup = old.subscriber_count() == 0;
        if is_streaming {
            *ts_receiver = Some(next_tuner.subscribe_with_claim(claim.priority, claim.exclusive));
        }
        *current_tuner = Some(next_tuner);

        if needs_cleanup {
            return Some(old);
        }
        return None;
    }

    if is_streaming {
        *ts_receiver = Some(next_tuner.subscribe_with_claim(claim.priority, claim.exclusive));
    }
    *current_tuner = Some(next_tuner);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuner::channel_key::ChannelKey;

    /// Same-tuner reuse while streaming must never let `subscriber_count`
    /// touch 0 partway through, and must end up back at exactly 1 (the
    /// session's own subscription) — verifying the RAII handoff preserves
    /// the pre-existing "subscribe new before dropping old" ordering that
    /// used to be spelled out with an explicit manual `unsubscribe()` call.
    #[tokio::test]
    async fn handoff_same_tuner_reuse_keeps_subscriber_count_stable() {
        let tuner = SharedTuner::new(ChannelKey::simple("/dev/test", 1), 2);
        let mut ts_receiver = Some(tuner.subscribe_with_claim(42, true));
        let mut current_tuner = Some(Arc::clone(&tuner));
        assert_eq!(tuner.subscriber_count(), 1);

        let cleanup = handoff_current_tuner_with_claim(
            1,
            &mut ts_receiver,
            &mut current_tuner,
            Arc::clone(&tuner),
            EffectiveClaim::new(42, true),
            true,
            "test:",
        )
        .await;

        assert!(
            cleanup.is_none(),
            "same-tuner reuse never returns a cleanup target"
        );
        assert_eq!(
            tuner.subscriber_count(),
            1,
            "count must settle back at 1, not leak or drop to 0"
        );
        assert!(ts_receiver.is_some());
        assert_eq!(
            tuner.incumbent_claim(),
            Some(crate::tuner::shared::Claim {
                priority: 42,
                exclusive: true
            })
        );
    }

    /// The exact claim passed to arbitration must survive handoff unchanged.
    #[tokio::test]
    async fn canonical_claim_survives_handoff_without_reinterpretation() {
        let tuner = SharedTuner::new(ChannelKey::simple("/dev/test", 1), 2);
        let mut ts_receiver = None;
        let mut current_tuner = None;
        let claim = EffectiveClaim::new(3, true);

        handoff_current_tuner_with_claim(
            7,
            &mut ts_receiver,
            &mut current_tuner,
            Arc::clone(&tuner),
            claim,
            true,
            "claim-test:",
        )
        .await;

        let incumbent = tuner.incumbent_claim().expect("subscription claim");
        assert_eq!(incumbent.priority, claim.priority);
        assert_eq!(incumbent.exclusive, claim.exclusive);
    }

    /// Switching to a different tuner while streaming: the old tuner's
    /// subscription is released and, when it was the sole subscriber, is
    /// returned to the caller as a cleanup target.
    #[tokio::test]
    async fn handoff_different_tuner_releases_old_and_reports_cleanup_when_sole_subscriber() {
        let old_tuner = SharedTuner::new(ChannelKey::simple("/dev/old", 1), 2);
        let new_tuner = SharedTuner::new(ChannelKey::simple("/dev/new", 1), 2);

        let mut ts_receiver = Some(old_tuner.subscribe());
        let mut current_tuner = Some(Arc::clone(&old_tuner));
        assert_eq!(old_tuner.subscriber_count(), 1);

        let cleanup = handoff_current_tuner(
            1,
            &mut ts_receiver,
            &mut current_tuner,
            Arc::clone(&new_tuner),
            0,
            false,
            true,
            "test:",
        )
        .await;

        assert!(
            cleanup.is_some() && Arc::ptr_eq(cleanup.as_ref().unwrap(), &old_tuner),
            "old tuner became subscriber-less and must be returned for caller cleanup"
        );
        assert_eq!(old_tuner.subscriber_count(), 0);
        assert_eq!(new_tuner.subscriber_count(), 1);
        assert!(Arc::ptr_eq(current_tuner.as_ref().unwrap(), &new_tuner));
    }

    /// Switching tuners when another subscriber still holds the old one: no
    /// cleanup target is reported (the old tuner is still in active use).
    #[tokio::test]
    async fn handoff_different_tuner_no_cleanup_when_old_has_other_subscribers() {
        let old_tuner = SharedTuner::new(ChannelKey::simple("/dev/old", 1), 2);
        let new_tuner = SharedTuner::new(ChannelKey::simple("/dev/new", 1), 2);

        let _other_sub = old_tuner.subscribe(); // another session's subscription
        let mut ts_receiver = Some(old_tuner.subscribe());
        let mut current_tuner = Some(Arc::clone(&old_tuner));
        assert_eq!(old_tuner.subscriber_count(), 2);

        let cleanup = handoff_current_tuner(
            1,
            &mut ts_receiver,
            &mut current_tuner,
            Arc::clone(&new_tuner),
            0,
            false,
            true,
            "test:",
        )
        .await;

        assert!(cleanup.is_none());
        assert_eq!(
            old_tuner.subscriber_count(),
            1,
            "only this session's own subscription is released"
        );
    }
}
