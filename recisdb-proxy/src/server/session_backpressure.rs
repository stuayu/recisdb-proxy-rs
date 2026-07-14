//! Per-session TS backpressure policy.
//!
//! Kept separate from the session state machine so VIEW/PREVIEW dropping and
//! RECORD blocking semantics can be reviewed and tested independently.

use bytes::Bytes;
use recisdb_protocol::StreamClass;
use tokio::sync::mpsc;

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
pub(super) async fn send_ts_frame(
    ts_write_tx: &mpsc::Sender<Bytes>,
    frame: Bytes,
    stream_class: StreamClass,
    record_overflow_timeout: std::time::Duration,
) -> TsFrameSendOutcome {
    match stream_class {
        StreamClass::View | StreamClass::Preview => match ts_write_tx.try_send(frame) {
            Ok(()) => TsFrameSendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => TsFrameSendOutcome::DroppedFull,
            Err(mpsc::error::TrySendError::Closed(_)) => TsFrameSendOutcome::WriterClosed,
        },
        StreamClass::Record => {
            match tokio::time::timeout(record_overflow_timeout, ts_write_tx.send(frame)).await {
                Ok(Ok(())) => TsFrameSendOutcome::Sent,
                Ok(Err(_)) => TsFrameSendOutcome::WriterClosed,
                Err(_) => TsFrameSendOutcome::RecordOverflowTimeout,
            }
        }
    }
}
