//! fuse-vfs-fuse3 — blind bake-off implementation of the FUSE VFS contract
//! using the `fuse3` crate (async, inode-based `fuse3::raw::Filesystem`).
//!
//! Spec: docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md
//!
//! `fuse-vfs-fuse3 <mountpoint>` mounts a read-write in-memory filesystem
//! (initially empty) at `<mountpoint>`, runs in the foreground until the
//! filesystem is unmounted (e.g. `fusermount3 -u`), then exits 0. Fatal mount
//! errors exit non-zero with a message on stderr.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuse3::raw::prelude::*;
use fuse3::{Errno, Inode, MountOptions, Result, Timestamp};
use futures_util::stream::{self, Iter};

const TTL: Duration = Duration::from_secs(1);
const BLKSIZE: u32 = 4096;
const ROOT_INO: u64 = 1;

fn now() -> Timestamp {
    SystemTime::now().into()
}

fn errno(code: i32) -> Errno {
    Errno::from(code)
}

// ---------------------------------------------------------------------------
// The in-memory store: a hand-rolled tree of directories and regular files.
// ---------------------------------------------------------------------------

enum Content {
    File(Vec<u8>),
    Dir(BTreeMap<OsString, u64>),
}

struct Node {
    parent: u64, // for ".." (root points at itself)
    perm: u16,
    atime: Timestamp,
    mtime: Timestamp,
    ctime: Timestamp,
    /// Open file handles referencing this node.
    open_count: u32,
    /// Unlinked while open: keep contents alive until the last release.
    unlinked: bool,
    content: Content,
}

impl Node {
    fn new_dir(parent: u64, perm: u16) -> Self {
        let t = now();
        Node {
            parent,
            perm,
            atime: t,
            mtime: t,
            ctime: t,
            open_count: 0,
            unlinked: false,
            content: Content::Dir(BTreeMap::new()),
        }
    }

    fn new_file(parent: u64, perm: u16) -> Self {
        let t = now();
        Node {
            parent,
            perm,
            atime: t,
            mtime: t,
            ctime: t,
            open_count: 0,
            unlinked: false,
            content: Content::File(Vec::new()),
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.content, Content::Dir(_))
    }
}

struct Store {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
    uid: u32,
    gid: u32,
}

impl Store {
    fn new(uid: u32, gid: u32) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(ROOT_INO, Node::new_dir(ROOT_INO, 0o755));
        Store {
            nodes,
            next_ino: ROOT_INO + 1,
            uid,
            gid,
        }
    }

    fn alloc_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn node(&self, ino: u64) -> Result<&Node> {
        self.nodes.get(&ino).ok_or_else(|| errno(libc::ENOENT))
    }

    fn node_mut(&mut self, ino: u64) -> Result<&mut Node> {
        self.nodes.get_mut(&ino).ok_or_else(|| errno(libc::ENOENT))
    }

    fn dir_children(&self, ino: u64) -> Result<&BTreeMap<OsString, u64>> {
        match &self.node(ino)?.content {
            Content::Dir(children) => Ok(children),
            Content::File(_) => Err(errno(libc::ENOTDIR)),
        }
    }

    fn dir_children_mut(&mut self, ino: u64) -> Result<&mut BTreeMap<OsString, u64>> {
        match &mut self.node_mut(ino)?.content {
            Content::Dir(children) => Ok(children),
            Content::File(_) => Err(errno(libc::ENOTDIR)),
        }
    }

    fn attr(&self, ino: u64) -> Result<FileAttr> {
        let node = self.node(ino)?;
        let (kind, size, nlink) = match &node.content {
            Content::File(data) => (FileType::RegularFile, data.len() as u64, 1u32),
            Content::Dir(children) => {
                // POSIX nlink for a directory: 2 (self + ".") plus one ".."
                // per child subdirectory.
                let subdirs = children
                    .values()
                    .filter(|c| self.nodes.get(c).map(|n| n.is_dir()).unwrap_or(false))
                    .count() as u32;
                (FileType::Directory, BLKSIZE as u64, 2 + subdirs)
            }
        };
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
            kind,
            perm: node.perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLKSIZE,
        })
    }

    fn touch_mtime(&mut self, ino: u64) {
        if let Ok(node) = self.node_mut(ino) {
            let t = now();
            node.mtime = t;
            node.ctime = t;
        }
    }

    /// Remove a node's contents once no directory entry and no open handle
    /// references it any more (POSIX unlink-while-open semantics).
    fn drop_if_unreferenced(&mut self, ino: u64) {
        if let Some(node) = self.nodes.get(&ino) {
            if node.unlinked && node.open_count == 0 {
                self.nodes.remove(&ino);
            }
        }
    }

    /// Detach a node from the namespace (after its dirent was removed).
    fn detach(&mut self, ino: u64) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.unlinked = true;
        }
        self.drop_if_unreferenced(ino);
    }

    /// Create a new child under `parent`, failing with EEXIST if the name is
    /// taken. Returns the new inode.
    fn create_child(&mut self, parent: u64, name: &OsStr, node: Node) -> Result<u64> {
        if self.dir_children(parent)?.contains_key(name) {
            return Err(errno(libc::EEXIST));
        }
        let ino = self.alloc_ino();
        self.nodes.insert(ino, node);
        self.dir_children_mut(parent)
            .expect("parent checked above")
            .insert(name.to_os_string(), ino);
        self.touch_mtime(parent);
        Ok(ino)
    }

    /// True if `ino` is `ancestor` or lies beneath it (for rename cycle checks).
    fn is_or_below(&self, ino: u64, ancestor: u64) -> bool {
        let mut cur = ino;
        loop {
            if cur == ancestor {
                return true;
            }
            match self.nodes.get(&cur) {
                Some(node) if cur != ROOT_INO => cur = node.parent,
                _ => return false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The FUSE filesystem: fuse3::raw::Filesystem over the store.
// ---------------------------------------------------------------------------

struct Vfs {
    store: Mutex<Store>,
    next_fh: AtomicU64,
}

impl Vfs {
    fn new(uid: u32, gid: u32) -> Self {
        Vfs {
            store: Mutex::new(Store::new(uid, gid)),
            next_fh: AtomicU64::new(1),
        }
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn entry(attr: FileAttr) -> ReplyEntry {
        ReplyEntry {
            ttl: TTL,
            attr,
            generation: 0,
        }
    }

    /// Shared rename logic; `no_replace` implements RENAME_NOREPLACE.
    fn do_rename(
        &self,
        parent: Inode,
        name: &OsStr,
        new_parent: Inode,
        new_name: &OsStr,
        no_replace: bool,
    ) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        let src_ino = *store
            .dir_children(parent)?
            .get(name)
            .ok_or_else(|| errno(libc::ENOENT))?;
        let target = store.dir_children(new_parent)?.get(new_name).copied();

        if let Some(target_ino) = target {
            if target_ino == src_ino {
                return Ok(()); // renaming a name onto itself is a no-op
            }
            if no_replace {
                return Err(errno(libc::EEXIST));
            }
            let src_is_dir = store.node(src_ino)?.is_dir();
            let dst_is_dir = store.node(target_ino)?.is_dir();
            match (src_is_dir, dst_is_dir) {
                (true, false) => return Err(errno(libc::ENOTDIR)),
                (false, true) => return Err(errno(libc::EISDIR)),
                (true, true) => {
                    if !store.dir_children(target_ino)?.is_empty() {
                        return Err(errno(libc::ENOTEMPTY));
                    }
                }
                (false, false) => {}
            }
        }

        // A directory must not be moved into itself or its own subtree.
        if store.node(src_ino)?.is_dir() && store.is_or_below(new_parent, src_ino) {
            return Err(errno(libc::EINVAL));
        }

        if let Some(target_ino) = target {
            store.dir_children_mut(new_parent)?.remove(new_name);
            store.detach(target_ino);
        }
        store.dir_children_mut(parent)?.remove(name);
        store
            .dir_children_mut(new_parent)?
            .insert(new_name.to_os_string(), src_ino);
        if let Ok(node) = store.node_mut(src_ino) {
            node.parent = new_parent;
            node.ctime = now();
        }
        store.touch_mtime(parent);
        if new_parent != parent {
            store.touch_mtime(new_parent);
        }
        Ok(())
    }

    /// Directory listing shared by readdir and readdirplus: ".", "..", then
    /// children in name order; the listing resumes from the requested offset.
    fn list_dir(&self, parent: Inode, offset: usize) -> Result<Vec<(u64, FileAttr, OsString)>> {
        let store = self.store.lock().unwrap();
        let node = store.node(parent)?;
        if !node.is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        let mut entries: Vec<(u64, FileAttr, OsString)> = Vec::new();
        entries.push((parent, store.attr(parent)?, OsString::from(".")));
        entries.push((node.parent, store.attr(node.parent)?, OsString::from("..")));
        for (name, child_ino) in store.dir_children(parent)? {
            // A child missing from the node map would be a store bug; surface
            // it as EIO rather than silently skipping the entry.
            let attr = store.attr(*child_ino).map_err(|_| errno(libc::EIO))?;
            entries.push((*child_ino, attr, name.clone()));
        }
        Ok(entries.into_iter().skip(offset).collect())
    }
}

type DirStream = Iter<std::vec::IntoIter<Result<DirectoryEntry>>>;
type DirPlusStream = Iter<std::vec::IntoIter<Result<DirectoryEntryPlus>>>;

impl Filesystem for Vfs {
    async fn init(&self, _req: Request) -> Result<ReplyInit> {
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1024 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {}

    async fn lookup(&self, _req: Request, parent: Inode, name: &OsStr) -> Result<ReplyEntry> {
        let store = self.store.lock().unwrap();
        let ino = *store
            .dir_children(parent)?
            .get(name)
            .ok_or_else(|| errno(libc::ENOENT))?;
        Ok(Self::entry(store.attr(ino)?))
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: Inode,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Result<ReplyAttr> {
        let store = self.store.lock().unwrap();
        Ok(ReplyAttr {
            ttl: TTL,
            attr: store.attr(inode)?,
        })
    }

    async fn setattr(
        &self,
        _req: Request,
        inode: Inode,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> Result<ReplyAttr> {
        let mut store = self.store.lock().unwrap();
        {
            let node = store.node_mut(inode)?;
            if let Some(size) = set_attr.size {
                match &mut node.content {
                    Content::File(data) => {
                        data.resize(size as usize, 0);
                        node.mtime = now();
                    }
                    Content::Dir(_) => return Err(errno(libc::EISDIR)),
                }
            }
            if let Some(mode) = set_attr.mode {
                node.perm = (mode & 0o7777) as u16;
            }
            if let Some(atime) = set_attr.atime {
                node.atime = atime;
            }
            if let Some(mtime) = set_attr.mtime {
                node.mtime = mtime;
            }
            node.ctime = set_attr.ctime.unwrap_or_else(now);
            // uid/gid changes accepted as no-ops: everything is reported as
            // the mounting user (spec: mounting user with sane modes).
        }
        Ok(ReplyAttr {
            ttl: TTL,
            attr: store.attr(inode)?,
        })
    }

    async fn mknod(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        mode: u32,
        _rdev: u32,
    ) -> Result<ReplyEntry> {
        let file_type = mode & libc::S_IFMT;
        if file_type != libc::S_IFREG && file_type != 0 {
            return Err(errno(libc::EPERM)); // only regular files in v1
        }
        let mut store = self.store.lock().unwrap();
        let perm = (mode & 0o7777) as u16;
        let ino = store.create_child(parent, name, Node::new_file(parent, perm))?;
        Ok(Self::entry(store.attr(ino)?))
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        mode: u32,
        _umask: u32,
    ) -> Result<ReplyEntry> {
        let mut store = self.store.lock().unwrap();
        let perm = (mode & 0o7777) as u16;
        let ino = store.create_child(parent, name, Node::new_dir(parent, perm))?;
        Ok(Self::entry(store.attr(ino)?))
    }

    async fn unlink(&self, _req: Request, parent: Inode, name: &OsStr) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        let ino = *store
            .dir_children(parent)?
            .get(name)
            .ok_or_else(|| errno(libc::ENOENT))?;
        if store.node(ino)?.is_dir() {
            return Err(errno(libc::EISDIR));
        }
        store.dir_children_mut(parent)?.remove(name);
        store.detach(ino);
        store.touch_mtime(parent);
        Ok(())
    }

    async fn rmdir(&self, _req: Request, parent: Inode, name: &OsStr) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        let ino = *store
            .dir_children(parent)?
            .get(name)
            .ok_or_else(|| errno(libc::ENOENT))?;
        if !store.node(ino)?.is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        if !store.dir_children(ino)?.is_empty() {
            return Err(errno(libc::ENOTEMPTY));
        }
        store.dir_children_mut(parent)?.remove(name);
        store.detach(ino);
        store.touch_mtime(parent);
        Ok(())
    }

    async fn rename(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        new_parent: Inode,
        new_name: &OsStr,
    ) -> Result<()> {
        self.do_rename(parent, name, new_parent, new_name, false)
    }

    async fn rename2(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        new_parent: Inode,
        new_name: &OsStr,
        flags: u32,
    ) -> Result<()> {
        const RENAME_NOREPLACE: u32 = 1;
        match flags {
            0 => self.do_rename(parent, name, new_parent, new_name, false),
            RENAME_NOREPLACE => self.do_rename(parent, name, new_parent, new_name, true),
            _ => Err(errno(libc::EINVAL)), // RENAME_EXCHANGE etc. unsupported
        }
    }

    async fn open(&self, _req: Request, inode: Inode, flags: u32) -> Result<ReplyOpen> {
        let mut store = self.store.lock().unwrap();
        let node = store.node_mut(inode)?;
        // FUSE_ATOMIC_O_TRUNC is negotiated, so O_TRUNC arrives here rather
        // than as a separate setattr.
        if flags & (libc::O_TRUNC as u32) != 0 {
            match &mut node.content {
                Content::File(data) => {
                    data.clear();
                    let t = now();
                    node.mtime = t;
                    node.ctime = t;
                }
                Content::Dir(_) => return Err(errno(libc::EISDIR)),
            }
        }
        node.open_count += 1;
        Ok(ReplyOpen {
            fh: self.alloc_fh(),
            flags: 0,
        })
    }

    async fn create(
        &self,
        _req: Request,
        parent: Inode,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> Result<ReplyCreated> {
        let mut store = self.store.lock().unwrap();
        let existing = store.dir_children(parent)?.get(name).copied();
        let ino = match existing {
            Some(ino) => {
                if flags & (libc::O_EXCL as u32) != 0 {
                    return Err(errno(libc::EEXIST));
                }
                let node = store.node_mut(ino)?;
                match &mut node.content {
                    Content::File(data) => {
                        if flags & (libc::O_TRUNC as u32) != 0 {
                            data.clear();
                            node.mtime = now();
                        }
                    }
                    Content::Dir(_) => return Err(errno(libc::EISDIR)),
                }
                ino
            }
            None => {
                let perm = (mode & 0o7777) as u16;
                store.create_child(parent, name, Node::new_file(parent, perm))?
            }
        };
        store.node_mut(ino)?.open_count += 1;
        let attr = store.attr(ino)?;
        Ok(ReplyCreated {
            ttl: TTL,
            attr,
            generation: 0,
            fh: self.alloc_fh(),
            flags: 0,
        })
    }

    async fn read(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<ReplyData> {
        let store = self.store.lock().unwrap();
        let data = match &store.node(inode)?.content {
            Content::File(data) => data,
            Content::Dir(_) => return Err(errno(libc::EISDIR)),
        };
        let start = (offset as usize).min(data.len());
        let end = (start + size as usize).min(data.len());
        Ok(ReplyData {
            data: data[start..end].to_vec().into(),
        })
    }

    async fn write(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> Result<ReplyWrite> {
        let mut store = self.store.lock().unwrap();
        let node = store.node_mut(inode)?;
        let contents = match &mut node.content {
            Content::File(contents) => contents,
            Content::Dir(_) => return Err(errno(libc::EISDIR)),
        };
        let offset = offset as usize;
        let end = offset + data.len();
        if end > contents.len() {
            contents.resize(end, 0); // sparse gap (if any) reads back as zeros
        }
        contents[offset..end].copy_from_slice(data);
        let t = now();
        node.mtime = t;
        node.ctime = t;
        Ok(ReplyWrite {
            written: data.len() as u32,
        })
    }

    async fn statfs(&self, _req: Request, _inode: Inode) -> Result<ReplyStatFs> {
        // Plausible non-zero totals: some tools divide by them (spec).
        let store = self.store.lock().unwrap();
        let used_blocks: u64 = store
            .nodes
            .values()
            .map(|n| match &n.content {
                Content::File(d) => (d.len() as u64).div_ceil(BLKSIZE as u64),
                Content::Dir(_) => 1,
            })
            .sum();
        let total_blocks: u64 = 4 * 1024 * 1024; // 16 GiB of 4 KiB blocks
        let free = total_blocks.saturating_sub(used_blocks);
        let files = store.nodes.len() as u64;
        Ok(ReplyStatFs {
            blocks: total_blocks,
            bfree: free,
            bavail: free,
            files: 1024 * 1024,
            ffree: (1024 * 1024u64).saturating_sub(files),
            bsize: BLKSIZE,
            namelen: 255,
            frsize: BLKSIZE,
        })
    }

    async fn release(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        if let Ok(node) = store.node_mut(inode) {
            node.open_count = node.open_count.saturating_sub(1);
            store.drop_if_unreferenced(inode);
        }
        Ok(())
    }

    async fn fsync(&self, _req: Request, _inode: Inode, _fh: u64, _datasync: bool) -> Result<()> {
        Ok(()) // in-memory: nothing to sync
    }

    async fn flush(&self, _req: Request, _inode: Inode, _fh: u64, _lock_owner: u64) -> Result<()> {
        Ok(())
    }

    async fn access(&self, _req: Request, inode: Inode, _mask: u32) -> Result<()> {
        // default_permissions is set, so the kernel checks modes itself; be
        // permissive if it still asks.
        let store = self.store.lock().unwrap();
        store.node(inode).map(|_| ())
    }

    async fn opendir(&self, _req: Request, inode: Inode, _flags: u32) -> Result<ReplyOpen> {
        let store = self.store.lock().unwrap();
        if !store.node(inode)?.is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        Ok(ReplyOpen { fh: 0, flags: 0 }) // stateless directory IO
    }

    #[allow(refining_impl_trait)] // concrete Stream type instead of the trait's opaque one
    async fn readdir(
        &self,
        _req: Request,
        parent: Inode,
        _fh: u64,
        offset: i64,
    ) -> Result<ReplyDirectory<DirStream>> {
        let entries = self.list_dir(parent, offset as usize)?;
        let base = offset;
        let entries: Vec<Result<DirectoryEntry>> = entries
            .into_iter()
            .enumerate()
            .map(|(i, (ino, attr, name))| {
                Ok(DirectoryEntry {
                    inode: ino,
                    kind: attr.kind,
                    name,
                    offset: base + i as i64 + 1,
                })
            })
            .collect();
        Ok(ReplyDirectory {
            entries: stream::iter(entries),
        })
    }

    #[allow(refining_impl_trait)] // concrete Stream type instead of the trait's opaque one
    async fn readdirplus(
        &self,
        _req: Request,
        parent: Inode,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Result<ReplyDirectoryPlus<DirPlusStream>> {
        let entries = self.list_dir(parent, offset as usize)?;
        let base = offset as i64;
        let entries: Vec<Result<DirectoryEntryPlus>> = entries
            .into_iter()
            .enumerate()
            .map(|(i, (ino, attr, name))| {
                Ok(DirectoryEntryPlus {
                    inode: ino,
                    generation: 0,
                    kind: attr.kind,
                    name,
                    offset: base + i as i64 + 1,
                    attr,
                    entry_ttl: TTL,
                    attr_ttl: TTL,
                })
            })
            .collect();
        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries),
        })
    }

    async fn fsyncdir(&self, _req: Request, _inode: Inode, _fh: u64, _datasync: bool) -> Result<()> {
        Ok(())
    }

    async fn fallocate(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Result<()> {
        const KEEP_SIZE: u32 = libc::FALLOC_FL_KEEP_SIZE as u32;
        let mut store = self.store.lock().unwrap();
        let node = store.node_mut(inode)?;
        let data = match &mut node.content {
            Content::File(data) => data,
            Content::Dir(_) => return Err(errno(libc::EISDIR)),
        };
        match mode {
            0 => {
                let end = (offset + length) as usize;
                if end > data.len() {
                    data.resize(end, 0);
                    node.mtime = now();
                }
                Ok(())
            }
            KEEP_SIZE => Ok(()), // no preallocation needed in memory
            _ => Err(errno(libc::EOPNOTSUPP)),
        }
    }
}

// ---------------------------------------------------------------------------
// main: mount at argv[1], drive the session in the foreground, exit 0 on
// unmount.
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mountpoint = match (args.next(), args.next()) {
        (Some(mp), None) => mp,
        _ => {
            eprintln!("usage: fuse-vfs-fuse3 <mountpoint>");
            return ExitCode::from(2);
        }
    };

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("fuse-vfs-fuse3: failed to start tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    let result = runtime.block_on(async move {
        let mut mount_options = MountOptions::default();
        mount_options
            .fs_name("fuse-vfs-fuse3")
            .uid(uid)
            .gid(gid)
            .default_permissions(true);

        // Unprivileged mount: fuse3 execs the setuid `fusermount3` binary; no
        // libfuse is linked (the crate speaks the FUSE protocol natively).
        let handle = Session::new(mount_options)
            .mount_with_unprivileged(Vfs::new(uid, gid), &mountpoint)
            .await?;

        // Foreground until unmounted (fusermount3 -u), then the session ends.
        handle.await
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("fuse-vfs-fuse3: mount failed: {err}");
            ExitCode::FAILURE
        }
    }
}
