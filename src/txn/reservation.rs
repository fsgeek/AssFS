//! Log space reservation and admission control.
//!
//! Episode insight: no GC on the log. Instead, admission control ensures
//! every transaction can complete within bounded space. Max transaction
//! size is bounded; each transaction reserves pessimistically and releases
//! progressively.

/// Maximum transaction size in bytes (from Episode: ~70KB).
pub const MAX_TRANSACTION_SIZE: u32 = 70 * 1024;

/// Maximum total log size before requiring a checkpoint.
pub const MAX_LOG_SIZE: u64 = 64 * 1024 * 1024; // 64MB for MVP

/// Reservation state for a transaction.
#[derive(Debug)]
pub struct Reservation {
    /// Total bytes reserved for this transaction.
    pub reserved: u32,
    /// Bytes used so far.
    pub used: u32,
}

impl Reservation {
    pub fn new(reserved: u32) -> Self {
        Self { reserved, used: 0 }
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

    /// Release reservation when a transaction completes.
    pub fn release(&mut self, reservation: &Reservation) {
        self.total_reserved = self.total_reserved.saturating_sub(reservation.reserved as u64);
    }
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self::new()
    }
}
