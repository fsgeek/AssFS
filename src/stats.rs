//! Filesystem operation statistics.
//!
//! Atomic counters track operation counts, timing, and data volumes.
//! OpTimer is an RAII guard that accumulates elapsed nanoseconds on drop,
//! so timing captures all exit paths (including early error returns).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

const R: Ordering = Ordering::Relaxed;

/// Metrics for a single operation type: count + cumulative nanoseconds.
pub struct OpMetrics {
    pub count: AtomicU64,
    pub total_ns: AtomicU64,
}

impl OpMetrics {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
        }
    }

    /// Record one invocation and return a timer that accumulates elapsed time on drop.
    pub fn enter(&self) -> OpTimer<'_> {
        self.count.fetch_add(1, R);
        OpTimer {
            ns_counter: &self.total_ns,
            start: Instant::now(),
        }
    }
}

/// RAII timer — adds elapsed nanoseconds to its counter when dropped.
pub struct OpTimer<'a> {
    ns_counter: &'a AtomicU64,
    start: Instant,
}

impl<'a> Drop for OpTimer<'a> {
    fn drop(&mut self) {
        self.ns_counter
            .fetch_add(self.start.elapsed().as_nanos() as u64, R);
    }
}

/// Aggregate filesystem statistics.
pub struct FsStats {
    pub mount_time: Instant,

    // Per-FUSE-operation metrics.
    pub lookup: OpMetrics,
    pub getattr: OpMetrics,
    pub setattr: OpMetrics,
    pub create: OpMetrics,
    pub mkdir: OpMetrics,
    pub unlink: OpMetrics,
    pub rmdir: OpMetrics,
    pub readdir: OpMetrics,
    pub read: OpMetrics,
    pub write: OpMetrics,
    pub link: OpMetrics,
    pub rename: OpMetrics,
    pub open: OpMetrics,
    pub setxattr: OpMetrics,
    pub getxattr: OpMetrics,
    pub listxattr: OpMetrics,
    pub removexattr: OpMetrics,

    // Data volumes (bytes).
    pub read_bytes: AtomicU64,
    pub write_bytes: AtomicU64,

    // Transaction lifecycle.
    pub txn_started: AtomicU64,
    pub txn_committed: AtomicU64,

    // EC merge counters.
    /// Number of EC merge operations.
    pub ec_merges: AtomicU64,
    /// Largest EC seen.
    pub ec_max_size: AtomicU64,
    /// Number of EC group commits (vs singleton).
    pub ec_commits: AtomicU64,
}

impl FsStats {
    pub fn new() -> Self {
        Self {
            mount_time: Instant::now(),
            lookup: OpMetrics::new(),
            getattr: OpMetrics::new(),
            setattr: OpMetrics::new(),
            create: OpMetrics::new(),
            mkdir: OpMetrics::new(),
            unlink: OpMetrics::new(),
            rmdir: OpMetrics::new(),
            readdir: OpMetrics::new(),
            read: OpMetrics::new(),
            write: OpMetrics::new(),
            link: OpMetrics::new(),
            rename: OpMetrics::new(),
            open: OpMetrics::new(),
            setxattr: OpMetrics::new(),
            getxattr: OpMetrics::new(),
            listxattr: OpMetrics::new(),
            removexattr: OpMetrics::new(),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            txn_started: AtomicU64::new(0),
            txn_committed: AtomicU64::new(0),
            ec_merges: AtomicU64::new(0),
            ec_max_size: AtomicU64::new(0),
            ec_commits: AtomicU64::new(0),
        }
    }

    /// Take a snapshot of all counters for display or serialization.
    pub fn snapshot(&self) -> StatsSnapshot {
        let uptime = self.mount_time.elapsed();

        let ops_raw: [(&str, &OpMetrics); 17] = [
            ("lookup", &self.lookup),
            ("getattr", &self.getattr),
            ("setattr", &self.setattr),
            ("create", &self.create),
            ("mkdir", &self.mkdir),
            ("unlink", &self.unlink),
            ("rmdir", &self.rmdir),
            ("readdir", &self.readdir),
            ("read", &self.read),
            ("write", &self.write),
            ("link", &self.link),
            ("rename", &self.rename),
            ("open", &self.open),
            ("setxattr", &self.setxattr),
            ("getxattr", &self.getxattr),
            ("listxattr", &self.listxattr),
            ("removexattr", &self.removexattr),
        ];

        let operations: Vec<OpStat> = ops_raw
            .iter()
            .filter_map(|(name, m)| {
                let count = m.count.load(R);
                if count == 0 {
                    return None;
                }
                let total_ns = m.total_ns.load(R);
                Some(OpStat {
                    name: name.to_string(),
                    count,
                    total_ns,
                    avg_ns: total_ns / count,
                })
            })
            .collect();

        let txn_started = self.txn_started.load(R);
        let txn_committed = self.txn_committed.load(R);

        StatsSnapshot {
            uptime_secs: uptime.as_secs_f64(),
            operations,
            txn_started,
            txn_committed,
            txn_aborted: txn_started.saturating_sub(txn_committed),
            read_bytes: self.read_bytes.load(R),
            write_bytes: self.write_bytes.load(R),
            ec_merges: self.ec_merges.load(R),
            ec_max_size: self.ec_max_size.load(R),
            ec_commits: self.ec_commits.load(R),
            log: None,
        }
    }

    /// Take a snapshot with log decomposition included.
    pub fn snapshot_with_log(&self, log_timings: &crate::txn::log::LogTimings) -> StatsSnapshot {
        let mut snap = self.snapshot();
        let r = R;
        let records = log_timings.record_count.load(r);
        if records > 0 {
            snap.log = Some(LogStats {
                records,
                bytes: log_timings.record_bytes.load(r),
                serialize_ms: log_timings.serialize_ns.load(r) as f64 / 1_000_000.0,
                write_ms: log_timings.write_ns.load(r) as f64 / 1_000_000.0,
                sync_ms: log_timings.sync_ns.load(r) as f64 / 1_000_000.0,
            });
        }
        snap
    }

    /// Print a human-readable summary to stderr.
    pub fn print_summary(&self, log_timings: Option<&crate::txn::log::LogTimings>) {
        let snap = match log_timings {
            Some(lt) => self.snapshot_with_log(lt),
            None => self.snapshot(),
        };

        eprintln!("\n=== AssFS Statistics ===");
        eprintln!("Uptime: {:.1}s\n", snap.uptime_secs);

        if !snap.operations.is_empty() {
            eprintln!(
                "  {:<12} {:>8} {:>12} {:>12}",
                "Op", "Count", "Total ms", "Avg \u{00b5}s"
            );
            eprintln!("  {}", "-".repeat(48));
            for op in &snap.operations {
                eprintln!(
                    "  {:<12} {:>8} {:>12.1} {:>12.0}",
                    op.name,
                    op.count,
                    op.total_ns as f64 / 1_000_000.0,
                    op.avg_ns as f64 / 1_000.0,
                );
            }
            eprintln!();
        }

        eprintln!(
            "  Transactions: {} started, {} committed, {} aborted",
            snap.txn_started, snap.txn_committed, snap.txn_aborted
        );
        if snap.ec_merges > 0 {
            eprintln!(
                "  EC merge: {} merges, max size {}, {} group commits",
                snap.ec_merges, snap.ec_max_size, snap.ec_commits
            );
        }

        if snap.read_bytes > 0 || snap.write_bytes > 0 {
            eprintln!(
                "  Data I/O: {} read, {} written",
                format_bytes(snap.read_bytes),
                format_bytes(snap.write_bytes),
            );
        }

        if let Some(ref log) = snap.log {
            let total = log.serialize_ms + log.write_ms + log.sync_ms;
            eprintln!();
            eprintln!("  Log breakdown ({} records, {}):", log.records, format_bytes(log.bytes));
            eprintln!(
                "    serialize: {:>8.1} ms ({:.0}%)",
                log.serialize_ms,
                if total > 0.0 { log.serialize_ms / total * 100.0 } else { 0.0 }
            );
            eprintln!(
                "    write:     {:>8.1} ms ({:.0}%)",
                log.write_ms,
                if total > 0.0 { log.write_ms / total * 100.0 } else { 0.0 }
            );
            eprintln!(
                "    sync:      {:>8.1} ms ({:.0}%)",
                log.sync_ms,
                if total > 0.0 { log.sync_ms / total * 100.0 } else { 0.0 }
            );
        }
        eprintln!();
    }

    /// Write JSON stats to a file.
    pub fn write_json(
        &self,
        path: &std::path::Path,
        log_timings: Option<&crate::txn::log::LogTimings>,
    ) -> std::io::Result<()> {
        let snap = match log_timings {
            Some(lt) => self.snapshot_with_log(lt),
            None => self.snapshot(),
        };
        let json = serde_json::to_string_pretty(&snap)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }
}

fn format_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{} B", b)
    } else if b < 1024 * 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    }
}

/// Serializable snapshot of all statistics.
#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub uptime_secs: f64,
    pub operations: Vec<OpStat>,
    pub txn_started: u64,
    pub txn_committed: u64,
    pub txn_aborted: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub ec_merges: u64,
    pub ec_max_size: u64,
    pub ec_commits: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<LogStats>,
}

/// Log decomposition: where does log time go?
#[derive(Debug, Serialize)]
pub struct LogStats {
    pub records: u64,
    pub bytes: u64,
    pub serialize_ms: f64,
    pub write_ms: f64,
    pub sync_ms: f64,
}

/// Statistics for a single operation type.
#[derive(Debug, Serialize)]
pub struct OpStat {
    pub name: String,
    pub count: u64,
    pub total_ns: u64,
    pub avg_ns: u64,
}
