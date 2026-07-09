//! fuse-vfs-easy_fuser — in-memory read-write VFS for the FUSE crate bake-off.
//!
//! Contract: docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md
//! CLI: `fuse-vfs-easy_fuser <mountpoint>` mounts an initially-empty in-memory
//! filesystem, runs in the foreground until unmounted (fusermount3 -u), exits 0.
//!
//! Crate posture: easy_fuser 0.5 in "parallel" mode (sync callbacks on a
//! threadpool; no tokio). TId = PathBuf, so the crate's PathResolver owns all
//! inode<->path bookkeeping and this handler is purely path-addressed.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use easy_fuser::delegate_fs;
use easy_fuser::fuse_parallel::prelude::*;
use easy_fuser::fuse_presets::DefaultFuseHandler;

// ---------------------------------------------------------------------------
// The in-memory store: a hand-rolled tree of directories and regular files.
// ---------------------------------------------------------------------------

struct Meta {
    mode: u32, // permission bits only (0o7777)
    atime: SystemTime,
    mtime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
}

impl Meta {
    fn new(mode: u32) -> Self {
        let now = SystemTime::now();
        Meta {
            mode: mode & 0o7777,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
        }
    }

    fn touch(&mut self) {
        let now = SystemTime::now();
        self.mtime = now;
        self.ctime = now;
    }
}

enum Body {
    File(Vec<u8>),
    Dir(BTreeMap<OsString, Node>),
}

struct Node {
    meta: Meta,
    body: Body,
}

impl Node {
    fn new_file(mode: u32) -> Self {
        Node {
            meta: Meta::new(mode),
            body: Body::File(Vec::new()),
        }
    }

    fn new_dir(mode: u32) -> Self {
        Node {
            meta: Meta::new(mode),
            body: Body::Dir(BTreeMap::new()),
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.body, Body::Dir(_))
    }

    fn dir_is_empty(&self) -> bool {
        matches!(&self.body, Body::Dir(c) if c.is_empty())
    }
}

fn err(kind: ErrorKind) -> PosixError {
    PosixError::new(kind, "")
}

/// Walk `path` (relative; empty = root) down from `root`.
fn node<'a>(root: &'a Node, path: &Path) -> FuseResult<&'a Node> {
    let mut cur = root;
    for comp in path.iter() {
        match &cur.body {
            Body::Dir(children) => {
                cur = children.get(comp).ok_or_else(|| err(ErrorKind::FileNotFound))?;
            }
            Body::File(_) => return Err(err(ErrorKind::NotADirectory)),
        }
    }
    Ok(cur)
}

fn node_mut<'a>(root: &'a mut Node, path: &Path) -> FuseResult<&'a mut Node> {
    let mut cur = root;
    for comp in path.iter() {
        match &mut cur.body {
            Body::Dir(children) => {
                cur = children
                    .get_mut(comp)
                    .ok_or_else(|| err(ErrorKind::FileNotFound))?;
            }
            Body::File(_) => return Err(err(ErrorKind::NotADirectory)),
        }
    }
    Ok(cur)
}

/// Resolve `path` and return its children map (ENOTDIR if it is a file).
fn dir_children_mut<'a>(
    root: &'a mut Node,
    path: &Path,
) -> FuseResult<&'a mut BTreeMap<OsString, Node>> {
    match &mut node_mut(root, path)?.body {
        Body::Dir(children) => Ok(children),
        Body::File(_) => Err(err(ErrorKind::NotADirectory)),
    }
}

// ---------------------------------------------------------------------------
// The FUSE handler.
// ---------------------------------------------------------------------------

struct Vfs {
    root: Mutex<Node>,
    uid: u32,
    gid: u32,
    /// Out-of-scope ops (symlinks, hard links, xattrs, locks, ...) are
    /// delegated here; it answers ENOSYS and the kernel falls back or the
    /// tool degrades, per the spec.
    unsupported: DefaultFuseHandler<PathBuf>,
}

impl Vfs {
    fn new() -> Self {
        Vfs {
            root: Mutex::new(Node::new_dir(0o755)),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            unsupported: DefaultFuseHandler::new(),
        }
    }

    fn attr_of(&self, n: &Node) -> FileAttribute {
        let (size, kind, nlink) = match &n.body {
            Body::File(data) => (data.len() as u64, FileKind::RegularFile, 1),
            // nlink = 1 for directories: "unknown", so tools like find do not
            // apply the leaf (nlink-2) optimisation to our synthetic tree.
            Body::Dir(_) => (4096, FileKind::Directory, 1),
        };
        FileAttribute {
            size,
            blocks: size.div_ceil(512),
            atime: n.meta.atime,
            mtime: n.meta.mtime,
            ctime: n.meta.ctime,
            crtime: n.meta.crtime,
            kind,
            perm: n.meta.mode as u16,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
            ttl: None,
            generation: None,
        }
    }

    fn stateless_handle() -> OwnedFileHandle {
        // We do stateless I/O (everything is addressed by path); the handle
        // value is never interpreted. Mirrors DefaultFuseHandler::opendir.
        unsafe { OwnedFileHandle::from_raw(0) }
    }
}

impl FuseHandler for Vfs {
    type TId = PathBuf;

    // ---- list / stat ------------------------------------------------------

    fn lookup(&self, _req: &RequestInfo, parent: PathBuf, name: &OsStr) -> FuseResult<FileAttribute> {
        let root = self.root.lock().unwrap();
        let child = node(&root, &parent.join(name))?;
        Ok(self.attr_of(child))
    }

    fn getattr(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        _fh: Option<BorrowedFileHandle<'_>>,
    ) -> FuseResult<FileAttribute> {
        let root = self.root.lock().unwrap();
        Ok(self.attr_of(node(&root, &id)?))
    }

    fn readdir(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        _fh: BorrowedFileHandle<'_>,
    ) -> FuseResult<Vec<(OsString, FileKind)>> {
        let root = self.root.lock().unwrap();
        let dir = node(&root, &id)?;
        match &dir.body {
            Body::Dir(children) => {
                let mut out = Vec::with_capacity(children.len() + 2);
                out.push((OsString::from("."), FileKind::Directory));
                out.push((OsString::from(".."), FileKind::Directory));
                for (name, child) in children {
                    let kind = if child.is_dir() {
                        FileKind::Directory
                    } else {
                        FileKind::RegularFile
                    };
                    out.push((name.clone(), kind));
                }
                Ok(out)
            }
            Body::File(_) => Err(err(ErrorKind::NotADirectory)),
        }
    }

    fn statfs(&self, _req: &RequestInfo, _id: PathBuf) -> FuseResult<StatFs> {
        // Plausible non-zero totals: 4 GiB of 4 KiB blocks, half free.
        Ok(StatFs {
            total_blocks: 1 << 20,
            free_blocks: 1 << 19,
            available_blocks: 1 << 19,
            total_files: 1 << 20,
            free_files: 1 << 19,
            block_size: 4096,
            max_filename_length: 255,
            fragment_size: 4096,
        })
    }

    fn access(&self, _req: &RequestInfo, _id: PathBuf, _mask: AccessMask) -> FuseResult<()> {
        Ok(())
    }

    // ---- read -------------------------------------------------------------

    fn open(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        flags: OpenFlags,
    ) -> FuseResult<(OwnedFileHandle, FUSEOpenResponseFlags)> {
        let mut root = self.root.lock().unwrap();
        let n = node_mut(&mut root, &id)?;
        let wants_write = flags.bits() & libc::O_ACCMODE != libc::O_RDONLY;
        match &mut n.body {
            Body::Dir(_) if wants_write => return Err(err(ErrorKind::IsADirectory)),
            Body::Dir(_) => {}
            Body::File(data) => {
                // Defensive: honour O_TRUNC even though the kernel normally
                // sends setattr(size=0) when atomic_o_trunc is off.
                if wants_write && flags.contains(OpenFlags::TRUNCATE) {
                    data.clear();
                    n.meta.touch();
                }
            }
        }
        Ok((Self::stateless_handle(), FUSEOpenResponseFlags::empty()))
    }

    fn read(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        _fh: BorrowedFileHandle<'_>,
        seek: SeekFrom,
        size: u32,
        _flags: FUSEOpenFlags,
        _lock_owner: Option<u64>,
    ) -> FuseResult<Vec<u8>> {
        let SeekFrom::Start(offset) = seek else {
            return Err(err(ErrorKind::InvalidArgument));
        };
        let root = self.root.lock().unwrap();
        match &node(&root, &id)?.body {
            Body::File(data) => {
                let start = (offset as usize).min(data.len());
                let end = start.saturating_add(size as usize).min(data.len());
                Ok(data[start..end].to_vec())
            }
            Body::Dir(_) => Err(err(ErrorKind::IsADirectory)),
        }
    }

    fn release(
        &self,
        _req: &RequestInfo,
        _id: PathBuf,
        _fh: OwnedFileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<u64>,
        _flush: bool,
    ) -> FuseResult<()> {
        Ok(())
    }

    // ---- write / create ---------------------------------------------------

    fn create(
        &self,
        _req: &RequestInfo,
        parent: PathBuf,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: OpenFlags,
    ) -> FuseResult<(OwnedFileHandle, FileAttribute, FUSEOpenResponseFlags)> {
        let mut root = self.root.lock().unwrap();
        let children = dir_children_mut(&mut root, &parent)?;
        let attr = match children.get_mut(name) {
            Some(existing) => {
                if flags.contains(OpenFlags::CREATE_EXCLUSIVE) {
                    return Err(err(ErrorKind::FileExists));
                }
                match &mut existing.body {
                    Body::Dir(_) => return Err(err(ErrorKind::IsADirectory)),
                    Body::File(data) => {
                        if flags.contains(OpenFlags::TRUNCATE) {
                            data.clear();
                            existing.meta.touch();
                        }
                        self.attr_of(existing)
                    }
                }
            }
            None => {
                let n = Node::new_file(mode & !umask);
                let attr = self.attr_of(&n);
                children.insert(name.to_os_string(), n);
                attr
            }
        };
        Ok((Self::stateless_handle(), attr, FUSEOpenResponseFlags::empty()))
    }

    fn mknod(
        &self,
        _req: &RequestInfo,
        parent: PathBuf,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: DeviceType,
    ) -> FuseResult<FileAttribute> {
        // Only regular files are in scope; devices/pipes/sockets are not.
        match rdev {
            DeviceType::RegularFile | DeviceType::Unknown => {}
            _ => return Err(err(ErrorKind::PermissionDenied)),
        }
        let mut root = self.root.lock().unwrap();
        let children = dir_children_mut(&mut root, &parent)?;
        if children.contains_key(name) {
            return Err(err(ErrorKind::FileExists));
        }
        let n = Node::new_file(mode & !umask);
        let attr = self.attr_of(&n);
        children.insert(name.to_os_string(), n);
        Ok(attr)
    }

    fn mkdir(
        &self,
        _req: &RequestInfo,
        parent: PathBuf,
        name: &OsStr,
        mode: u32,
        umask: u32,
    ) -> FuseResult<FileAttribute> {
        let mut root = self.root.lock().unwrap();
        let children = dir_children_mut(&mut root, &parent)?;
        if children.contains_key(name) {
            return Err(err(ErrorKind::FileExists));
        }
        let n = Node::new_dir(mode & !umask);
        let attr = self.attr_of(&n);
        children.insert(name.to_os_string(), n);
        Ok(attr)
    }

    fn write(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        _fh: BorrowedFileHandle<'_>,
        seek: SeekFrom,
        data: Vec<u8>,
        _write_flags: FUSEWriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<u64>,
    ) -> FuseResult<u32> {
        let SeekFrom::Start(offset) = seek else {
            return Err(err(ErrorKind::InvalidArgument));
        };
        let mut root = self.root.lock().unwrap();
        let n = node_mut(&mut root, &id)?;
        match &mut n.body {
            Body::File(contents) => {
                let offset = offset as usize;
                let end = offset + data.len();
                if contents.len() < end {
                    contents.resize(end, 0);
                }
                contents[offset..end].copy_from_slice(&data);
                n.meta.touch();
                Ok(data.len() as u32)
            }
            Body::Dir(_) => Err(err(ErrorKind::IsADirectory)),
        }
    }

    fn setattr(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        attrs: SetAttrRequest<'_>,
    ) -> FuseResult<FileAttribute> {
        let mut root = self.root.lock().unwrap();
        let n = node_mut(&mut root, &id)?;
        if let Some(size) = attrs.size {
            match &mut n.body {
                Body::File(data) => {
                    data.resize(size as usize, 0);
                    n.meta.touch();
                }
                Body::Dir(_) => return Err(err(ErrorKind::IsADirectory)),
            }
        }
        if let Some(mode) = attrs.mode {
            n.meta.mode = mode & 0o7777;
            n.meta.ctime = SystemTime::now();
        }
        let to_time = |t: TimeOrNow| match t {
            TimeOrNow::SpecificTime(t) => t,
            TimeOrNow::Now => SystemTime::now(),
        };
        if let Some(atime) = attrs.atime {
            n.meta.atime = to_time(atime);
        }
        if let Some(mtime) = attrs.mtime {
            n.meta.mtime = to_time(mtime);
            n.meta.ctime = SystemTime::now();
        }
        if let Some(ctime) = attrs.ctime {
            n.meta.ctime = ctime;
        }
        if let Some(crtime) = attrs.crtime {
            n.meta.crtime = crtime;
        }
        // uid/gid changes are accepted as no-ops (single-user VFS).
        Ok(self.attr_of(n))
    }

    fn flush(
        &self,
        _req: &RequestInfo,
        _id: PathBuf,
        _fh: BorrowedFileHandle<'_>,
        _lock_owner: u64,
    ) -> FuseResult<()> {
        Ok(())
    }

    fn fsync(
        &self,
        _req: &RequestInfo,
        _id: PathBuf,
        _fh: BorrowedFileHandle<'_>,
        _datasync: bool,
    ) -> FuseResult<()> {
        Ok(())
    }

    // ---- delete / move ----------------------------------------------------

    fn unlink(&self, _req: &RequestInfo, parent: PathBuf, name: &OsStr) -> FuseResult<()> {
        let mut root = self.root.lock().unwrap();
        let children = dir_children_mut(&mut root, &parent)?;
        match children.get(name) {
            None => Err(err(ErrorKind::FileNotFound)),
            Some(n) if n.is_dir() => Err(err(ErrorKind::IsADirectory)),
            Some(_) => {
                children.remove(name);
                Ok(())
            }
        }
    }

    fn rmdir(&self, _req: &RequestInfo, parent: PathBuf, name: &OsStr) -> FuseResult<()> {
        let mut root = self.root.lock().unwrap();
        let children = dir_children_mut(&mut root, &parent)?;
        match children.get(name) {
            None => Err(err(ErrorKind::FileNotFound)),
            Some(n) if !n.is_dir() => Err(err(ErrorKind::NotADirectory)),
            Some(n) if !n.dir_is_empty() => Err(err(ErrorKind::DirectoryNotEmpty)),
            Some(_) => {
                children.remove(name);
                Ok(())
            }
        }
    }

    fn rename(
        &self,
        _req: &RequestInfo,
        parent: PathBuf,
        name: &OsStr,
        newparent: PathBuf,
        newname: &OsStr,
        flags: RenameFlags,
    ) -> FuseResult<()> {
        let src_path = parent.join(name);
        let dst_path = newparent.join(newname);
        if src_path == dst_path {
            return Ok(());
        }
        // Cannot move a directory into its own subtree.
        if dst_path.starts_with(&src_path) {
            return Err(err(ErrorKind::InvalidArgument));
        }
        let mut root = self.root.lock().unwrap();

        // Validate source, destination parent, and overwrite semantics before
        // detaching anything (single lock => checks stay consistent).
        let src_is_dir = node(&root, &src_path)?.is_dir();
        match node(&root, &dst_path) {
            Ok(dst) => {
                if flags.contains(RenameFlags::NOREPLACE) {
                    return Err(err(ErrorKind::FileExists));
                }
                if !flags.contains(RenameFlags::EXCHANGE) {
                    match (src_is_dir, dst.is_dir()) {
                        (true, false) => return Err(err(ErrorKind::NotADirectory)),
                        (false, true) => return Err(err(ErrorKind::IsADirectory)),
                        (true, true) if !dst.dir_is_empty() => {
                            return Err(err(ErrorKind::DirectoryNotEmpty))
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => {
                if flags.contains(RenameFlags::EXCHANGE) {
                    return Err(err(ErrorKind::FileNotFound));
                }
                // Destination parent must exist and be a directory.
                dir_children_mut(&mut root, &newparent)?;
            }
        }

        let moved = dir_children_mut(&mut root, &parent)?
            .remove(name)
            .ok_or_else(|| err(ErrorKind::FileNotFound))?;
        let dst_children = dir_children_mut(&mut root, &newparent)
            .expect("destination parent validated above");
        let displaced = dst_children.insert(newname.to_os_string(), moved);
        if flags.contains(RenameFlags::EXCHANGE) {
            let displaced = displaced.expect("exchange target validated above");
            dir_children_mut(&mut root, &parent)
                .expect("source parent validated above")
                .insert(name.to_os_string(), displaced);
        }
        Ok(())
    }

    // ---- directories: stateless handles, nothing to sync -------------------

    fn opendir(
        &self,
        _req: &RequestInfo,
        id: PathBuf,
        _flags: OpenFlags,
    ) -> FuseResult<(OwnedFileHandle, FUSEOpenResponseFlags)> {
        let root = self.root.lock().unwrap();
        if !node(&root, &id)?.is_dir() {
            return Err(err(ErrorKind::NotADirectory));
        }
        Ok((Self::stateless_handle(), FUSEOpenResponseFlags::empty()))
    }

    fn releasedir(
        &self,
        _req: &RequestInfo,
        _id: PathBuf,
        _fh: OwnedFileHandle,
        _flags: OpenFlags,
    ) -> FuseResult<()> {
        Ok(())
    }

    fn fsyncdir(
        &self,
        _req: &RequestInfo,
        _id: PathBuf,
        _fh: BorrowedFileHandle<'_>,
        _datasync: bool,
    ) -> FuseResult<()> {
        Ok(())
    }

    // ---- out of scope for v1: answer ENOSYS via DefaultFuseHandler ---------
    // (symlinks, hard links, xattrs, locks, device maps, ioctl, lseek holes,
    // fallocate, copy_file_range — callers fall back or degrade.)

    delegate_fs! { unsupported, [
        bmap, copy_file_range, fallocate, getlk, setlk, getxattr, setxattr,
        listxattr, removexattr, ioctl, link, symlink, readlink, lseek
    ]}
}

// ---------------------------------------------------------------------------

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(mountpoint), None) = (args.next(), args.next()) else {
        eprintln!("usage: fuse-vfs-easy_fuser <mountpoint>");
        std::process::exit(2);
    };

    let options = [MountOption::FSName("fuse-vfs-easy_fuser".to_string())];
    // `mount` drives the FUSE session in the foreground and returns once the
    // filesystem is unmounted (fusermount3 -u).
    if let Err(e) = easy_fuser::fuse_parallel::mount(Vfs::new(), Path::new(&mountpoint), &options, None) {
        eprintln!("fuse-vfs-easy_fuser: mount failed: {e}");
        std::process::exit(1);
    }
}
