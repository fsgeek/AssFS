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
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};

use crate::graph::node::{GraphNode, NodeMetadata};
use crate::graph::operations::GraphNamespace;
use crate::projection::{InodeTable, Ino};
use crate::store::content::ContentStore;
use crate::txn::recovery;
use crate::txn::transaction::TransactionManager;
use crate::{EdgeType, NodeType, ObjectId, EMPTY_HASH};

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4096;

/// The AssFS filesystem — ties all layers together.
pub struct AssFs {
    pub graph: GraphNamespace,
    pub store: ContentStore,
    pub txn_mgr: std::sync::Mutex<TransactionManager>,
    pub inodes: InodeTable,
    pub root_id: ObjectId,
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

        Ok(Self {
            graph,
            store,
            txn_mgr: std::sync::Mutex::new(txn_mgr),
            inodes,
            root_id,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// Flush all state to disk.
    pub fn flush_all(&self) -> io::Result<()> {
        self.store.flush_index()?;
        self.graph.flush()?;
        Ok(())
    }

    /// Convert a GraphNode to FUSE FileAttr.
    fn node_to_attr(&self, node: &GraphNode) -> FileAttr {
        let ino = self.inodes.get_or_assign(node.id);
        let kind = match node.node_type {
            NodeType::File => FileType::RegularFile,
            NodeType::Directory => FileType::Directory,
            NodeType::Symlink => FileType::Symlink,
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

        // Start transaction.
        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Create new file node.
        let file_id = uuid::Uuid::new_v4();
        let file_node = GraphNode::new_file(file_id, req.uid(), req.gid(), mode);
        let node_data = bincode::serialize(&file_node).unwrap_or_default();

        // Log node addition, edge addition, and content registration.
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
            reply.error(libc::EIO);
            return;
        }

        // Apply to in-memory state.
        self.graph.add_node(file_node.clone());
        if let Err(_) = self.graph.add_edge(
            parent_id,
            file_id,
            EdgeType::Child,
            name_str.to_string(),
        ) {
            let _ = txn_mgr.abort_txn(txn_id);
            reply.error(libc::EIO);
            return;
        }
        self.store.register_empty(file_id);

        // Commit.
        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

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
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

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

        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

        let attr = self.node_to_attr(&dir_node);
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
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
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

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

        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
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
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

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

        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

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

        // Children.
        let neighbors = self.graph.neighbors(&node_id);
        for (_edge_type, label, target_id) in neighbors {
            if let Some(target_node) = self.graph.get_node(&target_id) {
                let target_ino = self.inodes.get_or_assign(target_id);
                let ft = match target_node.node_type {
                    NodeType::File => FileType::RegularFile,
                    NodeType::Directory => FileType::Directory,
                    NodeType::Symlink => FileType::Symlink,
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
                    reply.data(&data[offset..end]);
                }
            }
            Err(_) => {
                // No content yet (empty file).
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
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

        // Store new content.
        let new_hash = match self.store.put_object(obj_id, &content) {
            Ok(h) => h,
            Err(_) => {
                let _ = txn_mgr.abort_txn(txn_id);
                reply.error(libc::EIO);
                return;
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

        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

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
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

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

        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

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
        let txn_id = match txn_mgr.start_txn() {
            Ok(id) => id,
            Err(_) => {
                reply.error(libc::ENOSPC);
                return;
            }
        };

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

        if let Err(_) = txn_mgr.commit_txn(txn_id) {
            reply.error(libc::EIO);
            return;
        }

        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        // Verify the inode exists.
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

    fn destroy(&mut self) {
        log::info!("AssFS: unmounting, flushing state...");
        if let Err(e) = self.flush_all() {
            log::error!("Error flushing state on unmount: {}", e);
        }
    }
}
