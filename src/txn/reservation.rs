//! Log space reservation and admission control.
//!
//! Episode insight: no GC on the log. Instead, admission control ensures
//! every transaction can complete within bounded space. Max transaction
//! size is bounded; each transaction reserves pessimistically and releases
//! progressively.
//!
//! **Three-tier progressive release** (from Episode):
//! A transaction reserves MAX_TRANSACTION_SIZE pessimistically at start.
//! As it writes data records, used bytes increase. When the mutation phase
//! completes (pre-commit), the unused data portion is released back to the
//! global pool — other transactions can now use that space. After the END
//! record is written, the remaining reservation (end-record overhead) is
//! released.
//!
//! This matters most on constrained devices (slow I/O, small logs) where
//! checkpoint frequency is the bottleneck. More aggressive release = more
//! concurrent transactions before checkpoint is needed.

/// Maximum transaction size in bytes (from Episode: ~70KB).
pub const MAX_TRANSACTION_SIZE: u32 = 70 * 1024;

/// Overhead for the END record (serialized LogRecord with no payload).
/// Conservative estimate: 64 bytes covers the bincode envelope + length prefix.
pub const END_RECORD_OVERHEAD: u32 = 64;

/// Maximum total log size before requiring a checkpoint.
pub const MAX_LOG_SIZE: u64 = 64 * 1024 * 1024; // 64MB for MVP

/// Reservation state for a transaction.
///
/// Tracks three tiers of log space:
/// - **data**: space for mutation records (used progressively)
/// - **end**: space for the END record (released after commit)
///
/// The undo tier (Episode's third tier) is structural in AssFS —
/// separate Add/Remove variants mean undo data is minimal and
/// already accounted for in the data tier.
#[derive(Debug)]
pub struct Reservation {
    /// Total bytes currently reserved by this transaction.
    pub reserved: u32,
    /// Bytes used so far (data records written).
    pub used: u32,
    /// Whether the data portion has been released (pre-commit tightening).
    pub tightened: bool,
}

impl Reservation {
    pub fn new(reserved: u32) -> Self {
        Self {
            reserved,
            used: 0,
            tightened: false,
        }
    }

    /// Check if we can accommodate `additional` bytes.
    pub fn can_accommodate(&self, additional: u32) -> bool {
        self.used + additional <= self.reserved
    }

    /// Record bytes used.
    pub fn use_bytes(&mut self, bytes: u32) {
        self.used += bytes;
        debug_assert!(self.used <= self.reserved);
    }

    /// Bytes remaining in this reservation.
    pub fn remaining(&self) -> u32 {
        self.reserved - self.used
    }

    /// Tighten the reservation: release unused data space, keep only
    /// what's been used plus END_RECORD_OVERHEAD.
    ///
    /// Returns the number of bytes released (for the admission controller).
    /// Idempotent: calling twice returns 0 the second time.
    pub fn tighten(&mut self) -> u32 {
        if self.tightened {
            return 0;
        }
        self.tightened = true;

        let needed = self.used + END_RECORD_OVERHEAD;
        if needed < self.reserved {
            let released = self.reserved - needed;
            self.reserved = needed;
            released
        } else {
            0
        }
    }
}

/// Global admission controller.
#[derive(Debug)]
pub struct AdmissionController {
    /// Total bytes reserved by all active transactions.
    pub total_reserved: u64,
    /// Maximum log size.
    pub max_log_size: u64,
}

impl AdmissionController {
    pub fn new() -> Self {
        Self {
            total_reserved: 0,
            max_log_size: MAX_LOG_SIZE,
        }
    }

    /// Try to reserve space for a new transaction.
    /// Returns None if there isn't enough space (caller must wait for checkpoint).
    pub fn try_reserve(&mut self, bytes: u32) -> Option<Reservation> {
        let new_total = self.total_reserved + bytes as u64;
        if new_total > self.max_log_size {
            None
        } else {
            self.total_reserved = new_total;
            Some(Reservation::new(bytes))
        }
    }

    /// Release partial reservation (progressive release).
    /// Called when a transaction tightens its reservation.
    pub fn release_partial(&mut self, bytes: u32) {
        self.total_reserved = self.total_reserved.saturating_sub(bytes as u64);
    }

    /// Release full reservation when a transaction completes.
    pub fn release(&mut self, reservation: &Reservation) {
        self.total_reserved = self.total_reserved.saturating_sub(reservation.reserved as u64);
    }

    /// Reset after checkpoint — all log space is reclaimed.
    pub fn reset(&mut self) {
        self.total_reserved = 0;
    }
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self::new()
    }
}
