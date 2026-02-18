//! Log manager — sequential append log with sync writes.
//!
//! Log records are tuples: <operation, object_id, old_content_hash, new_content_hash>.
//! Old hash = undo, new hash = redo.
//!
//! Episode lesson (via Mike Young / Camelot): old+new value logging means
//! metadata can be written before transaction commits — undo is the safety
//! net, not a gate. Even sync writes here are simpler than Episode's async
//! log + io_window (that's Phase 2).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ContentHash, LogSequenceNumber, ObjectId, TransactionId};

/// Types of log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogRecordType {
    /// Data modification record.
    Data,
    /// Transaction end (commit marker).
    End,
    /// Checkpoint record.
    Checkpoint,
    /// Restart sentinel (written at recovery completion).
    Restart,
}

/// A single log record (tuple).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// Monotonically increasing sequence number.
    pub lsn: LogSequenceNumber,
    /// Record type.
    pub record_type: LogRecordType,
    /// Transaction this record belongs to.
    pub transaction_id: TransactionId,
    /// Object being modified (for Data records).
    pub object_id: ObjectId,
    /// Previous content hash (undo value).
    pub old_hash: Option<ContentHash>,
    /// New content hash (redo value).
    pub new_hash: Option<ContentHash>,
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
        // Determine next LSN by scanning existing records.
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

    /// Append a data record to the log.
    pub fn log_data(
        &mut self,
        txn_id: TransactionId,
        object_id: ObjectId,
        old_hash: Option<ContentHash>,
        new_hash: Option<ContentHash>,
    ) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::Data,
            transaction_id: txn_id,
            object_id,
            old_hash,
            new_hash,
        })
    }

    /// Write a transaction end (commit) record.
    pub fn log_end(&mut self, txn_id: TransactionId) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::End,
            transaction_id: txn_id,
            object_id: uuid::Uuid::nil(),
            old_hash: None,
            new_hash: None,
        })
    }

    /// Write a checkpoint record.
    pub fn log_checkpoint(&mut self) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::Checkpoint,
            transaction_id: 0,
            object_id: uuid::Uuid::nil(),
            old_hash: None,
            new_hash: None,
        })
    }

    /// Write a restart sentinel.
    pub fn log_restart(&mut self) -> io::Result<LogSequenceNumber> {
        self.append(LogRecord {
            lsn: self.next_lsn,
            record_type: LogRecordType::Restart,
            transaction_id: 0,
            object_id: uuid::Uuid::nil(),
            old_hash: None,
            new_hash: None,
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
                    // Partial write at end of log — truncated record, stop here.
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
