//! Reason-tagged reference counting.
//!
//! Each reference carries a "reason" tag so we can debug leaks and verify
//! correctness. The critical invariant: only the 1→0 transition requires
//! an exclusive lock (with re-check), everything else uses shared access.
//!
//! Pattern from Bitbucket object.c / FSDK.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Why a reference is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(usize)]
pub enum RefReason {
    GraphEdge = 0,
    TransactionHold = 1,
    CacheEntry = 2,
    OpenFileDesc = 3,
    TraversalCursor = 4,
    LogReference = 5,
    SnapshotPin = 6,
    InternalTemp = 7,
}

impl RefReason {
    pub const COUNT: usize = 8;
}

/// A reason-tagged reference count.
///
/// The RwLock protects only the 1→0 transition. All other inc/dec
/// operations use atomic operations under a shared (read) lock.
pub struct RefCount {
    /// Per-reason counts — always modified atomically.
    reasons: [AtomicU32; RefReason::COUNT],
    /// Lock for the 1→0 transition.
    /// Read lock: normal inc/dec.  Write lock: 1→0 check.
    transition_lock: RwLock<()>,
}

/// Result of a decrement operation.
#[derive(Debug, PartialEq, Eq)]
pub enum DecResult {
    /// Reference count decreased but still > 0.
    StillReferenced,
    /// Reference count reached zero — caller should clean up.
    Unreferenced,
}

impl RefCount {
    pub fn new() -> Self {
        Self {
            reasons: std::array::from_fn(|_| AtomicU32::new(0)),
            transition_lock: RwLock::new(()),
        }
    }

    /// Total reference count across all reasons.
    pub fn total(&self) -> u32 {
        self.reasons.iter().map(|r| r.load(Ordering::Acquire)).sum()
    }

    /// Count for a specific reason.
    pub fn count_for(&self, reason: RefReason) -> u32 {
        self.reasons[reason as usize].load(Ordering::Acquire)
    }

    /// Increment the reference count for the given reason.
    ///
    /// Takes a shared lock — concurrent increments are fine.
    pub fn inc(&self, reason: RefReason) {
        let _guard = self.transition_lock.read();
        self.reasons[reason as usize].fetch_add(1, Ordering::AcqRel);
    }

    /// Decrement the reference count for the given reason.
    ///
    /// If the total might reach zero, takes an exclusive lock and re-checks.
    /// Returns whether the object is now unreferenced.
    pub fn dec(&self, reason: RefReason) -> DecResult {
        // Fast path: shared lock, atomically decrement.
        {
            let _guard = self.transition_lock.read();
            let prev = self.reasons[reason as usize].fetch_sub(1, Ordering::AcqRel);
            assert!(prev > 0, "decrement below zero for reason {:?}", reason);

            // If prev > 1 for this reason, or total is clearly > 1, we're fine.
            if prev > 1 {
                return DecResult::StillReferenced;
            }
            // prev == 1, so this reason just went to 0.
            // Need to check if total is now 0.
            let total: u32 = self.reasons.iter().map(|r| r.load(Ordering::Acquire)).sum();
            if total > 0 {
                return DecResult::StillReferenced;
            }
        }
        // Slow path: total might be 0. Take exclusive lock and re-check.
        let _guard = self.transition_lock.write();
        let total: u32 = self.reasons.iter().map(|r| r.load(Ordering::Acquire)).sum();
        if total == 0 {
            DecResult::Unreferenced
        } else {
            // Someone incremented between our check and acquiring the write lock.
            DecResult::StillReferenced
        }
    }
}

impl Default for RefCount {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RefCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("RefCount");
        s.field("total", &self.total());
        for i in 0..RefReason::COUNT {
            let count = self.reasons[i].load(Ordering::Relaxed);
            if count > 0 {
                // Safe: we know i < COUNT
                let reason: RefReason = unsafe { std::mem::transmute(i) };
                s.field(&format!("{:?}", reason), &count);
            }
        }
        s.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inc_dec_basic() {
        let rc = RefCount::new();
        assert_eq!(rc.total(), 0);

        rc.inc(RefReason::GraphEdge);
        assert_eq!(rc.total(), 1);
        assert_eq!(rc.count_for(RefReason::GraphEdge), 1);

        rc.inc(RefReason::CacheEntry);
        assert_eq!(rc.total(), 2);

        assert_eq!(rc.dec(RefReason::GraphEdge), DecResult::StillReferenced);
        assert_eq!(rc.total(), 1);

        assert_eq!(rc.dec(RefReason::CacheEntry), DecResult::Unreferenced);
        assert_eq!(rc.total(), 0);
    }

    #[test]
    fn test_multiple_reasons() {
        let rc = RefCount::new();
        rc.inc(RefReason::GraphEdge);
        rc.inc(RefReason::GraphEdge);
        rc.inc(RefReason::OpenFileDesc);

        assert_eq!(rc.total(), 3);
        assert_eq!(rc.count_for(RefReason::GraphEdge), 2);
        assert_eq!(rc.count_for(RefReason::OpenFileDesc), 1);

        assert_eq!(rc.dec(RefReason::GraphEdge), DecResult::StillReferenced);
        assert_eq!(rc.dec(RefReason::GraphEdge), DecResult::StillReferenced);
        assert_eq!(rc.dec(RefReason::OpenFileDesc), DecResult::Unreferenced);
    }

    #[test]
    #[should_panic(expected = "decrement below zero")]
    fn test_dec_below_zero_panics() {
        let rc = RefCount::new();
        rc.dec(RefReason::GraphEdge);
    }
}
