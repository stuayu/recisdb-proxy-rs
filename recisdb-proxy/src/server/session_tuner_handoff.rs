//! Shared tuner handoff helpers for BNDP sessions.
//!
//! These helpers encapsulate the repetitive subscription / current-tuner
//! replacement flow used when a session reuses an already-running tuner or
//! switches to a newly selected one.

use std::sync::Arc;

use bytes::Bytes;
use log::debug;
use tokio::sync::broadcast;

use crate::tuner::SharedTuner;

/// Replace the session's current tuner with `next_tuner` while preserving the
/// active stream subscription if needed.
///
/// Returns the previous tuner when it became subscriber-less and needs caller
/// cleanup (idle close or capacity-aware stop). Same-tuner reuse never returns
/// a cleanup target.
pub(super) async fn handoff_current_tuner(
    session_id: u64,
    ts_receiver: &mut Option<broadcast::Receiver<Bytes>>,
    current_tuner: &mut Option<Arc<SharedTuner>>,
    next_tuner: Arc<SharedTuner>,
    is_streaming: bool,
    log_prefix: &str,
) -> Option<Arc<SharedTuner>> {
    let old_tuner = current_tuner.take();

    if let Some(old) = old_tuner {
        let same_tuner = Arc::ptr_eq(&old, &next_tuner);
        if same_tuner {
            debug!("[Session {}] {} reusing same tuner", session_id, log_prefix);
            if is_streaming {
                // Re-subscribe FIRST (count N→N+1), then unsubscribe old (count N+1→N).
                // This avoids a transient subscriber_count==0 on a still-active tuner.
                let new_rx = next_tuner.subscribe();
                *ts_receiver = Some(new_rx);
                old.unsubscribe();
            }
            *current_tuner = Some(next_tuner);
            return None;
        }

        if ts_receiver.is_some() {
            old.unsubscribe();
            *ts_receiver = None;
            debug!("[Session {}] {} unsubscribed from old tuner", session_id, log_prefix);
        }

        let needs_cleanup = old.subscriber_count() == 0;
        if is_streaming {
            *ts_receiver = Some(next_tuner.subscribe());
        }
        *current_tuner = Some(next_tuner);

        if needs_cleanup {
            return Some(old);
        }
        return None;
    }

    if is_streaming {
        *ts_receiver = Some(next_tuner.subscribe());
    }
    *current_tuner = Some(next_tuner);
    None
}
