//! Per-session TS backpressure policy.
//!
//! Kept separate from the session state machine so VIEW/PREVIEW dropping and
//! RECORD blocking semantics can be reviewed and tested independently.
//!
//! The queue this operates on is bounded by **bytes**, not by frame count —
//! see `ts_queue.rs` for why. That makes the amount of slack a function of the
//! stream bitrate and a configured duration, which is what matters when the
//! two ends sit in different cities.

use std::time::Instant;

use bytes::Bytes;
use recisdb_protocol::StreamClass;

use crate::server::ts_queue::{TsSendError, TsWriteQueue};

/// Effective channel priority at or above this value promotes a session to
/// the loss-intolerant RECORD class.
pub(super) const RECORD_PRIORITY_THRESHOLD: i32 = 200;

/// Maximum time a RECORD stream may wait for its client write queue.
pub(super) const RECORD_OVERFLOW_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

pub(super) fn should_auto_promote_to_record(effective_priority: i32) -> bool {
    effective_priority >= RECORD_PRIORITY_THRESHOLD
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TsFrameSendOutcome {
    Sent,
    DroppedFull,
    RecordOverflowTimeout,
    WriterClosed,
}

/// Enqueue a TS frame according to the class-specific backpressure policy.
///
/// * VIEW / PREVIEW — never block. If the frame does not fit in the byte
///   budget it is dropped, and the gap is accounted for by the caller.
/// * RECORD — never drop. Wait for the queue to drain, bounded by
///   `record_overflow_timeout`; on expiry report the overflow so the caller can
///   disconnect. Truncating a recording is recoverable, silently perforating
///   one is not.
pub(super) async fn send_ts_frame(
    queue: &TsWriteQueue,
    frame: Bytes,
    stream_class: StreamClass,
    record_overflow_timeout: std::time::Duration,
) -> TsFrameSendOutcome {
    let len = frame.len();

    match stream_class {
        StreamClass::View | StreamClass::Preview => {
            if !queue.try_reserve(len) {
                return TsFrameSendOutcome::DroppedFull;
            }
            match queue.send_reserved(frame) {
                Ok(()) => TsFrameSendOutcome::Sent,
                Err(TsSendError::Full) => TsFrameSendOutcome::DroppedFull,
                Err(TsSendError::Closed) => TsFrameSendOutcome::WriterClosed,
            }
        }
        StreamClass::Record => {
            let deadline = Instant::now() + record_overflow_timeout;
            loop {
                if queue.try_reserve(len) {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    return TsFrameSendOutcome::RecordOverflowTimeout;
                }
                // Wake as soon as the writer frees room; the timeout is only a
                // ceiling, not a poll interval.
                queue.shared().wait_drained(deadline - now).await;
            }

            match tokio::time::timeout(
                record_overflow_timeout,
                queue.send_reserved_blocking(frame),
            )
            .await
            {
                Ok(Ok(())) => TsFrameSendOutcome::Sent,
                Ok(Err(_)) => TsFrameSendOutcome::WriterClosed,
                Err(_) => TsFrameSendOutcome::RecordOverflowTimeout,
            }
        }
    }
}
