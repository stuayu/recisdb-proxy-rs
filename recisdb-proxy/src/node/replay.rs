//! Bounded replay buffer for lossless RECORD path migration.
//!
//! A remote tuner lease outlives an individual HTTP/2 connection. While a
//! downstream node reconnects over another transport path, the source keeps
//! the tuner open and appends frames here. The reconnecting node resumes with
//! `from_seq=N`; if N is still retained, no TS bytes are lost.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::frame::{NodeTsFrame, NODE_TS_HEADER_LEN};

#[derive(Debug, Clone, Copy)]
pub struct ReplayBudget {
    pub max_bytes: usize,
    pub max_age: Duration,
}

impl Default for ReplayBudget {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_age: Duration::from_secs(15),
        }
    }
}

struct StoredFrame {
    inserted_at: Instant,
    frame: NodeTsFrame,
    bytes: usize,
}

pub struct ReplayBuffer {
    budget: ReplayBudget,
    generation: Option<u32>,
    entries: VecDeque<StoredFrame>,
    bytes: usize,
    next_sequence: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReplayError {
    #[error("requested generation {requested} is no longer available (current {current:?})")]
    GenerationMismatch {
        requested: u32,
        current: Option<u32>,
    },
    #[error("requested sequence {requested} has already expired; oldest retained is {oldest:?}")]
    TooOld { requested: u64, oldest: Option<u64> },
    #[error("sequence gap in source frames: expected {expected}, got {actual}")]
    SourceGap { expected: u64, actual: u64 },
}

impl ReplayBuffer {
    pub fn new(budget: ReplayBudget) -> Self {
        Self {
            budget,
            generation: None,
            entries: VecDeque::new(),
            bytes: 0,
            next_sequence: None,
        }
    }

    /// Create a replay window already associated with a source generation.
    /// This matters before the first TS packet arrives: a reconnect against a
    /// newly-created lease should yield an empty replay, not a false
    /// GenerationMismatch.
    pub fn new_for_generation(budget: ReplayBudget, generation: u32) -> Self {
        let mut this = Self::new(budget);
        this.generation = Some(generation);
        this
    }

    pub fn generation(&self) -> Option<u32> {
        self.generation
    }

    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    pub fn oldest_sequence(&self) -> Option<u64> {
        self.entries.front().map(|entry| entry.frame.sequence)
    }

    pub fn newest_sequence(&self) -> Option<u64> {
        self.entries.back().map(|entry| entry.frame.sequence)
    }

    /// Append a source frame. A generation change is a source discontinuity,
    /// so retained frames from the previous physical source are discarded.
    pub fn push(&mut self, frame: NodeTsFrame) -> Result<(), ReplayError> {
        let now = Instant::now();
        if self.generation != Some(frame.generation) {
            self.clear();
            self.generation = Some(frame.generation);
            self.next_sequence = Some(frame.sequence);
        }

        if let Some(expected) = self.next_sequence {
            if frame.sequence != expected {
                return Err(ReplayError::SourceGap {
                    expected,
                    actual: frame.sequence,
                });
            }
        }
        self.next_sequence = Some(frame.sequence.saturating_add(1));

        let bytes = NODE_TS_HEADER_LEN + frame.payload.len();
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.push_back(StoredFrame {
            inserted_at: now,
            frame,
            bytes,
        });
        self.prune(now);
        Ok(())
    }

    pub fn replay_from(
        &mut self,
        generation: u32,
        from_sequence: u64,
    ) -> Result<Vec<NodeTsFrame>, ReplayError> {
        self.prune(Instant::now());
        if self.generation != Some(generation) {
            return Err(ReplayError::GenerationMismatch {
                requested: generation,
                current: self.generation,
            });
        }
        let oldest = self.oldest_sequence();
        // With an empty window there is no oldest entry to compare against,
        // but "empty" does not mean "nothing was lost": `prune` drops every
        // entry once the source has been quiet for `max_age`. Falling back to
        // `next_sequence` (the number the *next* frame will carry) is what
        // catches that case — without it a RECORD consumer reconnecting with
        // a stale `from_seq` would be answered `Ok(vec![])`, i.e. "you missed
        // nothing", and would resume from the live edge with a silent hole.
        //
        // `next_sequence == from_sequence` is not a loss: the consumer is
        // caught up and waiting for a frame that has not been produced yet.
        // `None` means nothing was ever published on this generation, so
        // there is nothing to have missed.
        let floor = oldest.or(self.next_sequence);
        if floor.is_some_and(|seq| from_sequence < seq) {
            return Err(ReplayError::TooOld {
                requested: from_sequence,
                oldest,
            });
        }
        Ok(self
            .entries
            .iter()
            .filter(|entry| entry.frame.sequence >= from_sequence)
            .map(|entry| entry.frame.clone())
            .collect())
    }

    fn prune(&mut self, now: Instant) {
        loop {
            let evict_for_bytes = self.bytes > self.budget.max_bytes;
            let evict_for_age = self
                .entries
                .front()
                .map(|entry| now.saturating_duration_since(entry.inserted_at) > self.budget.max_age)
                .unwrap_or(false);
            if !evict_for_bytes && !evict_for_age {
                break;
            }
            if let Some(entry) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.next_sequence = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(generation: u32, sequence: u64) -> NodeTsFrame {
        NodeTsFrame {
            generation,
            sequence,
            source_monotonic_ms: sequence,
            flags: Default::default(),
            payload: Bytes::from(vec![0x47; 188]),
        }
    }

    #[test]
    fn empty_initialized_generation_can_resume_before_first_packet() {
        let mut replay = ReplayBuffer::new_for_generation(ReplayBudget::default(), 7);
        assert!(replay.replay_from(7, 0).unwrap().is_empty());
    }

    /// The window going *empty* is not the same as nothing having been lost:
    /// `prune` drops every entry once the source has been quiet for
    /// `max_age`. A RECORD consumer reconnecting with a stale `from_seq` must
    /// still be told it fell behind, or it resumes from the live edge with an
    /// unannounced hole — exactly what this module exists to prevent.
    #[test]
    fn an_emptied_window_still_reports_a_consumer_that_fell_behind() {
        let mut replay = ReplayBuffer::new(ReplayBudget {
            max_bytes: usize::MAX,
            max_age: Duration::from_millis(1),
        });
        for seq in 1..4 {
            replay.push(frame(1, seq)).unwrap();
        }
        // Everything ages out; `push` prunes, so the window is now empty.
        std::thread::sleep(Duration::from_millis(20));
        replay.push(frame(1, 4)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            replay.replay_from(1, 5).unwrap().is_empty(),
            "a caught-up consumer is fine"
        );
        assert_eq!(replay.oldest_sequence(), None, "the window really is empty");

        assert_eq!(
            replay.replay_from(1, 2).unwrap_err(),
            ReplayError::TooOld {
                requested: 2,
                oldest: None
            },
            "a stale resume point must fail, not be answered 'you missed nothing'"
        );
    }

    #[test]
    fn resume_returns_exact_missing_tail() {
        let mut replay = ReplayBuffer::new(ReplayBudget::default());
        for seq in 10..15 {
            replay.push(frame(1, seq)).unwrap();
        }
        let resumed = replay.replay_from(1, 13).unwrap();
        assert_eq!(
            resumed.iter().map(|f| f.sequence).collect::<Vec<_>>(),
            vec![13, 14]
        );
    }

    #[test]
    fn byte_budget_expires_oldest_frames() {
        let per_frame = NODE_TS_HEADER_LEN + 188;
        let mut replay = ReplayBuffer::new(ReplayBudget {
            max_bytes: per_frame * 2,
            max_age: Duration::from_secs(60),
        });
        replay.push(frame(1, 1)).unwrap();
        replay.push(frame(1, 2)).unwrap();
        replay.push(frame(1, 3)).unwrap();
        assert_eq!(replay.oldest_sequence(), Some(2));
        assert_eq!(
            replay.replay_from(1, 1).unwrap_err(),
            ReplayError::TooOld {
                requested: 1,
                oldest: Some(2)
            }
        );
    }

    #[test]
    fn generation_change_cannot_be_stitched_as_same_source() {
        let mut replay = ReplayBuffer::new(ReplayBudget::default());
        replay.push(frame(1, 1)).unwrap();
        replay.push(frame(2, 50)).unwrap();
        assert_eq!(replay.generation(), Some(2));
        assert_eq!(replay.oldest_sequence(), Some(50));
        assert!(matches!(
            replay.replay_from(1, 1),
            Err(ReplayError::GenerationMismatch { .. })
        ));
    }

    #[test]
    fn source_sequence_gap_is_fatal() {
        let mut replay = ReplayBuffer::new(ReplayBudget::default());
        replay.push(frame(1, 1)).unwrap();
        assert_eq!(
            replay.push(frame(1, 3)).unwrap_err(),
            ReplayError::SourceGap {
                expected: 2,
                actual: 3
            }
        );
    }
}
