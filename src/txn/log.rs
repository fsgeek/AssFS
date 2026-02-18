//! Log manager — sequential append log with sync writes.
//!
//! Every mutation (content change, graph node add/remove, graph edge
//! add/remove) is logged with enough old/new state to undo and redo.
//!
//! Episode lesson (via Mike Young / Camelot): old+new value logging means
//! metadata can be written before transaction commits — undo is the safety
//! net, not a gate.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ContentHash, EdgeType, LogSequenceNumber, ObjectId, TransactionId};

/// Types of log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogRecordType {
    /// A mutation record (content or graph).
    Data,
    /// Transaction end (commit marker).
    End,
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

    /// Append a record, sync to disk, return its LSN.
    fn append(&mut self, record: LogRecord) -> io::Result<LogSequenceNumber> {
        let lsn = record.lsn;

        let record_bytes =
            bincode::serialize(&record).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let len = record_bytes.len() as u32;

        // Write length prefix + record data.
        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&record_bytes)?;
        self.writer.flush()?;

        // Sync to stable storage (this is the commit point for End records).
        self.writer.get_ref().sync_all()?;

        self.next_lsn += 1;
        Ok(lsn)
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
