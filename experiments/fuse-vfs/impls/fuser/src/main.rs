//! fuse-vfs-fuser — blind bake-off implementation for the `fuser` crate.
//!
//! Contract: docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md
//! An in-memory read-write VFS, starting empty, mounted at the CLI argument.
//! Runs in the foreground until unmounted, then exits 0.
//!
//! Store shape: a flat inode table (`HashMap<u64, Inode>`); directories hold a
//! sorted name → ino map. Unlinked-but-possibly-open inodes are simply left in
//! the table (never reused), which gives correct open-after-unlink semantics
//! for free at the cost of not reclaiming memory — fine for a spike.

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    TimeOrNow,
};
use libc::{EEXIST, EINVAL, EISDIR, ENOENT, ENOTDIR, ENOTEMPTY, EPERM};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::time::{Duration, SystemTime};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;
const BLKSIZE: u32 = 4096;

// ---------------------------------------------------------------------------
// The in-memory store
// ---------------------------------------------------------------------------

enum Node {
    File {
        data: Vec<u8>,
    },
    Dir {
        /// BTreeMap so readdir order is deterministic and stable across the
        /// kernel's offset-resumed readdir calls.
        children: BTreeMap<OsString, u64>,
        /// Inode of the containing directory (self for the root); used only
        /// to report `..` in readdir.
        parent: u64,
    },
}

struct Inode {
    node: Node,
    perm: u16, // permission bits (mode & 0o7777)
    atime: SystemTime,
    mtime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
}

impl Inode {
    fn new(node: Node, perm: u16) -> Self {
        let now = SystemTime::now();
        Inode {
            node,
            perm,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.node, Node::Dir { .. })
    }
}

struct Vfs {
    inodes: HashMap<u64, Inode>,
    next_ino: u64,
    uid: u32,
    gid: u32,
}

impl Vfs {
    fn new() -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_INO,
            Inode::new(
                Node::Dir {
                    children: BTreeMap::new(),
                    parent: ROOT_INO,
                },
                0o755,
            ),
        );
        Vfs {
            inodes,
            next_ino: ROOT_INO + 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn alloc(&mut self, node: Node, perm: u16) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, Inode::new(node, perm));
        ino
    }

    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let inode = self.inodes.get(&ino)?;
        let (kind, size, nlink) = match &inode.node {
            Node::File { data } => (FileType::RegularFile, data.len() as u64, 1),
            Node::Dir { children, .. } => {
                let subdirs = children
                    .values()
                    .filter(|c| self.inodes.get(c).is_some_and(Inode::is_dir))
                    .count() as u32;
                (FileType::Directory, BLKSIZE as u64, 2 + subdirs)
            }
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: inode.atime,
            mtime: inode.mtime,
            ctime: inode.ctime,
            crtime: inode.crtime,
            kind,
            perm: inode.perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLKSIZE,
            flags: 0,
        })
    }

    /// Children map of `ino`, or the errno a path-walk over it deserves.
    fn children(&self, ino: u64) -> Result<&BTreeMap<OsString, u64>, i32> {
        match self.inodes.get(&ino) {
            None => Err(ENOENT),
            Some(Inode {
                node: Node::Dir { children, .. },
                ..
            }) => Ok(children),
            Some(_) => Err(ENOTDIR),
        }
    }

    fn child_of(&self, parent: u64, name: &OsStr) -> Result<u64, i32> {
        self.children(parent)?.get(name).copied().ok_or(ENOENT)
    }

    fn touch_dir(&mut self, ino: u64) {
        if let Some(inode) = self.inodes.get_mut(&ino) {
            let now = SystemTime::now();
            inode.mtime = now;
            inode.ctime = now;
        }
    }

    /// Insert a fresh child under `parent`. Errors if the name exists.
    fn link_new(&mut self, parent: u64, name: &OsStr, node: Node, perm: u16) -> Result<u64, i32> {
        match self.children(parent) {
            Err(e) => return Err(e),
            Ok(c) if c.contains_key(name) => return Err(EEXIST),
            Ok(_) => {}
        }
        let ino = self.alloc(node, perm);
        if let Some(Inode {
            node: Node::Dir { children, .. },
            ..
        }) = self.inodes.get_mut(&parent)
        {
            children.insert(name.to_os_string(), ino);
        }
        self.touch_dir(parent);
        Ok(ino)
    }
}

fn systime(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

// ---------------------------------------------------------------------------
// FUSE callbacks
// ---------------------------------------------------------------------------

impl Filesystem for Vfs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.child_of(parent, name) {
            Ok(ino) => reply.entry(&TTL, &self.attr(ino).unwrap(), 0),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.attr(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(inode) = self.inodes.get_mut(&ino) else {
            reply.error(ENOENT);
            return;
        };
        if let Some(s) = size {
            match &mut inode.node {
                Node::File { data } => {
                    data.resize(s as usize, 0); // truncate or zero-extend
                    inode.mtime = SystemTime::now();
                }
                Node::Dir { .. } => {
                    reply.error(EISDIR);
                    return;
                }
            }
        }
        if let Some(m) = mode {
            inode.perm = (m & 0o7777) as u16;
        }
        if let Some(t) = atime {
            inode.atime = systime(t);
        }
        if let Some(t) = mtime {
            inode.mtime = systime(t);
        }
        inode.ctime = ctime.unwrap_or_else(SystemTime::now);
        reply.attr(&TTL, &self.attr(ino).unwrap());
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Regular files only; devices/fifos/sockets are out of scope for v1.
        let file_type = mode & libc::S_IFMT;
        if file_type != libc::S_IFREG && file_type != 0 {
            reply.error(EPERM);
            return;
        }
        let perm = (mode & !umask & 0o7777) as u16;
        match self.link_new(parent, name, Node::File { data: Vec::new() }, perm) {
            Ok(ino) => reply.entry(&TTL, &self.attr(ino).unwrap(), 0),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let perm = (mode & !umask & 0o7777) as u16;
        let node = Node::Dir {
            children: BTreeMap::new(),
            parent,
        };
        match self.link_new(parent, name, node, perm) {
            Ok(ino) => reply.entry(&TTL, &self.attr(ino).unwrap(), 0),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let ino = match self.child_of(parent, name) {
            Ok(i) => i,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if self.inodes[&ino].is_dir() {
            reply.error(EISDIR);
            return;
        }
        if let Some(Inode {
            node: Node::Dir { children, .. },
            ..
        }) = self.inodes.get_mut(&parent)
        {
            children.remove(name);
        }
        // The inode itself stays in the table: still-open handles keep working.
        self.touch_dir(parent);
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let ino = match self.child_of(parent, name) {
            Ok(i) => i,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        match &self.inodes[&ino].node {
            Node::File { .. } => {
                reply.error(ENOTDIR);
                return;
            }
            Node::Dir { children, .. } if !children.is_empty() => {
                reply.error(ENOTEMPTY);
                return;
            }
            Node::Dir { .. } => {}
        }
        if let Some(Inode {
            node: Node::Dir { children, .. },
            ..
        }) = self.inodes.get_mut(&parent)
        {
            children.remove(name);
        }
        self.touch_dir(parent);
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let src = match self.child_of(parent, name) {
            Ok(i) => i,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let existing = match self.children(newparent) {
            Ok(c) => c.get(newname).copied(),
            Err(e) => {
                reply.error(e);
                return;
            }
        };

        if flags & libc::RENAME_EXCHANGE != 0 {
            let Some(dst) = existing else {
                reply.error(ENOENT);
                return;
            };
            // Swap the two directory entries.
            if let Some(Inode {
                node: Node::Dir { children, .. },
                ..
            }) = self.inodes.get_mut(&parent)
            {
                children.insert(name.to_os_string(), dst);
            }
            if let Some(Inode {
                node: Node::Dir { children, .. },
                ..
            }) = self.inodes.get_mut(&newparent)
            {
                children.insert(newname.to_os_string(), src);
            }
            for (moved, into) in [(src, newparent), (dst, parent)] {
                if let Some(Inode {
                    node: Node::Dir { parent: p, .. },
                    ..
                }) = self.inodes.get_mut(&moved)
                {
                    *p = into;
                }
            }
            self.touch_dir(parent);
            self.touch_dir(newparent);
            reply.ok();
            return;
        }

        if let Some(dst) = existing {
            if flags & libc::RENAME_NOREPLACE != 0 {
                reply.error(EEXIST);
                return;
            }
            // POSIX overwrite semantics.
            match (&self.inodes[&src].node, &self.inodes[&dst].node) {
                (Node::Dir { .. }, Node::File { .. }) => {
                    reply.error(ENOTDIR);
                    return;
                }
                (Node::File { .. }, Node::Dir { .. }) => {
                    reply.error(EISDIR);
                    return;
                }
                (Node::Dir { .. }, Node::Dir { children, .. }) if !children.is_empty() => {
                    reply.error(ENOTEMPTY);
                    return;
                }
                _ => {} // replaced entry's inode is simply orphaned
            }
        }

        if let Some(Inode {
            node: Node::Dir { children, .. },
            ..
        }) = self.inodes.get_mut(&parent)
        {
            children.remove(name);
        }
        if let Some(Inode {
            node: Node::Dir { children, .. },
            ..
        }) = self.inodes.get_mut(&newparent)
        {
            children.insert(newname.to_os_string(), src);
        }
        if let Some(Inode {
            node: Node::Dir { parent: p, .. },
            ..
        }) = self.inodes.get_mut(&src)
        {
            *p = newparent;
        }
        self.touch_dir(parent);
        self.touch_dir(newparent);
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.inodes.get_mut(&ino) {
            None => reply.error(ENOENT),
            Some(inode) => {
                // Without ATOMIC_O_TRUNC the kernel truncates via setattr, but
                // honour O_TRUNC here too for completeness.
                if flags & libc::O_TRUNC != 0 {
                    if let Node::File { data } = &mut inode.node {
                        data.clear();
                        inode.mtime = SystemTime::now();
                    }
                }
                reply.opened(0, 0); // stateless: everything is addressed by ino
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.inodes.get(&ino) {
            None => reply.error(ENOENT),
            Some(Inode {
                node: Node::Dir { .. },
                ..
            }) => reply.error(EISDIR),
            Some(Inode {
                node: Node::File { data },
                ..
            }) => {
                if offset < 0 {
                    reply.error(EINVAL);
                    return;
                }
                let start = (offset as usize).min(data.len());
                let end = start.saturating_add(size as usize).min(data.len());
                reply.data(&data[start..end]); // short (or empty) at EOF
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        buf: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.inodes.get_mut(&ino) {
            None => reply.error(ENOENT),
            Some(Inode {
                node: Node::Dir { .. },
                ..
            }) => reply.error(EISDIR),
            Some(inode) => {
                if offset < 0 {
                    reply.error(EINVAL);
                    return;
                }
                let Node::File { data } = &mut inode.node else {
                    unreachable!()
                };
                let start = offset as usize;
                let end = start + buf.len();
                if end > data.len() {
                    data.resize(end, 0); // zero-fill any hole before `start`
                }
                data[start..end].copy_from_slice(buf);
                let now = SystemTime::now();
                inode.mtime = now;
                inode.ctime = now;
                reply.written(buf.len() as u32);
            }
        }
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
        reply.ok(); // nothing buffered outside the store
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok(); // memory is as durable as we get
    }

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let (children, parent) = match self.inodes.get(&ino) {
            None => {
                reply.error(ENOENT);
                return;
            }
            Some(Inode {
                node: Node::File { .. },
                ..
            }) => {
                reply.error(ENOTDIR);
                return;
            }
            Some(Inode {
                node: Node::Dir { children, parent },
                ..
            }) => (children, *parent),
        };
        let dot: [(u64, FileType, &OsStr); 2] = [
            (ino, FileType::Directory, OsStr::new(".")),
            (parent, FileType::Directory, OsStr::new("..")),
        ];
        let entries = dot.into_iter().chain(children.iter().map(|(name, &c)| {
            let kind = match self.inodes.get(&c).map(Inode::is_dir) {
                Some(true) => FileType::Directory,
                _ => FileType::RegularFile,
            };
            (c, kind, name.as_os_str())
        }));
        for (i, (entry_ino, kind, name)) in entries.enumerate().skip(offset as usize) {
            // The offset passed to add() is where a subsequent readdir resumes.
            if reply.add(entry_ino, (i + 1) as i64, kind, name) {
                break; // buffer full
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // Plausible non-zero fixed geometry: a 4 GiB volume, half free.
        let blocks: u64 = 1 << 20;
        let files: u64 = 1 << 20;
        reply.statfs(
            blocks,
            blocks / 2,
            blocks / 2,
            files,
            files - self.inodes.len() as u64,
            BLKSIZE,
            255,
            BLKSIZE,
        );
    }

    fn access(&mut self, _req: &Request<'_>, ino: u64, _mask: i32, reply: ReplyEmpty) {
        // Mounted with DefaultPermissions, so the kernel enforces the mode
        // bits itself; this is only reached if that option is ever dropped.
        if self.inodes.contains_key(&ino) {
            reply.ok();
        } else {
            reply.error(ENOENT);
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let perm = (mode & !umask & 0o7777) as u16;
        let ino = match self.link_new(parent, name, Node::File { data: Vec::new() }, perm) {
            Ok(ino) => ino,
            Err(EEXIST) if flags & libc::O_EXCL != 0 => {
                reply.error(EEXIST);
                return;
            }
            Err(EEXIST) => {
                // Open the existing file (the kernel normally resolves this
                // via lookup first, but handle the race-shaped case anyway).
                let ino = self.child_of(parent, name).unwrap();
                let inode = self.inodes.get_mut(&ino).unwrap();
                match &mut inode.node {
                    Node::Dir { .. } => {
                        reply.error(EISDIR);
                        return;
                    }
                    Node::File { data } => {
                        if flags & libc::O_TRUNC != 0 {
                            data.clear();
                            inode.mtime = SystemTime::now();
                        }
                    }
                }
                ino
            }
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        reply.created(&TTL, &self.attr(ino).unwrap(), 0, 0, 0);
    }

    // Out of scope for v1, but return the errno a real tool expects so its
    // fallback path (e.g. git's link→rename) triggers deterministically.
    fn link(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _newparent: u64,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(EPERM);
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        reply.error(EPERM);
    }
}

// ---------------------------------------------------------------------------

fn main() {
    let mut args = std::env::args_os();
    let argv0 = args.next();
    let (Some(mountpoint), None) = (args.next(), args.next()) else {
        eprintln!(
            "usage: {} <mountpoint>",
            argv0
                .as_deref()
                .unwrap_or(OsStr::new("fuse-vfs-fuser"))
                .to_string_lossy()
        );
        std::process::exit(2);
    };
    let options = [
        MountOption::FSName("fuse-vfs-fuser".to_string()),
        MountOption::DefaultPermissions,
    ];
    // mount2 drives the FUSE session on this thread until unmount.
    if let Err(e) = fuser::mount2(Vfs::new(), &mountpoint, &options) {
        eprintln!("fuse-vfs-fuser: mount failed: {e}");
        std::process::exit(1);
    }
}
