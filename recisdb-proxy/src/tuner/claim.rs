//! Canonical tuner arbitration claim.
//!
//! A request claim must be computed exactly once and the same value must be
//! carried through policy evaluation, the live subscription ledger and any
//! remote-node hop.  In particular, `exclusive` is an independent tie-breaker;
//! it must never be rewritten into `i32::MAX` priority because doing so makes
//! requester and incumbent rankings diverge after hand-off.

use serde::{Deserialize, Serialize};

/// Effective priority/exclusivity used by every arbitration layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct EffectiveClaim {
    pub priority: i32,
    pub exclusive: bool,
}

impl EffectiveClaim {
    pub const fn new(priority: i32, exclusive: bool) -> Self {
        Self { priority, exclusive }
    }

    /// Resolve client controls against the channel default once.
    ///
    /// Positive client priority wins.  Zero/negative means "use the channel
    /// default" for compatibility with the existing BNDP/Mirakurun behaviour.
    /// `exclusive` remains a separate rank component.
    pub const fn resolve(client_priority: i32, exclusive: bool, channel_default: i32) -> Self {
        Self {
            priority: if client_priority > 0 { client_priority } else { channel_default },
            exclusive,
        }
    }

    /// Strict total order used for eviction decisions.
    #[inline]
    pub const fn rank(self) -> (i32, u8) {
        (self.priority, self.exclusive as u8)
    }

    #[inline]
    pub const fn strictly_outranks(self, incumbent: Self) -> bool {
        let a = self.rank();
        let b = incumbent.rank();
        a.0 > b.0 || (a.0 == b.0 && a.1 > b.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_is_tie_breaker_not_max_priority() {
        let low_exclusive = EffectiveClaim::new(0, true);
        let recording = EffectiveClaim::new(2, false);
        assert!(!low_exclusive.strictly_outranks(recording));
    }

    #[test]
    fn exclusive_wins_only_on_equal_priority() {
        let normal = EffectiveClaim::new(2, false);
        let exclusive = EffectiveClaim::new(2, true);
        assert!(exclusive.strictly_outranks(normal));
        assert!(!normal.strictly_outranks(exclusive));
    }

    #[test]
    fn resolve_keeps_exclusive_separate() {
        assert_eq!(
            EffectiveClaim::resolve(0, true, 3),
            EffectiveClaim::new(3, true)
        );
        assert_eq!(
            EffectiveClaim::resolve(4, false, 3),
            EffectiveClaim::new(4, false)
        );
    }
}
