//! Log manager — sequential append log with deferred sync and background sync.
//!
//! Every mutation (content change, graph node add/remove, graph edge
//! add/remove) is logged with enough old/new state to undo and redo.
//!
//! Episode lesson (via Mike Young / Camelot): old+new value logging means
//! metadata can be written before transaction commits — undo is the safety
//! net, not a gate.
//!
//! **Deferred sync**: Data and BatchEnd records are buffered without fsync.
//! Only commit-point records (End, Checkpoint, Restart) trigger flush+sync,
//! which makes ALL preceding buffered records durable in a single fsync.
//! This is safe because:
//! - If Data records are lost (crash before END sync), the transaction has
//!   no END record, so recovery treats it as uncommitted.
//! - There's nothing to undo because the Data records are also gone.
//! - Content writes (STEAL mode) create new immutable blobs that become
//!   orphans if the transaction never commits — no corruption.
//!
//! **Background sync thread**: When enabled, End records only flush to the
//! kernel buffer (not fsync). A background thread periodically calls
//! sync_all(), advancing `io_window` to cover all flushed records in a
//! single fsync. This batches N concurrent commits into 1 fsync — the win
//! scales with concurrency (multi-threaded FUSE).
//!
//! The `io_window` tracks the highest LSN known to be on stable storage.
//! `flushed_lsn` tracks the highest LSN pushed to the kernel buffer.
//! Invariant: io_window <= flushed_lsn <= next_lsn - 1.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{ContentHash, EdgeType, LogSequenceNumber, ObjectId, TransactionId};

/// Shared state between LogManager and the background sync thread.
///
/// The sync thread reads `flushed_lsn`, calls sync_all on a dup'd fd,
/// then advances `io_window`. Waiters (FUSE fsync) block on the condvar
/// until `io_window >= target_lsn`.
pub struct SyncState {
    /// Highest LSN pushed to kernel buffer via flush().
    /// Written by main thread (under TransactionManager Mutex).
    pub flushed_lsn: AtomicU64,
    /// Highest LSN on stable storage (after sync_all).
    /// Written by sync thread, read by anyone.
    pub io_window: AtomicU64,
    /// Notifies the sync thread that new data has been flushed.
    work_notify: Condvar,
    work_mutex: Mutex<()>,
    /// Notifies durability waiters that io_window has advanced.
    durable_notify: Condvar,
    durable_mutex: Mutex<()>,
    /// Shutdown flag.
    pub shutdown: AtomicBool,
}

impl SyncState {
    fn new(initial_io_window: u64) -> Self {
        Self {
            flushed_lsn: AtomicU64::new(initial_io_window),
            io_window: AtomicU64::new(initial_io_window),
            work_notify: Condvar::new(),
            work_mutex: Mutex::new(()),
            durable_notify: Condvar::new(),
            durable_mutex: Mutex::new(()),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Block until io_window >= target_lsn or timeout.
    /// Returns true if the target became durable, false on timeout.
    pub fn wait_for_durable(&self, target_lsn: u64, timeout: Duration) -> bool {
        if self.io_window.load(Ordering::Acquire) >= target_lsn {
            return true;
        }
        let lock = self.durable_mutex.lock().unwrap();
        let deadline = Instant::now() + timeout;
        let mut guard = lock;
        loop {
            if self.io_window.load(Ordering::Acquire) >= target_lsn {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (new_guard, _timeout_result) =
                self.durable_notify.wait_timeout(guard, remaining).unwrap();
            guard = new_guard;
        }
    }
}

/// Log-level timing counters. Shared with FsStats via Arc or direct access.
pub struct LogTimings {
    pub serialize_ns: AtomicU64,
    pub write_ns: AtomicU64,
    pub sync_ns: AtomicU64,
    pub record_count: AtomicU64,
    pub record_bytes: AtomicU64,
}

impl LogTimings {
    pub fn new() -> Self {
        Self {
            serialize_ns: AtomicU64::new(0),
            write_ns: AtomicU64::new(0),
            sync_ns: AtomicU64::new(0),
            record_count: AtomicU64::new(0),
            record_bytes: AtomicU64::new(0),
        }
    }
}

/// Types of log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogRecordType {
    /// A mutation record (content or graph).
    Data,
    /// Transaction end (commit marker).
    End,
    /// Batch end — member of an EC that has finished.
    BatchEnd,
    /// Checkpoint record.
    Checkpoint,
    /// Restart sentinel (written at recovery completion).
    Restart,
}

/// The payload of a mutation record.
///
/// Each variant carries enough state to both redo (apply forward) and
/// undo (revert). This is old/new value logging — not write-ahead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogPayload {
    /// Content hash changed for an object.
    ContentChange {
        object_id: ObjectId,
        old_hash: Option<ContentHash>,
        new_hash: Option<ContentHash>,
    },
    /// Node added to the graph.
    AddNode {
        /// Serialized GraphNode (full state for undo = remove).
        node_data: Vec<u8>,
        node_id: ObjectId,
    },
    /// Node removed from the graph.
    RemoveNode {
        /// Serialized GraphNode (full state for undo = re-add).
        node_data: Vec<u8>,
        node_id: ObjectId,
    },
    /// Edge added to the graph.
    AddEdge {
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    },
    /// Edge removed from the graph.
    RemoveEdge {
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    },
    /// Batch end — this member is part of an EC headed by ec_head.
    BatchEnd {
        ec_head: TransactionId,
    },
}

/// A single log record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// Monotonically increasing sequence number.
    pub lsn: LogSequenceNumber,
    /// Record type.
    pub record_type: LogRecordType,
    /// Transaction this record belongs to.
    pub transaction_id: TransactionId,
    /// Payload (only meaningful for Data records).
    pub payload: Option<LogPayload>,
}

/// The log manager.
pub struct LogManager {
    log_path: PathBuf,
    writer: BufWriter<File>,
    next_lsn: LogSequenceNumber,
    pub timings: LogTimings,
    /// When true, skip fsync on every record (unsafe, for measurement only).
    pub no_sync: bool,
    /// Shared sync state — used by background thread and durability waiters.
    /// When no sync thread is running, io_window is updated directly in append().
    sync_state: Arc<SyncState>,
    /// Handle to the background sync thread (None if not started).
    sync_thread: Option<SyncThreadHandle>,
}

struct SyncThreadHandle {
    join_handle: JoinHandle<()>,
}

impl LogManager {
    /// Open or create a log file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let next_lsn = if path.exists() {
            let records = Self::read_all_records(path)?;
            records.last().map_or(1, |r| r.lsn + 1)
        } else {
            1
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let writer = BufWriter::new(file);

        let initial_io_window = if next_lsn > 1 { next_lsn - 1 } else { 0 };

        Ok(Self {
            log_path: path.to_path_buf(),
            writer,
            next_lsn,
            timings: LogTimings::new(),
            no_sync: false,
            sync_state: Arc::new(SyncState::new(initial_io_window)),
            sync_thread: None,
        })
    }

    /// Start the background sync thread.
    ///
    /// The thread periodically fsyncs the log file and advances io_window.
    /// When active, End records only flush to kernel (no fsync) — the thread
    /// batches multiple commits into a single fsync. Checkpoint and Restart
    /// records still sync synchronously for correctness.
    ///
    /// `interval` controls how often the thread wakes to sync. Shorter = lower
    /// latency, longer = more batching. 50ms is a reasonable default.
    pub fn start_sync_thread(&mut self, interval: Duration) -> io::Result<()> {
        if self.sync_thread.is_some() {
            return Ok(()); // already running
        }

        // Dup the file descriptor so the sync thread can call sync_all
        // independently of the BufWriter.
        let sync_fd = self.writer.get_ref().try_clone()?;
        let state = Arc::clone(&self.sync_state);
        let no_sync = self.no_sync;

        let join_handle = thread::Builder::new()
            .name("assfs-sync".to_string())
            .spawn(move || {
                Self::sync_thread_main(sync_fd, state, interval, no_sync);
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        self.sync_thread = Some(SyncThreadHandle { join_handle });
        log::info!(
            "Background sync thread started (interval: {:?})",
            interval
        );
        Ok(())
    }

    /// Stop the background sync thread and perform a final sync.
    pub fn stop_sync_thread(&mut self) -> io::Result<()> {
        if let Some(handle) = self.sync_thread.take() {
            // Signal shutdown.
            self.sync_state.shutdown.store(true, Ordering::Release);
            self.sync_state.work_notify.notify_one();

            // Wait for the thread to finish.
            handle
                .join_handle
                .join()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "sync thread panicked"))?;

            // Final sync to ensure everything is durable.
            self.sync_pending()?;

            log::info!("Background sync thread stopped");
        }
        Ok(())
    }

    /// Whether a background sync thread is active.
    pub fn has_sync_thread(&self) -> bool {
        self.sync_thread.is_some()
    }

    /// Get the shared sync state (for external waiters like FUSE fsync).
    pub fn sync_state(&self) -> Arc<SyncState> {
        Arc::clone(&self.sync_state)
    }

    /// Background sync thread main loop.
    fn sync_thread_main(sync_fd: File, state: Arc<SyncState>, interval: Duration, no_sync: bool) {
        loop {
            // Wait for work or timeout.
            {
                let lock = state.work_mutex.lock().unwrap();
                let _ = state.work_notify.wait_timeout(lock, interval);
            }

            if state.shutdown.load(Ordering::Acquire) {
                // Final sync before exit.
                if !no_sync {
                    let _ = sync_fd.sync_all();
                }
                let flushed = state.flushed_lsn.load(Ordering::Acquire);
                state.io_window.store(flushed, Ordering::Release);
                state.durable_notify.notify_all();
                break;
            }

            let flushed = state.flushed_lsn.load(Ordering::Acquire);
            let current_io = state.io_window.load(Ordering::Relaxed);

            if flushed > current_io {
                // There's unfsynced data in the kernel buffer. Sync it.
                if !no_sync {
                    if sync_fd.sync_all().is_err() {
                        continue; // Retry next cycle.
                    }
                }
                state.io_window.store(flushed, Ordering::Release);
                state.durable_notify.notify_all();
            }
        }
    }

    /// Append a data record with the given payload.
    pub fn log_data(
        &mut self,
        txn_id: TransactionId,
        payload: LogPayload,
    ) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::Data,
            transaction_id: txn_id,
            payload: Some(payload),
        })
    }

    /// Write a transaction end (commit) record.
    pub fn log_end(&mut self, txn_id: TransactionId) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::End,
            transaction_id: txn_id,
            payload: None,
        })
    }

    /// Write a batch end record for an EC member.
    pub fn log_batch_end(
        &mut self,
        member_id: TransactionId,
        head_id: TransactionId,
    ) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::BatchEnd,
            transaction_id: member_id,
            payload: Some(LogPayload::BatchEnd { ec_head: head_id }),
        })
    }

    /// Write a checkpoint record.
    pub fn log_checkpoint(&mut self) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::Checkpoint,
            transaction_id: 0,
            payload: None,
        })
    }

    /// Write a restart sentinel.
    pub fn log_restart(&mut self) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::Restart,
            transaction_id: 0,
            payload: None,
        })
    }

    /// Append a record, return its LSN.
    ///
    /// **Deferred sync**: Only commit-point records (End, Checkpoint, Restart)
    /// trigger flush+sync. Data and BatchEnd records are buffered in the
    /// BufWriter without fsync. The commit-point sync makes ALL preceding
    /// buffered records durable in a single fsync call.
    ///
    /// **Background sync mode**: When a sync thread is running, End records
    /// only flush to the kernel buffer (no fsync). The background thread
    /// batches fsyncs, advancing io_window. Checkpoint and Restart records
    /// still sync synchronously — they're rare and need immediate durability.
    ///
    /// This is safe because of old/new value logging: if buffered Data
    /// records are lost in a crash, the transaction has no END record on
    /// disk, so recovery treats it as uncommitted with nothing to undo.
    fn append(&mut self, record: LogRecord) -> io::Result<LogSequenceNumber> {
        let lsn = record.lsn;
        let r = Ordering::Relaxed;

        let record_type = record.record_type;

        // Commit-point records need at least a flush.
        let is_commit_point = matches!(
            record_type,
            LogRecordType::End | LogRecordType::Checkpoint | LogRecordType::Restart
        );

        // Checkpoint/Restart always sync synchronously (rare, correctness-critical).
        // End records sync synchronously only when no background sync thread.
        let needs_immediate_sync = match record_type {
            LogRecordType::Checkpoint | LogRecordType::Restart => true,
            LogRecordType::End => !self.has_sync_thread(),
            _ => false,
        };

        // Phase 1: Serialize.
        let t0 = Instant::now();
        let record_bytes =
            bincode::serialize(&record).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let len = record_bytes.len() as u32;
        self.timings.serialize_ns.fetch_add(t0.elapsed().as_nanos() as u64, r);

        // Phase 2: Write to buffer.
        let t1 = Instant::now();
        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&record_bytes)?;
        self.timings.write_ns.fetch_add(t1.elapsed().as_nanos() as u64, r);

        // Phase 3: Flush and/or sync.
        if is_commit_point {
            let t2 = Instant::now();
            // Always flush to kernel buffer for commit-point records.
            self.writer.flush()?;

            if needs_immediate_sync {
                // Synchronous path: fsync now, advance io_window directly.
                if !self.no_sync {
                    self.writer.get_ref().sync_all()?;
                }
                self.sync_state.io_window.store(lsn, Ordering::Release);
                self.sync_state.flushed_lsn.store(lsn, Ordering::Release);
            } else {
                // Async path: update flushed_lsn, wake the sync thread.
                // The sync thread will fsync and advance io_window.
                self.sync_state.flushed_lsn.store(lsn, Ordering::Release);
                self.sync_state.work_notify.notify_one();
            }

            self.timings.sync_ns.fetch_add(t2.elapsed().as_nanos() as u64, r);
        }

        self.timings.record_count.fetch_add(1, r);
        self.timings.record_bytes.fetch_add(record_bytes.len() as u64, r);

        self.next_lsn += 1;
        Ok(lsn)
    }

    /// The highest LSN known to be on stable storage.
    /// Records with LSN <= io_window are durable. Records above io_window
    /// may be in the BufWriter or kernel buffer but not yet fsynced.
    pub fn io_window(&self) -> LogSequenceNumber {
        self.sync_state.io_window.load(Ordering::Acquire)
    }

    /// Flush all buffered records to stable storage.
    /// Use this for clean shutdown or explicit sync points.
    pub fn sync_pending(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        if !self.no_sync {
            self.writer.get_ref().sync_all()?;
        }
        // io_window advances to cover everything written so far.
        if self.next_lsn > 1 {
            let lsn = self.next_lsn - 1;
            self.sync_state.io_window.store(lsn, Ordering::Release);
            self.sync_state.flushed_lsn.store(lsn, Ordering::Release);
            self.sync_state.durable_notify.notify_all();
        }
        Ok(())
    }

    /// Truncate the log and write a restart sentinel.
    ///
    /// Called during checkpoint after all state (graph, content index) has
    /// been flushed to disk. The log can be safely discarded because:
    /// - All committed transactions are reflected in the flushed state.
    /// - No active transactions exist (caller asserts this).
    /// - Recovery after a post-checkpoint crash loads the flushed state
    ///   and replays only post-checkpoint log records.
    pub fn truncate_and_restart(&mut self) -> io::Result<()> {
        // Flush any pending data in the buffer first.
        self.writer.flush()?;

        // Truncate the log file to zero length.
        self.writer.get_ref().set_len(0)?;

        // Write a restart sentinel. This is a commit-point record,
        // so it triggers flush+sync, making the truncation durable.
        self.log_restart()?;

        log::info!(
            "Log truncated and restarted at LSN {}",
            self.next_lsn - 1
        );

        Ok(())
    }

    /// Read all records from the log file (for recovery).
    pub fn read_all_records(path: &Path) -> io::Result<Vec<LogRecord>> {
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut records = Vec::new();

        loop {
            let pos = reader.stream_position()?;
            if pos >= file_len {
                break;
            }

            // Read length prefix.
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    log::warn!("Truncated record at log offset {}, ignoring", pos);
                    break;
                }
                Err(e) => return Err(e),
            }
            let len = u32::from_le_bytes(len_buf) as usize;

            // Read record data.
            let mut record_buf = vec![0u8; len];
            match reader.read_exact(&mut record_buf) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    log::warn!("Truncated record data at log offset {}, ignoring", pos);
                    break;
                }
                Err(e) => return Err(e),
            }

            match bincode::deserialize::<LogRecord>(&record_buf) {
                Ok(record) => records.push(record),
                Err(e) => {
                    log::warn!("Corrupt record at log offset {}: {}", pos, e);
                    break;
                }
            }
        }

        Ok(records)
    }

    /// Current byte position (for admission control).
    pub fn current_size(&self) -> io::Result<u64> {
        let meta = fs::metadata(&self.log_path)?;
        Ok(meta.len())
    }

    /// Next LSN that will be assigned.
    pub fn next_lsn(&self) -> LogSequenceNumber {
        self.next_lsn
    }
}

impl Drop for LogManager {
    fn drop(&mut self) {
        if self.sync_thread.is_some() {
            // Best-effort shutdown — ignore errors during drop.
            let _ = self.stop_sync_thread();
        }
    }
}
