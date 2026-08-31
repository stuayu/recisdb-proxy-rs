//! Byte-budgeted TS write queue (STREAMING_DESIGN.md §3.2).
//!
//! The per-session TS write queue used to be bounded by a **frame count**
//! (256 slots). A frame is one broadcast chunk, and a chunk is however much
//! the reader happened to pull from the driver in one go — up to 256 KB from a
//! local tuner, but typically 64 KB when the upstream is another proxy. The
//! same setting therefore bought roughly 8 seconds of slack in one deployment
//! and 32 in another, which is useless for sizing a link between sites.
//!
//! So the queue is bounded by **bytes**, and the byte budget is derived from
//! the stream's bitrate and a per-class duration:
//!
//! ```text
//! budget_bytes = bitrate_bps / 8 * queue_ms / 1000
//! ```
//!
//! The mpsc channel keeps a generous slot count purely as a transport-level
//! backstop; the budget is what actually decides when the queue is "full", and
//! it can be re-sized at runtime as the measured bitrate settles or the class
//! changes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, Notify};

/// Transport-level slot cap for the TS channel.
///
/// Not the real limit — [`TsQueueShared::budget_bytes`] is. This only stops an
/// unbounded number of very small frames from queueing up if the byte budget is
/// large.
pub const TS_WRITE_SLOTS: usize = 8192;

/// Bytes to fall back on before a bitrate is known (≈18 Mbps × 8 s).
pub const DEFAULT_BUDGET_BYTES: usize = 18_000_000 / 8 * 8;

/// Convert a bitrate and a duration into a byte budget.
///
/// Returns [`DEFAULT_BUDGET_BYTES`] when either input is zero, so a
/// misconfigured value degrades to the previous behaviour rather than to a
/// zero-capacity queue.
pub fn budget_bytes(bitrate_bps: u64, queue_ms: u64) -> usize {
    if bitrate_bps == 0 || queue_ms == 0 {
        return DEFAULT_BUDGET_BYTES;
    }
    let bytes = (bitrate_bps as f64 / 8.0) * (queue_ms as f64 / 1000.0);
    if !bytes.is_finite() || bytes <= 0.0 {
        DEFAULT_BUDGET_BYTES
    } else {
        bytes.round() as usize
    }
}

/// Accounting shared between the session (producer) and the writer task
/// (consumer).
pub struct TsQueueShared {
    /// Bytes handed to the channel but not yet written to the socket.
    queued_bytes: AtomicUsize,
    /// Current ceiling for `queued_bytes`.
    budget_bytes: AtomicUsize,
    /// Woken whenever bytes leave the queue or the budget grows, so a RECORD
    /// sender blocked on the budget resumes immediately instead of polling.
    drained: Notify,
}

impl TsQueueShared {
    fn new(budget: usize) -> Self {
        Self {
            queued_bytes: AtomicUsize::new(0),
            budget_bytes: AtomicUsize::new(budget.max(1)),
            drained: Notify::new(),
        }
    }

    /// Reserve room for `len` bytes, or report that the queue is full.
    ///
    /// A frame is always accepted into an empty queue even if it exceeds the
    /// budget: refusing it would stall the stream permanently whenever a single
    /// chunk is larger than the configured window.
    pub fn try_reserve(&self, len: usize) -> bool {
        let budget = self.budget_bytes.load(Ordering::Acquire);
        let mut queued = self.queued_bytes.load(Ordering::Acquire);
        loop {
            if queued != 0 && queued + len > budget {
                return false;
            }
            match self.queued_bytes.compare_exchange_weak(
                queued,
                queued + len,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => queued = actual,
            }
        }
    }

    /// Give back a reservation (frame written, or handing it off failed).
    pub fn release(&self, len: usize) {
        // `fetch_update` rather than `fetch_sub` so an accounting slip can
        // never wrap the counter around to a huge value and wedge the queue.
        let _ = self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                Some(queued.saturating_sub(len))
            });
        self.drained.notify_waiters();
    }

    /// Re-size the queue (channel switch, class change, bitrate settled).
    pub fn set_budget(&self, bytes: usize) {
        self.budget_bytes.store(bytes.max(1), Ordering::Release);
        // A larger budget may unblock a waiting RECORD sender.
        self.drained.notify_waiters();
    }

    pub fn budget(&self) -> usize {
        self.budget_bytes.load(Ordering::Acquire)
    }

    pub fn queued(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }

    /// Wait until bytes leave the queue, the budget grows, or `timeout` passes.
    pub async fn wait_drained(&self, timeout: Duration) {
        let notified = self.drained.notified();
        tokio::pin!(notified);
        // Register interest *before* the caller re-checks the budget, so a
        // release happening in between is not lost.
        notified.as_mut().enable();
        let _ = tokio::time::timeout(timeout, notified).await;
    }
}

/// Producer handle: an mpsc sender plus the byte accounting.
#[derive(Clone)]
pub struct TsWriteQueue {
    tx: mpsc::Sender<Bytes>,
    shared: Arc<TsQueueShared>,
}

impl TsWriteQueue {
    /// Create the queue, returning the producer handle, the receiver for the
    /// writer task, and the shared accounting the writer needs to release
    /// bytes as it drains.
    pub fn new(initial_budget_bytes: usize) -> (Self, mpsc::Receiver<Bytes>, Arc<TsQueueShared>) {
        let (tx, rx) = mpsc::channel::<Bytes>(TS_WRITE_SLOTS);
        let shared = Arc::new(TsQueueShared::new(initial_budget_bytes));
        (
            Self {
                tx,
                shared: Arc::clone(&shared),
            },
            rx,
            shared,
        )
    }

    pub fn shared(&self) -> &Arc<TsQueueShared> {
        &self.shared
    }

    pub fn try_reserve(&self, len: usize) -> bool {
        self.shared.try_reserve(len)
    }

    /// Hand a reserved frame to the writer. Releases the reservation if the
    /// hand-off fails.
    pub fn send_reserved(&self, frame: Bytes) -> Result<(), TsSendError> {
        let len = frame.len();
        match self.tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.shared.release(len);
                Err(TsSendError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.shared.release(len);
                Err(TsSendError::Closed)
            }
        }
    }

    /// Same as [`Self::send_reserved`] but waits for a transport slot. Only the
    /// RECORD path uses this; the byte budget has already been satisfied, so
    /// this only ever waits on the slot backstop.
    pub async fn send_reserved_blocking(&self, frame: Bytes) -> Result<(), TsSendError> {
        let len = frame.len();
        match self.tx.send(frame).await {
            Ok(()) => Ok(()),
            Err(_) => {
                self.shared.release(len);
                Err(TsSendError::Closed)
            }
        }
    }

    #[cfg(test)]
    pub fn capacity_slots(&self) -> usize {
        self.tx.capacity()
    }
}

/// Why a hand-off to the writer task failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsSendError {
    /// Transport slots exhausted.
    Full,
    /// Writer task is gone.
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_translates_bitrate_and_duration_to_bytes() {
        // 16 Mbps for 8 s = 16 MB.
        assert_eq!(budget_bytes(16_000_000, 8_000), 16_000_000);
        // Zero inputs fall back rather than producing a dead queue.
        assert_eq!(budget_bytes(0, 8_000), DEFAULT_BUDGET_BYTES);
        assert_eq!(budget_bytes(16_000_000, 0), DEFAULT_BUDGET_BYTES);
    }

    #[test]
    fn reservations_are_bounded_by_bytes_not_frame_count() {
        let shared = TsQueueShared::new(1000);

        assert!(shared.try_reserve(600));
        assert!(shared.try_reserve(400));
        assert_eq!(shared.queued(), 1000);

        // Budget exhausted — a frame count limit would still have 8190 slots.
        assert!(!shared.try_reserve(1));

        shared.release(400);
        assert_eq!(shared.queued(), 600);
        assert!(shared.try_reserve(400));
    }

    #[test]
    fn an_oversized_frame_still_passes_through_an_empty_queue() {
        // Otherwise a chunk larger than the configured window would wedge the
        // stream forever.
        let shared = TsQueueShared::new(1000);
        assert!(
            shared.try_reserve(5000),
            "empty queue must accept any frame"
        );
        assert!(!shared.try_reserve(1), "but it is now over budget");
        shared.release(5000);
        assert_eq!(shared.queued(), 0);
    }

    #[test]
    fn release_never_wraps_the_counter() {
        let shared = TsQueueShared::new(1000);
        shared.try_reserve(100);
        shared.release(500); // more than was reserved
        assert_eq!(shared.queued(), 0);
        assert!(shared.try_reserve(1000), "queue must still be usable");
    }

    #[test]
    fn growing_the_budget_admits_more_bytes() {
        let shared = TsQueueShared::new(1000);
        assert!(shared.try_reserve(1000));
        assert!(!shared.try_reserve(500));

        shared.set_budget(2000);
        assert!(
            shared.try_reserve(500),
            "resize must take effect immediately"
        );
    }

    #[tokio::test]
    async fn wait_drained_wakes_on_release() {
        let shared = Arc::new(TsQueueShared::new(1000));
        assert!(shared.try_reserve(1000));

        let waiter = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            waiter.wait_drained(Duration::from_secs(5)).await;
            waiter.try_reserve(500)
        });

        // Give the waiter a moment to park, then free room.
        tokio::task::yield_now().await;
        shared.release(1000);

        assert!(task.await.unwrap(), "waiter must resume once room appears");
    }

    #[tokio::test]
    async fn wait_drained_gives_up_after_the_timeout() {
        let shared = TsQueueShared::new(1000);
        assert!(shared.try_reserve(1000));
        let start = std::time::Instant::now();
        shared.wait_drained(Duration::from_millis(50)).await;
        assert!(start.elapsed() >= Duration::from_millis(50));
    }
}
