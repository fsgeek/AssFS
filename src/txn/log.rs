//! Log manager — sequential append log with deferred sync.
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
//! The `io_window` field tracks the highest LSN known to be on stable
//! storage. All records with LSN <= io_window are durable.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::{ContentHash, EdgeType, LogSequenceNumber, ObjectId, TransactionId};

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
    /// Highest LSN known to be on stable storage (durable after fsync).
    /// All records with LSN <= io_window are guaranteed durable.
    io_window: LogSequenceNumber,
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

        Ok(Self {
            log_path: path.to_path_buf(),
            writer,
            next_lsn,
            timings: LogTimings::new(),
            no_sync: false,
            io_window: if next_lsn > 1 { next_lsn - 1 } else { 0 },
        })
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
    /// This is safe because of old/new value logging: if buffered Data
    /// records are lost in a crash, the transaction has no END record on
    /// disk, so recovery treats it as uncommitted with nothing to undo.
    fn append(&mut self, record: LogRecord) -> io::Result<LogSequenceNumber> {
        let lsn = record.lsn;
        let r = Ordering::Relaxed;

        // Commit-point records require durable sync.
        let needs_sync = matches!(
            record.record_type,
            LogRecordType::End | LogRecordType::Checkpoint | LogRecordType::Restart
        );

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

        // Phase 3: Flush + sync only for commit-point records.
        // The flush() pushes all buffered data (including prior Data records)
        // to the kernel, and sync_all() forces it to stable storage.
        if needs_sync {
            let t2 = Instant::now();
            self.writer.flush()?;
            if !self.no_sync {
                self.writer.get_ref().sync_all()?;
            }
            self.timings.sync_ns.fetch_add(t2.elapsed().as_nanos() as u64, r);
            self.io_window = lsn;
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
        self.io_window
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
            self.io_window = self.next_lsn - 1;
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
