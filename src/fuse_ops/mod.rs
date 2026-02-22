//! FUSE low-level adapter.
//!
//! Maps FUSE operations to graph namespace operations through the
//! hierarchical projection layer.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};

use crate::graph::node::{GraphNode, NodeMetadata};
use crate::graph::operations::GraphNamespace;
use crate::projection::{InodeTable, Ino};
use crate::stats::FsStats;
use crate::store::content::ContentStore;
use crate::txn::recovery;
use crate::txn::tlock::TlockTable;
use crate::txn::transaction::TransactionManager;
use crate::{EdgeType, NodeType, ObjectId, TransactionId, EMPTY_HASH};

use std::sync::atomic::Ordering::Relaxed;

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4096;

/// The AssFS filesystem — ties all layers together.
pub struct AssFs {
    pub graph: GraphNamespace,
    pub store: ContentStore,
    pub txn_mgr: std::sync::Mutex<TransactionManager>,
    pub inodes: InodeTable,
    pub root_id: ObjectId,
    /// Tag registry node — edges to all tag anchor nodes.
    pub tag_registry_id: ObjectId,
    pub tlocks: TlockTable,
    pub stats: FsStats,
    pub stats_file: Option<PathBuf>,
    pub no_sync: bool,
    pub no_steal: bool,
    #[allow(dead_code)]
    data_dir: PathBuf,
}

impl AssFs {
    /// Open or create the filesystem at the given data directory.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(data_dir)?;

        let store = ContentStore::open(data_dir)?;
        let graph = GraphNamespace::open(data_dir)?;
        let log_path = data_dir.join("assfs.log");

        // Run recovery before anything else.
        let recovery_result = recovery::recover(&log_path, &store, &graph)?;
        log::info!("Recovery: {:?}", recovery_result);

        let mut txn_mgr = TransactionManager::open(&log_path)?;
        txn_mgr.set_next_txn_id(recovery_result.max_txn_id);

        let inodes = InodeTable::new();

        // Determine root: check for persisted root_id, or init fresh.
        let root_id_path = data_dir.join("root_id");
        let root_id = if root_id_path.exists() {
            let bytes = std::fs::read(&root_id_path)?;
            let id: ObjectId = bincode::deserialize(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            // Verify it still exists in the graph.
            if graph.node_exists(&id) {
                id
            } else {
                let id = graph.init_root(unsafe { libc::getuid() }, unsafe { libc::getgid() });
                store.register_empty(id);
                let bytes = bincode::serialize(&id)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                std::fs::write(&root_id_path, &bytes)?;
                id
            }
        } else {
            let id = graph.init_root(unsafe { libc::getuid() }, unsafe { libc::getgid() });
            store.register_empty(id);
            let bytes =
                bincode::serialize(&id).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            std::fs::write(&root_id_path, &bytes)?;
            id
        };

        inodes.set_root(root_id);

        // Tag registry — a pure graph node that indexes all tag anchors.
        let tag_reg_path = data_dir.join("tag_registry_id");
        let tag_registry_id = if tag_reg_path.exists() {
            let bytes = std::fs::read(&tag_reg_path)?;
            let id: ObjectId = bincode::deserialize(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if graph.node_exists(&id) {
                id
            } else {
                let id = uuid::Uuid::new_v4();
                graph.add_node(crate::graph::node::GraphNode::new_tag_anchor(id));
                let bytes = bincode::serialize(&id)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                std::fs::write(&tag_reg_path, &bytes)?;
                id
            }
        } else {
            let id = uuid::Uuid::new_v4();
            graph.add_node(crate::graph::node::GraphNode::new_tag_anchor(id));
            let bytes =
                bincode::serialize(&id).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            std::fs::write(&tag_reg_path, &bytes)?;
            id
        };

        Ok(Self {
            graph,
            store,
            txn_mgr: std::sync::Mutex::new(txn_mgr),
            inodes,
            root_id,
            tag_registry_id,
            tlocks: TlockTable::new(),
            stats: FsStats::new(),
            stats_file: None,
            no_sync: false,
            no_steal: false,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// Enable no-sync mode on the log manager (unsafe, for measurement only).
    pub fn set_no_sync(&mut self, no_sync: bool) {
        self.no_sync = no_sync;
        if no_sync {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            txn_mgr.log.no_sync = true;
            log::warn!("AssFS: --no-sync enabled. Crash recovery is UNSAFE.");
        }
    }

    /// Enable no-steal mode: buffer content writes until commit (for measurement).
    pub fn set_no_steal(&mut self, no_steal: bool) {
        self.no_steal = no_steal;
        if no_steal {
            log::warn!("NO-STEAL mode: content writes buffered until commit (for measurement)");
        }
    }

    /// Flush all state to disk.
    pub fn flush_all(&self) -> io::Result<()> {
        self.store.flush_index()?;
        self.graph.flush()?;
        Ok(())
    }

    /// Start a transaction, performing a proactive checkpoint if admission
    /// control rejects the request due to log space exhaustion.
    ///
    /// Episode insight: checkpoint is NOT an error — it's a normal lifecycle
    /// event. The FUSE layer should handle it transparently rather than
    /// propagating ENOSPC to the caller.
    ///
    /// Precondition: `txn_mgr` must have no active transactions for
    /// checkpoint to succeed (single-threaded FUSE guarantees this).
    pub fn start_txn_or_checkpoint(
        &self,
        txn_mgr: &mut TransactionManager,
    ) -> io::Result<TransactionId> {
        match txn_mgr.start_txn() {
            Ok(id) => {
                self.stats.txn_started.fetch_add(1, Relaxed);
                Ok(id)
            }
            Err(_) => {
                // Admission control rejected: log space exhausted.
                // Flush durable state, checkpoint, and retry.
                log::info!("Admission control full, triggering proactive checkpoint");
                self.store.flush_index()?;
                self.graph.flush()?;
                txn_mgr.checkpoint()?;
                self.stats.checkpoint_count.fetch_add(1, Relaxed);
                let id = txn_mgr.start_txn()?;
                self.stats.txn_started.fetch_add(1, Relaxed);
                Ok(id)
            }
        }
    }

    /// Convert a GraphNode to FUSE FileAttr.
    fn node_to_attr(&self, node: &GraphNode) -> FileAttr {
        let ino = self.inodes.get_or_assign(node.id);
        let kind = match node.node_type {
            NodeType::File => FileType::RegularFile,
            NodeType::Directory => FileType::Directory,
            NodeType::Symlink => FileType::Symlink,
            NodeType::TagAnchor => FileType::RegularFile, // not reachable via POSIX tree
        };

        FileAttr {
            ino,
            size: node.metadata.size,
            blocks: (node.metadata.size + 511) / 512,
            atime: system_time(node.metadata.atime, node.metadata.atime_nsec),
            mtime: system_time(node.metadata.mtime, node.metadata.mtime_nsec),
            ctime: system_time(node.metadata.ctime, node.metadata.ctime_nsec),
            crtime: system_time(node.metadata.ctime, node.metadata.ctime_nsec),
            kind,
            perm: node.metadata.mode as u16,
            nlink: node.metadata.nlink,
            uid: node.metadata.uid,
            gid: node.metadata.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    /// Resolve an inode to a GraphNode.
    fn get_node_by_ino(&self, ino: Ino) -> Option<GraphNode> {
        let id = self.inodes.lookup_ino(ino)?;
        self.graph.get_node(&id)
    }
}

fn system_time(sec: i64, nsec: u32) -> SystemTime {
    if sec >= 0 {
        UNIX_EPOCH + Duration::new(sec as u64, nsec)
    } else {
        UNIX_EPOCH
    }
}

impl Filesystem for AssFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let _t = self.stats.lookup.enter();
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let parent_id = match self.inodes.lookup_ino(parent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Try Child edge first, then Hardlink.
        let target_id = self
            .graph
            .resolve(&parent_id, EdgeType::Child, name_str)
            .or_else(|| self.graph.resolve(&parent_id, EdgeType::Hardlink, name_str));

        match target_id {
            Some(tid) => match self.graph.get_node(&tid) {
                Some(node) => {
                    let attr = self.node_to_attr(&node);
                    reply.entry(&TTL, &attr, 0);
                }
                None => reply.error(libc::ENOENT),
            },
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let _t = self.stats.getattr.enter();
        match self.get_node_by_ino(ino) {
            Some(node) => {
                let attr = self.node_to_attr(&node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(libc::ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgflags: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let _t = self.stats.setattr.enter();
        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Handle truncation.
        if let Some(new_size) = size {
            if new_size == 0 {
                self.store.register_empty(obj_id);
            } else {
                // Partial truncation: read, truncate, rewrite.
                match self.store.get_content(&obj_id) {
                    Ok(mut data) => {
                        data.truncate(new_size as usize);
                        if let Err(_) = self.store.put_object(obj_id, &data) {
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                    Err(_) => {
                        // No content yet, just set size.
                        self.store.register_empty(obj_id);
                    }
                }
            }
        }

        let updated = self.graph.update_node(&obj_id, |node| {
            if let Some(m) = mode {
                node.metadata.mode = m;
            }
            if let Some(u) = uid {
                node.metadata.uid = u;
            }
            if let Some(g) = gid {
                node.metadata.gid = g;
            }
            if let Some(s) = size {
                node.metadata.size = s;
            }
            if let Some(t) = atime {
                let (sec, nsec) = match t {
                    TimeOrNow::SpecificTime(st) => {
                        let d = st.duration_since(UNIX_EPOCH).unwrap_or_default();
                        (d.as_secs() as i64, d.subsec_nanos())
                    }
                    TimeOrNow::Now => NodeMetadata::now(),
                };
                node.metadata.atime = sec;
                node.metadata.atime_nsec = nsec;
            }
            if let Some(t) = mtime {
                let (sec, nsec) = match t {
                    TimeOrNow::SpecificTime(st) => {
                        let d = st.duration_since(UNIX_EPOCH).unwrap_or_default();
                        (d.as_secs() as i64, d.subsec_nanos())
                    }
                    TimeOrNow::Now => NodeMetadata::now(),
                };
                node.metadata.mtime = sec;
                node.metadata.mtime_nsec = nsec;
            }
            let (sec, nsec) = NodeMetadata::now();
            node.metadata.ctime = sec;
            node.metadata.ctime_nsec = nsec;
        });

        if !updated {
            reply.error(libc::ENOENT);
            return;
        }

        match self.get_node_by_ino(ino) {
            Some(node) => {
                let attr = self.node_to_attr(&node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(libc::ENOENT),
        }
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let _t = self.stats.create.enter();
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let parent_id = match self.inodes.lookup_ino(parent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check if name already exists.
        if self
            .graph
            .resolve(&parent_id, EdgeType::Child, name_str)
            .is_some()
        {
            reply.error(libc::EEXIST);
            return;
        }

        // 1. Start transaction (brief lock, with proactive checkpoint).
        let txn_id = {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            match self.start_txn_or_checkpoint(&mut txn_mgr) {
                Ok(id) => id,
                Err(_) => {
                    reply.error(libc::ENOSPC);
                    return;
                }
            }
        };

        // 2. Touch tlocks (may trigger EC merge).
        if let Some(other) = self.tlocks.touch(parent_id, txn_id) {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            txn_mgr.ec_merge(txn_id, other);
        }

        // 3. Create new file node.
        let file_id = uuid::Uuid::new_v4();
        let file_node = GraphNode::new_file(file_id, req.uid(), req.gid(), mode);
        let node_data = bincode::serialize(&file_node).unwrap_or_default();

        // 4. Touch tlock for the new node (no conflict possible, new node).
        self.tlocks.touch(file_id, txn_id);

        // 5. Log operations (brief lock).
        {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            if txn_mgr
                .log_add_node(txn_id, file_id, node_data)
                .is_err()
                || txn_mgr
                    .log_add_edge(
                        txn_id,
                        parent_id,
                        file_id,
                        EdgeType::Child,
                        name_str.to_string(),
                    )
                    .is_err()
                || txn_mgr
                    .log_content_change(txn_id, file_id, None, Some(EMPTY_HASH))
                    .is_err()
            {
                let _ = txn_mgr.abort_txn(txn_id);
                self.tlocks.release_all(txn_id);
                reply.error(libc::EIO);
                return;
            }
        }

        // 6. Apply to in-memory state.
        self.graph.add_node(file_node.clone());
        if let Err(_) = self.graph.add_edge(
            parent_id,
            file_id,
            EdgeType::Child,
            name_str.to_string(),
        ) {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            let _ = txn_mgr.abort_txn(txn_id);
            self.tlocks.release_all(txn_id);
            reply.error(libc::EIO);
            return;
        }
        self.store.register_empty(file_id);

        // 7. End transaction (brief lock, may or may not commit depending on EC).
        let pending = {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            match txn_mgr.end_txn(txn_id) {
                Ok(p) => p,
                Err(_) => {
                    self.tlocks.release_all(txn_id);
                    reply.error(libc::EIO);
                    return;
                }
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        // 8. Flush pending writes if NO-STEAL and EC committed.
        if self.no_steal && !pending.is_empty() {
            if let Err(_) = self.store.flush_pending(&pending) {
                self.tlocks.release_all(txn_id);
                reply.error(libc::EIO);
                return;
            }
        }

        // 9. Release tlocks.
        self.tlocks.release_all(txn_id);

        let attr = self.node_to_attr(&file_node);
        reply.created(&TTL, &attr, 0, 0, 0);
    }

    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let _t = self.stats.mkdir.enter();
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let parent_id = match self.inodes.lookup_ino(parent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        if self
            .graph
            .resolve(&parent_id, EdgeType::Child, name_str)
            .is_some()
        {
            reply.error(libc::EEXIST);
            return;
        }

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlock on parent directory (EC merge trigger under concurrency).
        if let Some(other) = self.tlocks.touch(parent_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        let dir_id = uuid::Uuid::new_v4();
        let dir_node = GraphNode::new_dir(dir_id, req.uid(), req.gid(), mode);
        let node_data = bincode::serialize(&dir_node).unwrap_or_default();

        // Log node addition, edge addition, and content registration.
        if txn_mgr
            .log_add_node(txn_id, dir_id, node_data)
            .is_err()
            || txn_mgr
                .log_add_edge(
                    txn_id,
                    parent_id,
                    dir_id,
                    EdgeType::Child,
                    name_str.to_string(),
                )
                .is_err()
            || txn_mgr
                .log_content_change(txn_id, dir_id, None, Some(EMPTY_HASH))
                .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Apply to in-memory state.
        self.graph.add_node(dir_node.clone());
        if let Err(_) =
            self.graph
                .add_edge(parent_id, dir_id, EdgeType::Child, name_str.to_string())
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }
        self.store.register_empty(dir_id);

        // Increment parent nlink.
        self.graph.update_node(&parent_id, |n| {
            n.metadata.nlink += 1;
        });

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        let attr = self.node_to_attr(&dir_node);
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let _t = self.stats.unlink.enter();
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let parent_id = match self.inodes.lookup_ino(parent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Find the target.
        let target_id = match self.graph.resolve(&parent_id, EdgeType::Child, name_str) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check it's not a directory.
        if let Some(node) = self.graph.get_node(&target_id) {
            if node.node_type == NodeType::Directory {
                reply.error(libc::EISDIR);
                return;
            }
        }

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlocks on parent directory and target file.
        if let Some(other) = self.tlocks.touch(parent_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }
        if let Some(other) = self.tlocks.touch(target_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        // Capture node data for potential undo before removing anything.
        let target_node_data = self
            .graph
            .get_node(&target_id)
            .and_then(|n| bincode::serialize(&n).ok())
            .unwrap_or_default();
        let old_hash = self.store.get_hash(&target_id);

        // Log edge removal.
        if txn_mgr
            .log_remove_edge(
                txn_id,
                parent_id,
                target_id,
                EdgeType::Child,
                name_str.to_string(),
            )
            .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Apply edge removal.
        self.graph
            .remove_edge(&parent_id, EdgeType::Child, name_str);

        // Check if node should be removed (no more incoming edges).
        let should_remove = self.graph.reverse_lookup(&target_id).is_empty();

        if should_remove {
            // Log node and content removal.
            if txn_mgr
                .log_remove_node(txn_id, target_id, target_node_data)
                .is_err()
                || txn_mgr
                    .log_content_change(txn_id, target_id, old_hash, None)
                    .is_err()
            {
                let _ = txn_mgr.abort_txn(txn_id);
                reply.error(libc::EIO);
                return;
            }
            self.graph.remove_node(&target_id);
            self.store.remove_object(&target_id);
            self.inodes.remove_id(&target_id);
        } else {
            self.graph.update_node(&target_id, |n| {
                n.metadata.nlink = n.metadata.nlink.saturating_sub(1);
            });
        }

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let _t = self.stats.rmdir.enter();
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let parent_id = match self.inodes.lookup_ino(parent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let target_id = match self.graph.resolve(&parent_id, EdgeType::Child, name_str) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check it's a directory.
        if let Some(node) = self.graph.get_node(&target_id) {
            if node.node_type != NodeType::Directory {
                reply.error(libc::ENOTDIR);
                return;
            }
        } else {
            reply.error(libc::ENOENT);
            return;
        }

        // Check it's empty.
        if self.graph.edge_count(&target_id) > 0 {
            reply.error(libc::ENOTEMPTY);
            return;
        }

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlocks on parent directory and target directory.
        if let Some(other) = self.tlocks.touch(parent_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }
        if let Some(other) = self.tlocks.touch(target_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        // Capture node data for undo before removing.
        let target_node_data = self
            .graph
            .get_node(&target_id)
            .and_then(|n| bincode::serialize(&n).ok())
            .unwrap_or_default();

        // Log edge removal, node removal, and content removal.
        if txn_mgr
            .log_remove_edge(
                txn_id,
                parent_id,
                target_id,
                EdgeType::Child,
                name_str.to_string(),
            )
            .is_err()
            || txn_mgr
                .log_remove_node(txn_id, target_id, target_node_data)
                .is_err()
            || txn_mgr
                .log_content_change(txn_id, target_id, Some(EMPTY_HASH), None)
                .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Apply to in-memory state.
        self.graph
            .remove_edge(&parent_id, EdgeType::Child, name_str);
        self.graph.remove_node(&target_id);
        self.store.remove_object(&target_id);
        self.inodes.remove_id(&target_id);

        // Decrement parent nlink.
        self.graph.update_node(&parent_id, |n| {
            n.metadata.nlink = n.metadata.nlink.saturating_sub(1);
        });

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let _t = self.stats.readdir.enter();
        let node_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let mut entries: Vec<(Ino, FileType, String)> = Vec::new();

        // . and ..
        entries.push((ino, FileType::Directory, ".".to_string()));
        // For .., find parent. If root, parent is self.
        let parent_ino = {
            let reverse = self.graph.reverse_lookup(&node_id);
            if let Some((parent_id, _, _)) = reverse.first() {
                self.inodes.get_or_assign(*parent_id)
            } else {
                ino // root
            }
        };
        entries.push((parent_ino, FileType::Directory, "..".to_string()));

        // Children — only show Child and Hardlink edges in POSIX readdir.
        let neighbors = self.graph.neighbors(&node_id);
        for (edge_type, label, target_id) in neighbors {
            if edge_type != EdgeType::Child && edge_type != EdgeType::Hardlink {
                continue;
            }
            if let Some(target_node) = self.graph.get_node(&target_id) {
                let target_ino = self.inodes.get_or_assign(target_id);
                let ft = match target_node.node_type {
                    NodeType::File => FileType::RegularFile,
                    NodeType::Directory => FileType::Directory,
                    NodeType::Symlink => FileType::Symlink,
                    NodeType::TagAnchor => continue, // not visible in POSIX tree
                };
                entries.push((target_ino, ft, label));
            }
        }

        for (i, (ino, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when the buffer is full.
            if reply.add(*ino, (i + 1) as i64, *ft, name) {
                break;
            }
        }

        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let _t = self.stats.read.enter();
        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match self.store.get_content(&obj_id) {
            Ok(data) => {
                let offset = offset as usize;
                if offset >= data.len() {
                    reply.data(&[]);
                } else {
                    let end = std::cmp::min(offset + size as usize, data.len());
                    self.stats.read_bytes.fetch_add((end - offset) as u64, Relaxed);
                    reply.data(&data[offset..end]);
                }
            }
            Err(_) => {
                reply.data(&[]);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let _t = self.stats.write.enter();
        self.stats.write_bytes.fetch_add(data.len() as u64, Relaxed);
        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let old_hash = self.store.get_hash(&obj_id);

        // Read existing content, apply write.
        let mut content = match self.store.get_content(&obj_id) {
            Ok(d) => d,
            Err(_) => Vec::new(),
        };

        let offset = offset as usize;
        let end = offset + data.len();
        if end > content.len() {
            content.resize(end, 0);
        }
        content[offset..end].copy_from_slice(data);

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Store new content (or buffer in NO-STEAL mode).
        let new_hash = if self.no_steal {
            match self.store.put_object_buffered(obj_id, &content) {
                Ok(h) => {
                    // Stash the data in pending_writes for flush at commit.
                    txn_mgr.active.get_mut(&txn_id).unwrap()
                        .pending_writes.push((obj_id, content.clone()));
                    h
                }
                Err(_) => {
                    let _ = txn_mgr.abort_txn(txn_id);
                    reply.error(libc::EIO);
                    return;
                }
            }
        } else {
            match self.store.put_object(obj_id, &content) {
                Ok(h) => h,
                Err(_) => {
                    let _ = txn_mgr.abort_txn(txn_id);
                    reply.error(libc::EIO);
                    return;
                }
            }
        };

        // Log the content change.
        if let Err(_) = txn_mgr.log_content_change(txn_id, obj_id, old_hash, Some(new_hash)) {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Update metadata.
        self.graph.update_node(&obj_id, |node| {
            node.metadata.size = content.len() as u64;
            node.content_hash = new_hash;
            let (sec, nsec) = NodeMetadata::now();
            node.metadata.mtime = sec;
            node.metadata.mtime_nsec = nsec;
            node.metadata.ctime = sec;
            node.metadata.ctime_nsec = nsec;
        });

        let pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        // NO-STEAL: flush buffered content writes to disk now that the
        // transaction is committed.
        if self.no_steal && !pending.is_empty() {
            if let Err(_) = self.store.flush_pending(&pending) {
                reply.error(libc::EIO);
                return;
            }
        }
        self.stats.txn_committed.fetch_add(1, Relaxed);

        reply.written(data.len() as u32);
    }

    fn link(
        &mut self,
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let _t = self.stats.link.enter();
        let name_str = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let target_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let parent_id = match self.inodes.lookup_ino(newparent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Can't hard-link directories.
        if let Some(node) = self.graph.get_node(&target_id) {
            if node.node_type == NodeType::Directory {
                reply.error(libc::EPERM);
                return;
            }
        }

        // Check if name already exists.
        if self
            .graph
            .resolve(&parent_id, EdgeType::Child, name_str)
            .is_some()
        {
            reply.error(libc::EEXIST);
            return;
        }

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlocks on parent directory and target file.
        if let Some(other) = self.tlocks.touch(parent_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }
        if let Some(other) = self.tlocks.touch(target_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        // Log the edge addition — this is the associative property.
        if txn_mgr
            .log_add_edge(
                txn_id,
                parent_id,
                target_id,
                EdgeType::Child,
                name_str.to_string(),
            )
            .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Apply to in-memory state.
        if let Err(_) = self.graph.add_edge(
            parent_id,
            target_id,
            EdgeType::Child,
            name_str.to_string(),
        ) {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Increment nlink.
        self.graph.update_node(&target_id, |n| {
            n.metadata.nlink += 1;
            let (sec, nsec) = NodeMetadata::now();
            n.metadata.ctime = sec;
            n.metadata.ctime_nsec = nsec;
        });

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        match self.graph.get_node(&target_id) {
            Some(node) => {
                let attr = self.node_to_attr(&node);
                reply.entry(&TTL, &attr, 0);
            }
            None => reply.error(libc::EIO),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let _t = self.stats.rename.enter();
        let old_name = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let new_name = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let old_parent_id = match self.inodes.lookup_ino(parent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let new_parent_id = match self.inodes.lookup_ino(newparent) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Find source.
        let target_id = match self.graph.resolve(&old_parent_id, EdgeType::Child, old_name) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlocks: source dir, target dir, source object.
        // Rename is the hottest op for EC merge — up to 4 objects locked.
        if let Some(other) = self.tlocks.touch(old_parent_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }
        if old_parent_id != new_parent_id {
            if let Some(other) = self.tlocks.touch(new_parent_id, txn_id) {
                txn_mgr.ec_merge(txn_id, other);
            }
        }
        if let Some(other) = self.tlocks.touch(target_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        // If destination exists, remove it inside the transaction.
        if let Some(existing_id) =
            self.graph.resolve(&new_parent_id, EdgeType::Child, new_name)
        {
            let existing_node_data = self
                .graph
                .get_node(&existing_id)
                .and_then(|n| bincode::serialize(&n).ok())
                .unwrap_or_default();
            let existing_hash = self.store.get_hash(&existing_id);

            if txn_mgr
                .log_remove_edge(
                    txn_id,
                    new_parent_id,
                    existing_id,
                    EdgeType::Child,
                    new_name.to_string(),
                )
                .is_err()
            {
                let _ = txn_mgr.abort_txn(txn_id);
                reply.error(libc::EIO);
                return;
            }

            self.graph
                .remove_edge(&new_parent_id, EdgeType::Child, new_name);

            let should_remove = self.graph.reverse_lookup(&existing_id).is_empty();
            if should_remove {
                if txn_mgr
                    .log_remove_node(txn_id, existing_id, existing_node_data)
                    .is_err()
                    || txn_mgr
                        .log_content_change(txn_id, existing_id, existing_hash, None)
                        .is_err()
                {
                    let _ = txn_mgr.abort_txn(txn_id);
                    reply.error(libc::EIO);
                    return;
                }
                self.graph.remove_node(&existing_id);
                self.store.remove_object(&existing_id);
                self.inodes.remove_id(&existing_id);
            }
        }

        // Log old edge removal, new edge addition.
        if txn_mgr
            .log_remove_edge(
                txn_id,
                old_parent_id,
                target_id,
                EdgeType::Child,
                old_name.to_string(),
            )
            .is_err()
            || txn_mgr
                .log_add_edge(
                    txn_id,
                    new_parent_id,
                    target_id,
                    EdgeType::Child,
                    new_name.to_string(),
                )
                .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Apply to in-memory state.
        self.graph
            .remove_edge(&old_parent_id, EdgeType::Child, old_name);
        if let Err(_) = self.graph.add_edge(
            new_parent_id,
            target_id,
            EdgeType::Child,
            new_name.to_string(),
        ) {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        // Update nlink if moving a directory between parents.
        if old_parent_id != new_parent_id {
            if let Some(node) = self.graph.get_node(&target_id) {
                if node.node_type == NodeType::Directory {
                    self.graph.update_node(&old_parent_id, |n| {
                        n.metadata.nlink = n.metadata.nlink.saturating_sub(1);
                    });
                    self.graph.update_node(&new_parent_id, |n| {
                        n.metadata.nlink += 1;
                    });
                }
            }
        }

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        self.stats.open.count.fetch_add(1, Relaxed);
        if self.inodes.lookup_ino(ino).is_some() {
            reply.opened(0, 0);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(
            0,     // blocks
            0,     // bfree
            0,     // bavail
            0,     // files
            0,     // ffree
            BLOCK_SIZE,  // bsize
            255,   // namelen
            BLOCK_SIZE,  // frsize
        );
    }

    /// Set an extended attribute — the gateway to graph edge creation.
    ///
    /// Supported namespaces:
    ///   user.assfs.tag.<name>  — tag this object (creates graph edge)
    fn setxattr(
        &mut self,
        _req: &Request,
        ino: u64,
        name: &OsStr,
        _value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let _t = self.stats.setxattr.enter();

        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        // Parse the assfs tag namespace.
        let tag_name = match name_str.strip_prefix("user.assfs.tag.") {
            Some(t) if !t.is_empty() => t,
            _ => {
                reply.error(libc::ENOTSUP);
                return;
            }
        };

        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check if this object already has this tag.
        let already_tagged = self
            .graph
            .resolve(&obj_id, EdgeType::Tag, tag_name)
            .is_some();

        // Honor XATTR_CREATE / XATTR_REPLACE flags.
        if flags & 0x1 != 0 && already_tagged {
            // XATTR_CREATE: fail if exists
            reply.error(libc::EEXIST);
            return;
        }
        if flags & 0x2 != 0 && !already_tagged {
            // XATTR_REPLACE: fail if not exists
            reply.error(libc::ENODATA);
            return;
        }
        if already_tagged {
            reply.ok();
            return;
        }

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlock on the tagged object (tag mutation).
        if let Some(other) = self.tlocks.touch(obj_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        // Find or create the tag anchor node.
        let anchor_id = if let Some(id) =
            self.graph
                .resolve(&self.tag_registry_id, EdgeType::Tag, tag_name)
        {
            id
        } else {
            // New tag — create anchor node and register it.
            let id = uuid::Uuid::new_v4();
            let node = GraphNode::new_tag_anchor(id);
            let node_data = bincode::serialize(&node).unwrap_or_default();

            if txn_mgr.log_add_node(txn_id, id, node_data).is_err()
                || txn_mgr
                    .log_add_edge(
                        txn_id,
                        self.tag_registry_id,
                        id,
                        EdgeType::Tag,
                        tag_name.to_string(),
                    )
                    .is_err()
            {
                let _ = txn_mgr.abort_txn(txn_id);
                reply.error(libc::EIO);
                return;
            }

            self.graph.add_node(node);
            if self
                .graph
                .add_edge(
                    self.tag_registry_id,
                    id,
                    EdgeType::Tag,
                    tag_name.to_string(),
                )
                .is_err()
            {
                let _ = txn_mgr.abort_txn(txn_id);
                reply.error(libc::EIO);
                return;
            }

            id
        };

        // Add Tag edge from object → tag anchor.
        if txn_mgr
            .log_add_edge(
                txn_id,
                obj_id,
                anchor_id,
                EdgeType::Tag,
                tag_name.to_string(),
            )
            .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        if self
            .graph
            .add_edge(obj_id, anchor_id, EdgeType::Tag, tag_name.to_string())
            .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        log::debug!("Tagged ino {} with '{}'", ino, tag_name);
        reply.ok();
    }

    /// Get an extended attribute.
    ///
    /// Supported names:
    ///   user.assfs.tag.<name>  — empty if tagged, ENODATA if not
    ///   user.assfs.id          — object UUID as string
    ///   user.assfs.refs        — incoming references (the backpointer!)
    fn getxattr(
        &mut self,
        _req: &Request,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let _t = self.stats.getxattr.enter();

        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let value: Vec<u8> = if let Some(tag_name) = name_str.strip_prefix("user.assfs.tag.") {
            if self
                .graph
                .resolve(&obj_id, EdgeType::Tag, tag_name)
                .is_some()
            {
                Vec::new() // tag exists, value is empty (boolean tag)
            } else {
                reply.error(libc::ENODATA);
                return;
            }
        } else if name_str == "user.assfs.id" {
            obj_id.to_string().into_bytes()
        } else if name_str == "user.assfs.refs" {
            // The backpointer: who references this object?
            let refs = self.graph.reverse_lookup(&obj_id);
            let mut lines = Vec::new();
            for (source_id, edge_type, label) in &refs {
                lines.push(format!("{:?}:{}:{}", edge_type, source_id, label));
            }
            lines.join("\n").into_bytes()
        } else {
            reply.error(libc::ENODATA);
            return;
        };

        if size == 0 {
            reply.size(value.len() as u32);
        } else if value.len() <= size as usize {
            reply.data(&value);
        } else {
            reply.error(libc::ERANGE);
        }
    }

    /// List extended attribute names.
    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        let _t = self.stats.listxattr.enter();

        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let mut names: Vec<String> = Vec::new();

        // Built-in graph attributes (always present).
        names.push("user.assfs.id".to_string());
        names.push("user.assfs.refs".to_string());

        // Tag edges — outgoing edges of type Tag.
        let neighbors = self.graph.neighbors(&obj_id);
        for (edge_type, label, _target) in &neighbors {
            if *edge_type == EdgeType::Tag {
                names.push(format!("user.assfs.tag.{}", label));
            }
        }

        // Format as null-separated list with trailing null.
        let mut data = Vec::new();
        for name in &names {
            data.extend_from_slice(name.as_bytes());
            data.push(0);
        }

        if size == 0 {
            reply.size(data.len() as u32);
        } else if data.len() <= size as usize {
            reply.data(&data);
        } else {
            reply.error(libc::ERANGE);
        }
    }

    /// Remove an extended attribute — removes a tag edge.
    fn removexattr(&mut self, _req: &Request, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        let _t = self.stats.removexattr.enter();

        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let tag_name = match name_str.strip_prefix("user.assfs.tag.") {
            Some(t) if !t.is_empty() => t,
            _ => {
                // Can't remove built-in attrs.
                reply.error(libc::ENOTSUP);
                return;
            }
        };

        let obj_id = match self.inodes.lookup_ino(ino) {
            Some(id) => id,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Verify the tag edge exists.
        let anchor_id = match self.graph.resolve(&obj_id, EdgeType::Tag, tag_name) {
            Some(id) => id,
            None => {
                reply.error(libc::ENODATA);
                return;
            }
        };

        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match self.start_txn_or_checkpoint(&mut txn_mgr) {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Touch tlock on the object being untagged.
        if let Some(other) = self.tlocks.touch(obj_id, txn_id) {
            txn_mgr.ec_merge(txn_id, other);
        }

        if txn_mgr
            .log_remove_edge(
                txn_id,
                obj_id,
                anchor_id,
                EdgeType::Tag,
                tag_name.to_string(),
            )
            .is_err()
        {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }

        self.graph.remove_edge(&obj_id, EdgeType::Tag, tag_name);

        let _pending = match txn_mgr.commit_txn(txn_id) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        self.stats.txn_committed.fetch_add(1, Relaxed);

        log::debug!("Untagged ino {} from '{}'", ino, tag_name);
        reply.ok();
    }

    fn destroy(&mut self) {
        log::info!("AssFS: unmounting, flushing state...");
        // Checkpoint: flush all state, then truncate the log.
        // This reclaims log space and ensures a clean restart.
        if let Err(e) = self.flush_all() {
            log::error!("Error flushing state on unmount: {}", e);
        }
        {
            let mut txn_mgr = self.txn_mgr.lock().unwrap();
            // Capture stats BEFORE checkpoint truncates the log timing data.
            self.stats.print_summary(Some(&txn_mgr.log.timings));
            if let Some(ref path) = self.stats_file {
                if let Err(e) = self.stats.write_json(path, Some(&txn_mgr.log.timings)) {
                    log::error!("Error writing stats file: {}", e);
                }
            }
            // Truncate log and reset admission control.
            if let Err(e) = txn_mgr.checkpoint() {
                log::error!("Error during checkpoint on unmount: {}", e);
            }
        }
        // Directory mutation frequency distribution.
        let summary = self.tlocks.access_summary();
        if summary.total_touches > 0 {
            eprintln!("\n=== Directory Mutation Distribution ===");
            eprint!("{}", summary);
        }
    }
}
