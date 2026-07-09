//! fuse-vfs-fuse-backend-rs — in-memory read-write VFS served over FUSE via
//! the `fuse-backend-rs` crate (rust-vmm lineage), for the FUSE VFS bake-off.
//!
//! Spec: docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md
//!
//! Shape of the crate: `fuse-backend-rs` is a low-level FUSE *transport* plus a
//! chromeos/virtiofs-style `FileSystem` callback trait. There is no built-in
//! "run" loop for external filesystems: you create a `FuseSession` (which
//! mounts — direct mount(2), falling back to setuid fusermount3 on EPERM),
//! pull `FuseChannel`s off it, and drive `Server::handle_message` yourself on
//! however many threads you want. Everything here is sync; the async story is
//! a separate tokio-uring-based `async-io` feature.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CStr;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse_backend_rs::abi::fuse_abi::{stat64, statvfs64, CreateIn};
use fuse_backend_rs::api::filesystem::{
    Context, DirEntry, Entry, FileSystem, FsOptions, OpenOptions, SetattrValid, ZeroCopyReader,
    ZeroCopyWriter, ROOT_ID,
};
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::transport::{FuseChannel, FuseSession};

const FSNAME: &str = "fuse-vfs-fuse-backend-rs";
/// The kernel is the only accessor of this filesystem (nothing mutates the
/// tree behind the mount), so cached attrs/entries can live essentially
/// forever.
const TTL: Duration = Duration::from_secs(86400);
const BLKSIZE: u64 = 4096;
const NUM_CHANNELS: usize = 4;

fn err(no: i32) -> io::Error {
    io::Error::from_raw_os_error(no)
}

fn now() -> (i64, i64) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i64),
        Err(_) => (0, 0),
    }
}

// ---------------------------------------------------------------------------
// The in-memory tree
// ---------------------------------------------------------------------------

enum Body {
    File(Vec<u8>),
    Dir(BTreeMap<Vec<u8>, u64>), // name -> ino
}

struct Node {
    parent: u64,
    mode: u32, // full st_mode including S_IFMT
    nlink: u32,
    uid: u32,
    gid: u32,
    atime: (i64, i64),
    mtime: (i64, i64),
    ctime: (i64, i64),
    /// Kernel lookup count (FORGET bookkeeping). Atomic so `lookup` can run
    /// under the read lock.
    nlookup: AtomicU64,
    /// No longer reachable by name; drop from the table once nlookup hits 0.
    unlinked: bool,
    body: Body,
}

impl Node {
    fn new_dir(parent: u64, mode: u32, uid: u32, gid: u32) -> Node {
        let t = now();
        Node {
            parent,
            mode: libc::S_IFDIR | (mode & 0o7777),
            nlink: 2,
            uid,
            gid,
            atime: t,
            mtime: t,
            ctime: t,
            nlookup: AtomicU64::new(0),
            unlinked: false,
            body: Body::Dir(BTreeMap::new()),
        }
    }

    fn new_file(parent: u64, mode: u32, uid: u32, gid: u32) -> Node {
        let t = now();
        Node {
            parent,
            mode: libc::S_IFREG | (mode & 0o7777),
            nlink: 1,
            uid,
            gid,
            atime: t,
            mtime: t,
            ctime: t,
            nlookup: AtomicU64::new(0),
            unlinked: false,
            body: Body::File(Vec::new()),
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.body, Body::Dir(_))
    }

    fn size(&self) -> u64 {
        match &self.body {
            Body::File(d) => d.len() as u64,
            Body::Dir(_) => BLKSIZE,
        }
    }

    fn dir(&self) -> io::Result<&BTreeMap<Vec<u8>, u64>> {
        match &self.body {
            Body::Dir(c) => Ok(c),
            Body::File(_) => Err(err(libc::ENOTDIR)),
        }
    }

    fn dir_mut(&mut self) -> io::Result<&mut BTreeMap<Vec<u8>, u64>> {
        match &mut self.body {
            Body::Dir(c) => Ok(c),
            Body::File(_) => Err(err(libc::ENOTDIR)),
        }
    }

    fn file_mut(&mut self) -> io::Result<&mut Vec<u8>> {
        match &mut self.body {
            Body::File(d) => Ok(d),
            Body::Dir(_) => Err(err(libc::EISDIR)),
        }
    }
}

struct State {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
}

impl State {
    fn get(&self, ino: u64) -> io::Result<&Node> {
        self.nodes.get(&ino).ok_or_else(|| err(libc::ENOENT))
    }

    fn get_mut(&mut self, ino: u64) -> io::Result<&mut Node> {
        self.nodes.get_mut(&ino).ok_or_else(|| err(libc::ENOENT))
    }

    fn alloc_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    /// Look up `name` in directory `parent`; ENOENT if missing.
    fn child_of(&self, parent: u64, name: &[u8]) -> io::Result<u64> {
        let p = self.get(parent)?;
        p.dir()?.get(name).copied().ok_or_else(|| err(libc::ENOENT))
    }

    /// Drop a node from the table if it is both unlinked and forgotten.
    fn maybe_reap(&mut self, ino: u64) {
        if let Some(n) = self.nodes.get(&ino) {
            if n.unlinked && n.nlookup.load(Ordering::Acquire) == 0 {
                self.nodes.remove(&ino);
            }
        }
    }
}

/// Snapshot of a directory taken at opendir time (rewinddir semantics), so
/// readdir offsets stay stable while the directory is concurrently modified.
struct DirSnapshot {
    entries: Vec<(u64, u32, Vec<u8>)>, // (ino, dtype, name)
}

pub struct MemFs {
    state: RwLock<State>,
    dir_handles: Mutex<HashMap<u64, Arc<DirSnapshot>>>,
    next_handle: AtomicU64,
    uid: u32,
    gid: u32,
}

impl MemFs {
    pub fn new() -> MemFs {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let mut nodes = HashMap::new();
        let root = Node::new_dir(ROOT_ID, 0o755, uid, gid);
        root.nlookup.store(1, Ordering::Release); // the root is never forgotten
        nodes.insert(ROOT_ID, root);
        MemFs {
            state: RwLock::new(State {
                nodes,
                next_ino: ROOT_ID + 1,
            }),
            dir_handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            uid,
            gid,
        }
    }

    fn attr_of(&self, ino: u64, n: &Node) -> stat64 {
        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = ino;
        st.st_mode = n.mode;
        st.st_nlink = n.nlink as libc::nlink_t;
        st.st_uid = n.uid;
        st.st_gid = n.gid;
        st.st_size = n.size() as i64;
        st.st_blksize = BLKSIZE as libc::blksize_t;
        st.st_blocks = ((n.size() + 511) / 512) as libc::blkcnt64_t;
        st.st_atime = n.atime.0;
        st.st_atime_nsec = n.atime.1;
        st.st_mtime = n.mtime.0;
        st.st_mtime_nsec = n.mtime.1;
        st.st_ctime = n.ctime.0;
        st.st_ctime_nsec = n.ctime.1;
        st
    }

    fn entry_of(&self, ino: u64, n: &Node) -> Entry {
        Entry {
            inode: ino,
            generation: 0,
            attr: self.attr_of(ino, n),
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        }
    }

    fn new_dir_handle(&self, snap: DirSnapshot) -> u64 {
        let h = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.dir_handles.lock().unwrap().insert(h, Arc::new(snap));
        h
    }

    /// Detach `ino` (already removed from its parent's children) and reap it
    /// if the kernel holds no reference.
    fn mark_unlinked(state: &mut State, ino: u64, is_dir: bool) {
        if let Some(n) = state.nodes.get_mut(&ino) {
            n.unlinked = true;
            n.nlink = if is_dir { 0 } else { n.nlink.saturating_sub(1) };
            n.ctime = now();
        }
        state.maybe_reap(ino);
    }
}

fn name_bytes(name: &CStr) -> io::Result<&[u8]> {
    let b = name.to_bytes();
    if b.is_empty() || b == b"." || b == b".." || b.contains(&b'/') {
        return Err(err(libc::EINVAL));
    }
    Ok(b)
}

fn dtype_of(n: &Node) -> u32 {
    if n.is_dir() {
        libc::DT_DIR as u32
    } else {
        libc::DT_REG as u32
    }
}

// ---------------------------------------------------------------------------
// FileSystem impl
// ---------------------------------------------------------------------------

impl FileSystem for MemFs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // The server intersects this with `capable`. MAX_PAGES lifts
        // max_write to 1 MiB (matters for the large-file rung);
        // ATOMIC_O_TRUNC folds truncate-on-open into open().
        Ok(capable
            & (FsOptions::ASYNC_READ
                | FsOptions::BIG_WRITES
                | FsOptions::MAX_PAGES
                | FsOptions::ATOMIC_O_TRUNC
                | FsOptions::PARALLEL_DIROPS))
    }

    fn lookup(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        let name = name.to_bytes();
        let state = self.state.read().unwrap();
        let ino = if name == b"." || name.is_empty() {
            parent
        } else if name == b".." {
            state.get(parent)?.parent
        } else {
            state.child_of(parent, name)?
        };
        let n = state.get(ino)?;
        n.nlookup.fetch_add(1, Ordering::AcqRel);
        Ok(self.entry_of(ino, n))
    }

    fn forget(&self, _ctx: &Context, inode: u64, count: u64) {
        let mut state = self.state.write().unwrap();
        if let Some(n) = state.nodes.get(&inode) {
            let mut cur = n.nlookup.load(Ordering::Acquire);
            loop {
                let next = cur.saturating_sub(count);
                match n
                    .nlookup
                    .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => break,
                    Err(v) => cur = v,
                }
            }
        }
        state.maybe_reap(inode);
    }

    fn batch_forget(&self, ctx: &Context, requests: Vec<(u64, u64)>) {
        for (inode, count) in requests {
            self.forget(ctx, inode, count);
        }
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> io::Result<(stat64, Duration)> {
        let state = self.state.read().unwrap();
        let n = state.get(inode)?;
        Ok((self.attr_of(inode, n), TTL))
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: u64,
        attr: stat64,
        _handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        let mut state = self.state.write().unwrap();
        let n = state.get_mut(inode)?;
        let t = now();
        if valid.contains(SetattrValid::SIZE) {
            let data = n.file_mut()?; // EISDIR on directories
            data.resize(attr.st_size as usize, 0);
            n.mtime = t;
        }
        if valid.contains(SetattrValid::MODE) {
            n.mode = (n.mode & libc::S_IFMT) | (attr.st_mode & 0o7777);
        }
        if valid.contains(SetattrValid::UID) {
            n.uid = attr.st_uid;
        }
        if valid.contains(SetattrValid::GID) {
            n.gid = attr.st_gid;
        }
        if valid.contains(SetattrValid::ATIME) {
            n.atime = (attr.st_atime, attr.st_atime_nsec);
        }
        if valid.contains(SetattrValid::ATIME_NOW) {
            n.atime = t;
        }
        if valid.contains(SetattrValid::MTIME) {
            n.mtime = (attr.st_mtime, attr.st_mtime_nsec);
        }
        if valid.contains(SetattrValid::MTIME_NOW) {
            n.mtime = t;
        }
        if valid.contains(SetattrValid::CTIME) {
            n.ctime = (attr.st_ctime, attr.st_ctime_nsec);
        } else if !valid.is_empty() {
            n.ctime = t;
        }
        Ok((self.attr_of(inode, n), TTL))
    }

    fn mknod(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        let fmt = mode & libc::S_IFMT;
        if fmt != 0 && fmt != libc::S_IFREG {
            return Err(err(libc::EPERM)); // devices/fifos/sockets out of scope
        }
        let mut state = self.state.write().unwrap();
        let p = state.get(parent)?;
        let nb = name_bytes(name)?.to_vec();
        if p.dir()?.contains_key(&nb) {
            return Err(err(libc::EEXIST));
        }
        let ino = state.alloc_ino();
        let node = Node::new_file(parent, mode, self.uid, self.gid);
        node.nlookup.store(1, Ordering::Release);
        let entry = self.entry_of(ino, &node);
        state.nodes.insert(ino, node);
        let t = now();
        let p = state.get_mut(parent)?;
        p.dir_mut()?.insert(nb, ino);
        p.mtime = t;
        p.ctime = t;
        Ok(entry)
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        let mut state = self.state.write().unwrap();
        let p = state.get(parent)?;
        let nb = name_bytes(name)?.to_vec();
        if p.dir()?.contains_key(&nb) {
            return Err(err(libc::EEXIST));
        }
        let ino = state.alloc_ino();
        let node = Node::new_dir(parent, mode, self.uid, self.gid);
        node.nlookup.store(1, Ordering::Release);
        let entry = self.entry_of(ino, &node);
        state.nodes.insert(ino, node);
        let t = now();
        let p = state.get_mut(parent)?;
        p.dir_mut()?.insert(nb, ino);
        p.nlink += 1; // the child's ".."
        p.mtime = t;
        p.ctime = t;
        Ok(entry)
    }

    fn unlink(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        let nb = name_bytes(name)?;
        let mut state = self.state.write().unwrap();
        let ino = state.child_of(parent, nb)?;
        if state.get(ino)?.is_dir() {
            return Err(err(libc::EISDIR));
        }
        let t = now();
        let p = state.get_mut(parent)?;
        p.dir_mut()?.remove(nb);
        p.mtime = t;
        p.ctime = t;
        MemFs::mark_unlinked(&mut state, ino, false);
        Ok(())
    }

    fn rmdir(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        let nb = name_bytes(name)?;
        let mut state = self.state.write().unwrap();
        let ino = state.child_of(parent, nb)?;
        let n = state.get(ino)?;
        if !n.dir()?.is_empty() {
            return Err(err(libc::ENOTEMPTY));
        }
        let t = now();
        let p = state.get_mut(parent)?;
        p.dir_mut()?.remove(nb);
        p.nlink = p.nlink.saturating_sub(1);
        p.mtime = t;
        p.ctime = t;
        MemFs::mark_unlinked(&mut state, ino, true);
        Ok(())
    }

    fn rename(
        &self,
        _ctx: &Context,
        olddir: u64,
        oldname: &CStr,
        newdir: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        let ob = name_bytes(oldname)?.to_vec();
        let nb = name_bytes(newname)?.to_vec();
        let mut state = self.state.write().unwrap();
        let src = state.child_of(olddir, &ob)?;
        let dst = state.get(newdir)?.dir()?.get(&nb).copied();

        if flags & libc::RENAME_NOREPLACE != 0 && dst.is_some() {
            return Err(err(libc::EEXIST));
        }
        if flags & libc::RENAME_EXCHANGE != 0 {
            let dst = dst.ok_or_else(|| err(libc::ENOENT))?;
            let src_is_dir = state.get(src)?.is_dir();
            let dst_is_dir = state.get(dst)?.is_dir();
            let t = now();
            state.get_mut(olddir)?.dir_mut()?.insert(ob, dst);
            state.get_mut(newdir)?.dir_mut()?.insert(nb, src);
            if olddir != newdir {
                state.get_mut(src)?.parent = newdir;
                state.get_mut(dst)?.parent = olddir;
                // Fix up parents' link counts if directory-ness differs.
                if src_is_dir != dst_is_dir {
                    let (dir_gain, dir_loss) = if src_is_dir {
                        (newdir, olddir)
                    } else {
                        (olddir, newdir)
                    };
                    state.get_mut(dir_gain)?.nlink += 1;
                    let l = state.get_mut(dir_loss)?;
                    l.nlink = l.nlink.saturating_sub(1);
                }
            }
            for d in [olddir, newdir] {
                let p = state.get_mut(d)?;
                p.mtime = t;
                p.ctime = t;
            }
            return Ok(());
        }

        if dst == Some(src) {
            return Ok(()); // rename onto itself is a no-op
        }
        let src_is_dir = state.get(src)?.is_dir();
        if let Some(dst) = dst {
            let d = state.get(dst)?;
            let dst_is_dir = d.is_dir();
            if src_is_dir && !dst_is_dir {
                return Err(err(libc::ENOTDIR));
            }
            if !src_is_dir && dst_is_dir {
                return Err(err(libc::EISDIR));
            }
            if dst_is_dir && !d.dir()?.is_empty() {
                return Err(err(libc::ENOTEMPTY));
            }
            state.get_mut(newdir)?.dir_mut()?.remove(&nb);
            if dst_is_dir {
                let p = state.get_mut(newdir)?;
                p.nlink = p.nlink.saturating_sub(1);
            }
            MemFs::mark_unlinked(&mut state, dst, dst_is_dir);
        }
        state.get_mut(olddir)?.dir_mut()?.remove(&ob);
        state.get_mut(newdir)?.dir_mut()?.insert(nb, src);
        state.get_mut(src)?.parent = newdir;
        if src_is_dir && olddir != newdir {
            let p = state.get_mut(olddir)?;
            p.nlink = p.nlink.saturating_sub(1);
            state.get_mut(newdir)?.nlink += 1;
        }
        let t = now();
        for d in [olddir, newdir] {
            let p = state.get_mut(d)?;
            p.mtime = t;
            p.ctime = t;
        }
        state.get_mut(src)?.ctime = t;
        Ok(())
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: u64,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions, Option<u32>)> {
        // ATOMIC_O_TRUNC is negotiated, so O_TRUNC arrives here.
        if flags & (libc::O_TRUNC as u32) != 0 {
            let mut state = self.state.write().unwrap();
            let n = state.get_mut(inode)?;
            if let Body::File(data) = &mut n.body {
                data.clear();
                let t = now();
                n.mtime = t;
                n.ctime = t;
            }
        } else {
            // Existence check.
            let state = self.state.read().unwrap();
            state.get(inode)?;
        }
        // No per-open state is needed for files: read/write receive the open
        // flags with every request. KEEP_CACHE is safe (nothing changes the
        // tree behind the kernel's back) and keeps page cache warm.
        let h = self.next_handle.fetch_add(1, Ordering::Relaxed);
        Ok((Some(h), OpenOptions::KEEP_CACHE, None))
    }

    fn create(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(Entry, Option<u64>, OpenOptions, Option<u32>)> {
        let mut state = self.state.write().unwrap();
        let p = state.get(parent)?;
        let nb = name_bytes(name)?.to_vec();
        let entry = if let Some(&existing) = p.dir()?.get(&nb) {
            if args.flags & (libc::O_EXCL as u32) != 0 {
                return Err(err(libc::EEXIST));
            }
            let n = state.get_mut(existing)?;
            if n.is_dir() {
                return Err(err(libc::EISDIR));
            }
            if args.flags & (libc::O_TRUNC as u32) != 0 {
                n.file_mut()?.clear();
                let t = now();
                n.mtime = t;
                n.ctime = t;
            }
            n.nlookup.fetch_add(1, Ordering::AcqRel);
            self.entry_of(existing, state.get(existing)?)
        } else {
            let ino = state.alloc_ino();
            let node = Node::new_file(parent, args.mode, self.uid, self.gid);
            node.nlookup.store(1, Ordering::Release);
            let entry = self.entry_of(ino, &node);
            state.nodes.insert(ino, node);
            let t = now();
            let p = state.get_mut(parent)?;
            p.dir_mut()?.insert(nb, ino);
            p.mtime = t;
            p.ctime = t;
            entry
        };
        let h = self.next_handle.fetch_add(1, Ordering::Relaxed);
        Ok((entry, Some(h), OpenOptions::KEEP_CACHE, None))
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: u64,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let state = self.state.read().unwrap();
        let n = state.get(inode)?;
        let data = match &n.body {
            Body::File(d) => d,
            Body::Dir(_) => return Err(err(libc::EISDIR)),
        };
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(size as usize).min(data.len());
        w.write_all(&data[start..end])?;
        Ok(end - start)
    }

    fn write(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: u64,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        let mut state = self.state.write().unwrap();
        let n = state.get_mut(inode)?;
        let t = now();
        n.mtime = t;
        n.ctime = t;
        let data = n.file_mut()?;
        // Without writeback caching the filesystem owns O_APPEND semantics.
        let off = if flags & (libc::O_APPEND as u32) != 0 {
            data.len()
        } else {
            offset as usize
        };
        let end = off + size as usize;
        if data.len() < end {
            data.resize(end, 0); // zero-fills any gap (sparse write)
        }
        r.read_exact(&mut data[off..end])?;
        Ok(size as usize)
    }

    fn flush(&self, _ctx: &Context, inode: u64, _handle: u64, _lock_owner: u64) -> io::Result<()> {
        let state = self.state.read().unwrap();
        state.get(inode)?;
        Ok(())
    }

    fn fsync(&self, _ctx: &Context, _inode: u64, _datasync: bool, _handle: u64) -> io::Result<()> {
        Ok(())
    }

    fn fsyncdir(
        &self,
        _ctx: &Context,
        _inode: u64,
        _datasync: bool,
        _handle: u64,
    ) -> io::Result<()> {
        Ok(())
    }

    fn fallocate(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: u64,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        let mode = mode as i32;
        if mode & !(libc::FALLOC_FL_KEEP_SIZE | libc::FALLOC_FL_PUNCH_HOLE) != 0 {
            return Err(err(libc::EOPNOTSUPP));
        }
        let mut state = self.state.write().unwrap();
        let n = state.get_mut(inode)?;
        let data = n.file_mut()?;
        let end = (offset + length) as usize;
        if mode & libc::FALLOC_FL_PUNCH_HOLE != 0 {
            let stop = end.min(data.len());
            let start = (offset as usize).min(stop);
            data[start..stop].fill(0);
        } else if mode & libc::FALLOC_FL_KEEP_SIZE == 0 && data.len() < end {
            data.resize(end, 0);
        }
        Ok(())
    }

    fn release(
        &self,
        _ctx: &Context,
        _inode: u64,
        _flags: u32,
        _handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn statfs(&self, _ctx: &Context, _inode: u64) -> io::Result<statvfs64> {
        let state = self.state.read().unwrap();
        let used: u64 = state
            .nodes
            .values()
            .map(|n| (n.size() + BLKSIZE - 1) / BLKSIZE)
            .sum();
        let total: u64 = 16 * 1024 * 1024; // 64 GiB of 4k blocks — plausible
        let free = total.saturating_sub(used);
        let mut st: statvfs64 = unsafe { std::mem::zeroed() };
        st.f_bsize = BLKSIZE;
        st.f_frsize = BLKSIZE;
        st.f_blocks = total;
        st.f_bfree = free;
        st.f_bavail = free;
        st.f_files = 1 << 20;
        st.f_ffree = (1u64 << 20).saturating_sub(state.nodes.len() as u64);
        st.f_favail = st.f_ffree;
        st.f_namemax = 255;
        Ok(st)
    }

    fn opendir(
        &self,
        _ctx: &Context,
        inode: u64,
        _flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        let state = self.state.read().unwrap();
        let n = state.get(inode)?;
        let children = n.dir()?;
        let mut entries = Vec::with_capacity(children.len() + 2);
        entries.push((inode, libc::DT_DIR as u32, b".".to_vec()));
        entries.push((n.parent, libc::DT_DIR as u32, b"..".to_vec()));
        for (name, &ino) in children {
            let dtype = state.get(ino).map(dtype_of).unwrap_or(libc::DT_REG as u32);
            entries.push((ino, dtype, name.clone()));
        }
        let h = self.new_dir_handle(DirSnapshot { entries });
        Ok((Some(h), OpenOptions::CACHE_DIR))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let snap = {
            let handles = self.dir_handles.lock().unwrap();
            match handles.get(&handle) {
                Some(s) => Arc::clone(s),
                None => {
                    // Shouldn't happen (we always hand out opendir handles),
                    // but keep a graceful failure mode.
                    let state = self.state.read().unwrap();
                    state.get(inode)?.dir()?;
                    return Err(err(libc::EBADF));
                }
            }
        };
        for (i, (ino, dtype, name)) in snap.entries.iter().enumerate().skip(offset as usize) {
            let written = add_entry(DirEntry {
                ino: *ino,
                offset: (i + 1) as u64,
                type_: *dtype,
                name,
            })?;
            if written == 0 {
                break; // reply buffer full; kernel will come back with offset
            }
        }
        Ok(())
    }

    fn releasedir(&self, _ctx: &Context, _inode: u64, _flags: u32, handle: u64) -> io::Result<()> {
        self.dir_handles.lock().unwrap().remove(&handle);
        Ok(())
    }

    fn access(&self, _ctx: &Context, inode: u64, _mask: u32) -> io::Result<()> {
        // Mounted with default_permissions (the crate hard-wires it), so the
        // kernel does mode-bit checks; existence is all that's left.
        let state = self.state.read().unwrap();
        state.get(inode)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// main: mount and drive the session
// ---------------------------------------------------------------------------

fn service_loop(server: Arc<Server<MemFs>>, mut channel: FuseChannel) {
    loop {
        match channel.get_request() {
            Ok(Some((reader, writer))) => {
                if let Err(e) = server.handle_message(reader, writer.into(), None, None) {
                    match e {
                        fuse_backend_rs::Error::EncodeMessage(_) => break, // kernel gone
                        _ => continue, // per-request error was already replied
                    }
                }
            }
            Ok(None) => break, // session unmounted / woken for exit
            Err(_) => break,
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: {FSNAME} <mountpoint>");
        std::process::exit(2);
    }
    let mountpoint = Path::new(&args[1]);

    let fs = MemFs::new();
    let server = Arc::new(Server::new(fs));

    let mut session = match FuseSession::new(mountpoint, FSNAME, "", false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{FSNAME}: failed to create session on {}: {e}", args[1]);
            std::process::exit(1);
        }
    };
    // allow_other defaults to *true* in fuse-backend-rs; fusermount3 rejects
    // it unless /etc/fuse.conf has user_allow_other, so switch it off.
    session.set_allow_other(false);
    if let Err(e) = session.mount() {
        eprintln!("{FSNAME}: mount on {} failed: {e}", args[1]);
        std::process::exit(1);
    }

    let mut workers = Vec::with_capacity(NUM_CHANNELS);
    for _ in 0..NUM_CHANNELS {
        match session.new_channel() {
            Ok(ch) => {
                let server = Arc::clone(&server);
                workers.push(thread::spawn(move || service_loop(server, ch)));
            }
            Err(e) => {
                eprintln!("{FSNAME}: failed to create FUSE channel: {e}");
                let _ = session.umount();
                std::process::exit(1);
            }
        }
    }
    for w in workers {
        let _ = w.join();
    }
    // The harness unmounts with fusermount3 -u; reaching here means the
    // kernel connection is gone. Session Drop tidies up whatever remains.
}
