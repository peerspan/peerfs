//! A peerfs workspace as a file system, served over FUSE or loopback NFS.
//!
//! [`WorkspaceFs`] implements fuse-backend-rs's `FileSystem` on top of a [`Workspace`], so
//! the same code serves `/dev/fuse` on Linux and, later, vhost-user-fs for VMs. The NFS
//! server, for macOS, answers with the same code; see [`Frontend::Nfs`]. Every operation
//! takes the space's lock, so the mount and the local API see one workspace.
//!
//! - Reads come from the overlay's real file, or from the store for a file only in the
//!   base. Recently read base files are kept decoded.
//! - The first write, truncation, `chmod` or `touch` copies a base file into the overlay;
//!   later ones go straight to the real file. Data is durable once `fsync` returns, not
//!   after each `write`.
//! - Removing a base entry records a whiteout. Renaming a directory that holds base
//!   content fails with `EXDEV`, as on kernel overlay file systems, so `mv` copies instead.
//! - Inode numbers are given per path and follow renames.
//! - A base file's modification time is the workspace's stamp for its content at that
//!   path: the overlay file's mtime when a commit took it, or when the workspace first saw
//!   that content. Stamps persist, so neither a commit nor a new mount makes unchanged
//!   files look modified.
//! - The mount keeps one view of the workspace and reloads it only when the local API
//!   has changed its state or removals. It keeps recently used overlay files open, and
//!   closes them all when the local API changes the overlay's files.
//!
//! - A hard link shares one overlay file and one inode number between its paths, copying
//!   a base file up first. A commit takes each path's content, so afterwards they are
//!   separate files.
//!
//! Not supported: hard links to directories, special files, extended attributes, and file
//! locks beyond what the kernel does itself.

use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::fs::{self, File, FileTimes, OpenOptions as FsOpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse_backend_rs::abi::fuse_abi::{CreateIn, stat64, statvfs64};
use fuse_backend_rs::api::filesystem::{
    Context, DirEntry, Entry as FuseEntry, FileSystem, FsOptions, OpenOptions, SetattrValid,
    ZeroCopyReader, ZeroCopyWriter,
};
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::transport::{FuseChannel, FuseSession};
use peerfs_core::{
    Content, Entry, EntryKind, Error, FileMode, Located, Name, NewEntry, Space, TreePath, Workspace,
};

mod nfs;

/// How long the kernel may cache names and attributes. Short, because the local API can
/// change the workspace underneath the mount.
const TTL: Duration = Duration::from_secs(1);

/// Base files kept decoded for reads, and their total size.
const CACHED_FILES: usize = 16;
const CACHED_BYTES: usize = 256 * 1024 * 1024;

/// Overlay files kept open for reads and writes.
const OPEN_FILES: usize = 128;

/// Threads serving FUSE requests. Requests serialize on the space's lock anyway.
const THREADS: usize = 2;

/// The root's inode number, fixed by FUSE.
const ROOT: u64 = 1;

/// Inode numbers by path, and back. The paths of a hard link share a number; requests on
/// it use the first.
#[derive(Debug, Default)]
struct Inodes {
    paths: HashMap<u64, Vec<TreePath>>,
    numbers: HashMap<TreePath, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut inodes = Self {
            next: ROOT + 1,
            ..Self::default()
        };
        inodes.paths.insert(ROOT, vec![TreePath::root()]);
        inodes.numbers.insert(TreePath::root(), ROOT);
        inodes
    }

    fn path(&self, inode: u64) -> io::Result<TreePath> {
        self.paths
            .get(&inode)
            .and_then(|paths| paths.first())
            .cloned()
            .ok_or_else(|| errno(libc::ENOENT))
    }

    fn number(&mut self, path: &TreePath) -> u64 {
        if let Some(number) = self.numbers.get(path) {
            return *number;
        }
        let number = self.next;
        self.next += 1;
        self.paths.insert(number, vec![path.clone()]);
        self.numbers.insert(path.clone(), number);
        number
    }

    /// Gives `to` the number of `from`, forgetting any it had.
    fn link(&mut self, from: &TreePath, to: &TreePath) -> u64 {
        let number = self.number(from);
        self.forget(to);
        self.paths.entry(number).or_default().push(to.clone());
        self.numbers.insert(to.clone(), number);
        number
    }

    /// Forgets `path` if it is one of a link's paths. A removed file's sole path keeps its
    /// number, as before.
    fn unlink(&mut self, path: &TreePath) {
        let linked = self
            .numbers
            .get(path)
            .and_then(|number| self.paths.get(number))
            .is_some_and(|paths| paths.len() > 1);
        if linked {
            self.forget(path);
        }
    }

    fn forget(&mut self, path: &TreePath) {
        let Some(number) = self.numbers.remove(path) else {
            return;
        };
        if let Some(paths) = self.paths.get_mut(&number) {
            paths.retain(|other| other != path);
            if paths.is_empty() {
                self.paths.remove(&number);
            }
        }
    }

    /// Gives each path that no longer shares its link's file, as `file` identifies them, a
    /// number of its own on its next lookup.
    fn split_links(&mut self, mut file: impl FnMut(&TreePath) -> Option<(u64, u64)>) {
        let linked: Vec<Vec<TreePath>> = self
            .paths
            .values()
            .filter(|paths| paths.len() > 1)
            .cloned()
            .collect();
        for paths in linked {
            let files: Vec<_> = paths.iter().map(&mut file).collect();
            // Keep the paths that share the first overlay file found; with none, the first.
            let kept = files.iter().find_map(|file| *file);
            for (index, (path, file)) in paths.iter().zip(files).enumerate() {
                let keep = match kept {
                    Some(kept) => file == Some(kept),
                    None => index == 0,
                };
                if !keep {
                    self.forget(path);
                }
            }
        }
    }

    /// Moves `from` and everything under it to `to`; whatever was at `to` is forgotten.
    fn rename(&mut self, from: &TreePath, to: &TreePath) {
        let moved: Vec<(TreePath, u64)> = self
            .numbers
            .iter()
            .filter(|(path, _)| path.starts_with(from) || path.starts_with(to))
            .map(|(path, number)| (path.clone(), *number))
            .collect();
        for (path, _) in &moved {
            self.forget(path);
        }
        for (path, number) in moved {
            if !path.starts_with(from) {
                continue;
            }
            let mut renamed = to.clone();
            for name in &path.components()[from.components().len()..] {
                renamed = renamed.join(name);
            }
            self.paths.entry(number).or_default().push(renamed.clone());
            self.numbers.insert(renamed, number);
        }
    }
}

/// Decoded base files, oldest first.
#[derive(Debug, Default)]
struct Cache {
    files: VecDeque<(Content, Arc<[u8]>)>,
    bytes: usize,
}

impl Cache {
    fn get(&mut self, space: &Space, content: &Content) -> io::Result<Arc<[u8]>> {
        if let Some((_, bytes)) = self.files.iter().find(|(cached, _)| cached == content) {
            return Ok(Arc::clone(bytes));
        }
        let bytes: Arc<[u8]> = space.read_content(content).map_err(to_errno)?.into();
        self.bytes += bytes.len();
        self.files.push_back((*content, Arc::clone(&bytes)));
        while self.files.len() > CACHED_FILES || self.bytes > CACHED_BYTES {
            match self.files.pop_front() {
                Some((_, evicted)) => self.bytes -= evicted.len(),
                None => break,
            }
        }
        Ok(bytes)
    }
}

/// What a file is open for. Open for writing, it can be read too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Access {
    Read,
    Write,
}

/// Overlay files held open by path, so reads and writes neither reopen nor locate them.
/// The mount forgets a path before changing what is there itself, and every path when
/// another value of the workspace changes the overlay, so an open file is always the one at
/// its path.
#[derive(Debug, Default)]
struct OpenFiles {
    files: HashMap<TreePath, (File, Access)>,
}

impl OpenFiles {
    fn get(&self, path: &TreePath, access: Access) -> Option<&File> {
        self.files
            .get(path)
            .filter(|(_, open)| *open >= access)
            .map(|(file, _)| file)
    }

    fn open(&mut self, path: &TreePath, local: &Path, access: Access) -> io::Result<()> {
        let file = FsOpenOptions::new()
            .read(true)
            .write(access == Access::Write)
            .custom_flags(libc::O_NOFOLLOW)
            .open(local)?;
        if self.files.len() >= OPEN_FILES {
            self.files.clear();
        }
        self.files.insert(path.clone(), (file, access));
        Ok(())
    }

    /// Forgets `path` and everything under it.
    fn forget(&mut self, path: &TreePath) {
        self.files.retain(|open, _| !open.starts_with(path));
    }

    fn clear(&mut self) {
        self.files.clear();
    }
}

/// Where a write goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteAt {
    Offset(u64),
    /// The end of the file, as for a file opened to append.
    End,
}

/// What `fsync` makes durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sync {
    /// The data, and the metadata needed to read it back.
    Data,
    /// The data and all metadata.
    All,
}

/// What the mount remembers between requests.
#[derive(Debug)]
struct State {
    inodes: Inodes,
    /// The workspace as last read, reused while it is current.
    workspace: Option<Workspace>,
    cache: Cache,
    files: OpenFiles,
}

/// A workspace of a space, as a file system.
#[derive(Debug)]
pub struct WorkspaceFs {
    space: Arc<Mutex<Space>>,
    workspace: String,
    state: Mutex<State>,
    uid: u32,
    gid: u32,
}

impl WorkspaceFs {
    /// Serves `workspace` of `space`, which the caller shares with anything else that uses
    /// the space.
    pub fn new(space: Arc<Mutex<Space>>, workspace: &str) -> Self {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        // SAFETY: getgid has no preconditions and cannot fail.
        let gid = unsafe { libc::getgid() };
        Self {
            space,
            workspace: workspace.to_owned(),
            state: Mutex::new(State {
                inodes: Inodes::new(),
                workspace: None,
                cache: Cache::default(),
                files: OpenFiles::default(),
            }),
            uid,
            gid,
        }
    }

    /// Runs `work` with the space, a fresh view of the workspace, and the mount's state.
    fn with<T>(
        &self,
        work: impl FnOnce(&mut Space, &mut Workspace, &mut State) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut space = lock(&self.space);
        let mut state = lock(&self.state);
        let mut workspace = match state.workspace.take() {
            Some(workspace) if workspace.is_current() => workspace,
            _ => {
                let workspace = space.workspace(&self.workspace).map_err(to_errno)?;
                state.files.clear();
                // A commit or the local API may have turned a link's paths into separate
                // files.
                state
                    .inodes
                    .split_links(|path| match workspace.locate(&space, path) {
                        Ok(Some(Located::Local(local))) => fs::symlink_metadata(local)
                            .ok()
                            .map(|meta| (meta.dev(), meta.ino())),
                        _ => None,
                    });
                workspace
            }
        };
        if workspace.overlay_changed_elsewhere() {
            state.files.clear();
        }
        let done = work(&mut space, &mut workspace, &mut state);
        state.workspace = Some(workspace);
        done
    }

    fn attr(
        &self,
        space: &Space,
        workspace: &mut Workspace,
        state: &mut State,
        path: &TreePath,
    ) -> io::Result<stat64> {
        let number = state.inodes.number(path);
        let located = workspace
            .locate(space, path)
            .map_err(to_errno)?
            .ok_or_else(|| errno(libc::ENOENT))?;
        // SAFETY: stat64 is a plain C struct for which all zeroes is a valid value.
        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = number;
        st.st_uid = self.uid;
        st.st_gid = self.gid;
        st.st_blksize = 4096;
        match located {
            Located::Local(local) => {
                let meta = fs::symlink_metadata(&local)?;
                st.st_mode = meta.mode();
                st.st_nlink = meta.nlink();
                st.st_size = to_i64(meta.size());
                st.st_mtime = meta.mtime();
                st.st_mtime_nsec = meta.mtime_nsec();
            }
            Located::Base(entry) => {
                let (mode, size) = match &entry {
                    Entry::File { content, mode } => {
                        let bits = match mode {
                            FileMode::Regular => 0o644,
                            FileMode::Executable => 0o755,
                        };
                        (
                            libc::S_IFREG | bits,
                            space.content_size(content).map_err(to_errno)?,
                        )
                    }
                    Entry::Symlink(target) => (libc::S_IFLNK | 0o777, len_u64(target.len())),
                    Entry::Tree(_) => (libc::S_IFDIR | 0o755, 4096),
                };
                let stamped = workspace.stamp(path, &entry).map_err(to_errno)?;
                let since = stamped.duration_since(UNIX_EPOCH).unwrap_or_default();
                st.st_mode = mode;
                st.st_size = to_i64(size);
                st.st_mtime = to_i64(since.as_secs());
                st.st_mtime_nsec = i64::from(since.subsec_nanos());
            }
        }
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            st.st_nlink = 2;
        } else if st.st_nlink == 0 {
            st.st_nlink = 1;
        }
        st.st_blocks = (st.st_size + 511) / 512;
        st.st_atime = st.st_mtime;
        st.st_atime_nsec = st.st_mtime_nsec;
        st.st_ctime = st.st_mtime;
        st.st_ctime_nsec = st.st_mtime_nsec;
        Ok(st)
    }

    fn entry(
        &self,
        space: &Space,
        workspace: &mut Workspace,
        state: &mut State,
        path: &TreePath,
    ) -> io::Result<FuseEntry> {
        let attr = self.attr(space, workspace, state, path)?;
        Ok(FuseEntry {
            inode: attr.st_ino,
            generation: 0,
            attr,
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    /// The inode number of the directory holding `inode`; the root's is its own.
    fn parent(&self, inode: u64) -> io::Result<u64> {
        self.with(|_, _, state| {
            let path = state.inodes.path(inode)?;
            Ok(path
                .split_last()
                .map_or(ROOT, |(parent, _)| state.inodes.number(&parent)))
        })
    }

    /// Reads up to `size` bytes at `offset` of the file `inode`, fewer only at its end.
    fn read_bytes(&self, inode: u64, size: u32, offset: u64) -> io::Result<Vec<u8>> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            if let Some(file) = state.files.get(&path, Access::Read) {
                return read_at(file, size, offset);
            }
            match workspace.locate(space, &path).map_err(to_errno)? {
                Some(Located::Local(local)) => {
                    state.files.open(&path, &local, Access::Read)?;
                    let file = state
                        .files
                        .get(&path, Access::Read)
                        .ok_or_else(|| errno(libc::EIO))?;
                    read_at(file, size, offset)
                }
                Some(Located::Base(Entry::File { content, .. })) => {
                    let bytes = state.cache.get(space, &content)?;
                    let start = to_usize_saturating(offset).min(bytes.len());
                    let end = start.saturating_add(to_usize(size)).min(bytes.len());
                    Ok(bytes[start..end].to_vec())
                }
                Some(Located::Base(Entry::Tree(_))) => Err(errno(libc::EISDIR)),
                Some(Located::Base(Entry::Symlink(_))) => Err(errno(libc::EINVAL)),
                None => Err(errno(libc::ENOENT)),
            }
        })
    }

    /// Writes `data` to the file `inode`, at `at`.
    fn write_bytes(&self, inode: u64, at: WriteAt, data: &[u8]) -> io::Result<usize> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            let WriteAt::Offset(offset) = at else {
                let local = workspace.copy_up(space, &path).map_err(to_errno)?;
                FsOpenOptions::new()
                    .append(true)
                    .open(&local)?
                    .write_all(data)?;
                return Ok(data.len());
            };
            if state.files.get(&path, Access::Write).is_none() {
                let local = workspace.copy_up(space, &path).map_err(to_errno)?;
                state.files.open(&path, &local, Access::Write)?;
            }
            state
                .files
                .get(&path, Access::Write)
                .ok_or_else(|| errno(libc::EIO))?
                .write_all_at(data, offset)?;
            Ok(data.len())
        })
    }

    /// The directory `inode`'s entries, `.` and `..` first: inode number, type and name.
    fn entries(&self, inode: u64) -> io::Result<Vec<(u64, u8, Vec<u8>)>> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            let parent = path
                .split_last()
                .map_or(ROOT, |(parent, _)| state.inodes.number(&parent));
            let mut entries = vec![
                (inode, libc::DT_DIR, b".".to_vec()),
                (parent, libc::DT_DIR, b"..".to_vec()),
            ];
            for (name, kind) in workspace.list(space, &path.to_string()).map_err(to_errno)? {
                let child = child_named(&path, &name)?;
                let kind = match kind {
                    EntryKind::File => libc::DT_REG,
                    EntryKind::Symlink => libc::DT_LNK,
                    EntryKind::Directory => libc::DT_DIR,
                };
                entries.push((state.inodes.number(&child), kind, name.into_bytes()));
            }
            Ok(entries)
        })
    }

    /// Syncs the file `inode` as `what` says.
    fn sync(&self, inode: u64, what: Sync) -> io::Result<()> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            if let Some(Located::Local(local)) = workspace.locate(space, &path).map_err(to_errno)? {
                let file = File::open(&local)?;
                match what {
                    Sync::Data => file.sync_data()?,
                    Sync::All => file.sync_all()?,
                }
                if let Some(parent) = local.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Ok(())
        })
    }

    /// Creates `name` in `parent` and returns its entry.
    fn make(&self, parent: u64, name: &CStr, new: NewEntry<'_>) -> io::Result<FuseEntry> {
        self.with(|space, workspace, state| {
            let path = child(&state.inodes.path(parent)?, name)?;
            state.files.forget(&path);
            workspace
                .create_entry(space, &path, new)
                .map_err(to_errno)?;
            self.entry(space, workspace, state, &path)
        })
    }
}

impl FileSystem for WorkspaceFs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // Writes of up to 1 MiB per request, instead of one request per page.
        Ok(capable & (FsOptions::BIG_WRITES | FsOptions::MAX_PAGES))
    }

    fn lookup(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<FuseEntry> {
        self.with(|space, workspace, state| {
            let path = child(&state.inodes.path(parent)?, name)?;
            self.entry(space, workspace, state, &path)
        })
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> io::Result<(stat64, Duration)> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            Ok((self.attr(space, workspace, state, &path)?, TTL))
        })
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: u64,
        attr: stat64,
        _handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            let changes = SetattrValid::MODE
                | SetattrValid::SIZE
                | SetattrValid::ATIME
                | SetattrValid::MTIME
                | SetattrValid::ATIME_NOW
                | SetattrValid::MTIME_NOW;
            if valid.intersects(changes) {
                let local = workspace.copy_up(space, &path).map_err(to_errno)?;
                if valid.contains(SetattrValid::SIZE) {
                    let file = FsOpenOptions::new().write(true).open(&local)?;
                    file.set_len(u64::try_from(attr.st_size).map_err(|_| errno(libc::EINVAL))?)?;
                }
                if valid.contains(SetattrValid::MODE) {
                    fs::set_permissions(&local, fs::Permissions::from_mode(attr.st_mode & 0o7777))?;
                }
                let times = set_times(&attr, valid);
                if times.is_some() && !fs::symlink_metadata(&local)?.is_symlink() {
                    File::open(&local)?.set_times(times.unwrap_or_default())?;
                }
            }
            Ok((self.attr(space, workspace, state, &path)?, TTL))
        })
    }

    fn readlink(&self, _ctx: &Context, inode: u64) -> io::Result<Vec<u8>> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            match workspace.locate(space, &path).map_err(to_errno)? {
                Some(Located::Local(local)) => {
                    Ok(fs::read_link(local)?.into_os_string().into_encoded_bytes())
                }
                Some(Located::Base(Entry::Symlink(target))) => Ok(target.into_bytes()),
                Some(_) => Err(errno(libc::EINVAL)),
                None => Err(errno(libc::ENOENT)),
            }
        })
    }

    fn symlink(
        &self,
        _ctx: &Context,
        linkname: &CStr,
        parent: u64,
        name: &CStr,
    ) -> io::Result<FuseEntry> {
        let target = linkname.to_str().map_err(|_| errno(libc::EINVAL))?;
        self.make(parent, name, NewEntry::Symlink(target))
    }

    fn mknod(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        umask: u32,
    ) -> io::Result<FuseEntry> {
        if mode & libc::S_IFMT != libc::S_IFREG {
            return Err(errno(libc::EPERM));
        }
        let executable = mode & !umask & 0o111 != 0;
        self.make(parent, name, NewEntry::File { executable })
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        _mode: u32,
        _umask: u32,
    ) -> io::Result<FuseEntry> {
        self.make(parent, name, NewEntry::Directory)
    }

    fn unlink(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.with(|space, workspace, state| {
            let path = child(&state.inodes.path(parent)?, name)?;
            if is_dir(workspace.locate(space, &path).map_err(to_errno)?.as_ref()) {
                return Err(errno(libc::EISDIR));
            }
            state.files.forget(&path);
            workspace
                .remove(space, &path.to_string())
                .map_err(to_errno)?;
            state.inodes.unlink(&path);
            Ok(())
        })
    }

    fn rmdir(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.with(|space, workspace, state| {
            let path = child(&state.inodes.path(parent)?, name)?;
            match workspace.locate(space, &path).map_err(to_errno)? {
                None => return Err(errno(libc::ENOENT)),
                Some(located) if !is_dir(Some(&located)) => return Err(errno(libc::ENOTDIR)),
                Some(_) => {}
            }
            state.files.forget(&path);
            workspace.remove_dir(space, &path).map_err(to_errno)
        })
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
        if flags & libc::RENAME_EXCHANGE != 0 {
            return Err(errno(libc::EINVAL));
        }
        self.with(|space, workspace, state| {
            let from = child(&state.inodes.path(olddir)?, oldname)?;
            let to = child(&state.inodes.path(newdir)?, newname)?;
            if flags & libc::RENAME_NOREPLACE != 0
                && workspace.locate(space, &to).map_err(to_errno)?.is_some()
            {
                return Err(errno(libc::EEXIST));
            }
            state.files.forget(&from);
            state.files.forget(&to);
            workspace.rename(space, &from, &to).map_err(to_errno)?;
            state.inodes.rename(&from, &to);
            Ok(())
        })
    }

    fn link(
        &self,
        _ctx: &Context,
        inode: u64,
        newparent: u64,
        newname: &CStr,
    ) -> io::Result<FuseEntry> {
        self.with(|space, workspace, state| {
            let from = state.inodes.path(inode)?;
            let to = child(&state.inodes.path(newparent)?, newname)?;
            state.files.forget(&to);
            workspace
                .link(space, &from, &to)
                .map_err(|error| match error {
                    Error::WrongEntry {
                        found: "directory", ..
                    } => errno(libc::EPERM),
                    error => to_errno(error),
                })?;
            state.inodes.link(&from, &to);
            self.entry(space, workspace, state, &to)
        })
    }

    fn open(
        &self,
        _ctx: &Context,
        _inode: u64,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions, Option<u32>)> {
        Ok((None, OpenOptions::empty(), None))
    }

    fn create(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(FuseEntry, Option<u64>, OpenOptions, Option<u32>)> {
        let executable = args.mode & !args.umask & 0o111 != 0;
        let entry = self.make(parent, name, NewEntry::File { executable })?;
        Ok((entry, None, OpenOptions::empty(), None))
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
        let bytes = self.read_bytes(inode, size, offset)?;
        w.write_all(&bytes)?;
        Ok(bytes.len())
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
        let mut data = vec![0; to_usize(size)];
        r.read_exact(&mut data)?;
        let at = if flags & u32::try_from(libc::O_APPEND).unwrap_or(0) == 0 {
            WriteAt::Offset(offset)
        } else {
            WriteAt::End
        };
        self.write_bytes(inode, at, &data)
    }

    fn flush(&self, _ctx: &Context, _inode: u64, _handle: u64, _lock_owner: u64) -> io::Result<()> {
        Ok(())
    }

    fn fsync(&self, _ctx: &Context, inode: u64, datasync: bool, _handle: u64) -> io::Result<()> {
        self.sync(inode, if datasync { Sync::Data } else { Sync::All })
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
        let dir = lock(&self.space).path().to_owned();
        statvfs(&dir)
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry<'_>) -> io::Result<usize>,
    ) -> io::Result<()> {
        let entries = self.entries(inode)?;
        for (index, (ino, kind, name)) in
            entries.iter().enumerate().skip(to_usize_saturating(offset))
        {
            let added = add_entry(DirEntry {
                ino: *ino,
                offset: len_u64(index + 1),
                type_: u32::from(*kind),
                name,
            })?;
            if added == 0 {
                break;
            }
        }
        Ok(())
    }
}

/// How a workspace is mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Frontend {
    /// FUSE, on Linux.
    #[default]
    Fuse,
    /// An NFS version 3 server on the loopback interface, mounted by the kernel's client: for
    /// macOS, and testable on Linux. Mounting needs the privileges `mount` needs.
    Nfs,
}

/// A workspace mounted at a directory, served by threads of this process. Dropping it
/// unmounts.
pub struct Mount {
    path: PathBuf,
    served: Served,
}

enum Served {
    Fuse {
        session: Option<FuseSession>,
        threads: Vec<JoinHandle<()>>,
    },
    Nfs(nfs::NfsMount),
}

impl Mount {
    /// Mounts `workspace` of `space` at `path`, an existing empty directory, through
    /// `frontend`.
    ///
    /// # Errors
    ///
    /// Fails if `path` is not an empty directory, or the kernel refuses the mount.
    pub fn new(
        space: Arc<Mutex<Space>>,
        workspace: &str,
        path: &Path,
        frontend: Frontend,
    ) -> io::Result<Self> {
        if fs::read_dir(path)?.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                format!("{} is not empty", path.display()),
            ));
        }
        let fs = WorkspaceFs::new(space, workspace);
        let served = match frontend {
            Frontend::Fuse => mount_fuse(fs, workspace, path)?,
            Frontend::Nfs => Served::Nfs(nfs::NfsMount::new(fs, workspace, path)?),
        };
        Ok(Self {
            path: path.to_owned(),
            served,
        })
    }

    /// Where the workspace is mounted.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How the workspace is mounted.
    pub fn frontend(&self) -> Frontend {
        match self.served {
            Served::Fuse { .. } => Frontend::Fuse,
            Served::Nfs(_) => Frontend::Nfs,
        }
    }

    /// Unmounts and waits for the serving threads to finish.
    ///
    /// # Errors
    ///
    /// Fails if the kernel refuses to unmount, as when a process has a file open there.
    pub fn unmount(mut self) -> io::Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> io::Result<()> {
        match &mut self.served {
            Served::Fuse { session, threads } => {
                if let Some(mut mounted) = session.take() {
                    let unmounted = mounted.umount().map_err(|e| fuse_error(&e));
                    if let Err(e) = unmounted {
                        *session = Some(mounted);
                        return Err(e);
                    }
                    mounted.wake().map_err(|e| fuse_error(&e))?;
                }
                for thread in threads.drain(..) {
                    let _ = thread.join();
                }
                Ok(())
            }
            Served::Nfs(mount) => mount.stop(&self.path),
        }
    }
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mount")
            .field("path", &self.path)
            .field("frontend", &self.frontend())
            .finish_non_exhaustive()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn mount_fuse(fs: WorkspaceFs, workspace: &str, path: &Path) -> io::Result<Served> {
    let server = Arc::new(Server::new(Arc::new(fs)));
    let mut session =
        FuseSession::new(path, "peerfs", workspace, false).map_err(|e| fuse_error(&e))?;
    session.set_allow_other(false);
    session.mount().map_err(|e| fuse_error(&e))?;
    let mut threads = Vec::new();
    for _ in 0..THREADS {
        let channel = session.new_channel().map_err(|e| fuse_error(&e))?;
        let server = Arc::clone(&server);
        threads.push(
            thread::Builder::new()
                .name("peerfs-fuse".to_owned())
                .spawn(move || serve(&server, channel))?,
        );
    }
    Ok(Served::Fuse {
        session: Some(session),
        threads,
    })
}

fn serve(server: &Server<Arc<WorkspaceFs>>, mut channel: FuseChannel) {
    loop {
        match channel.get_request() {
            Ok(Some((reader, writer))) => {
                if let Err(fuse_backend_rs::Error::EncodeMessage(_)) =
                    server.handle_message(reader, writer.into(), None, None)
                {
                    // The kernel closed the session.
                    return;
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

/// Reads up to `size` bytes at `offset`, fewer only at the end of the file.
fn read_at(file: &File, size: u32, offset: u64) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0; to_usize(size)];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read_at(&mut buffer[filled..], offset + len_u64(filled))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn child(parent: &TreePath, name: &CStr) -> io::Result<TreePath> {
    child_named(parent, name.to_str().map_err(|_| errno(libc::EINVAL))?)
}

fn child_named(parent: &TreePath, name: &str) -> io::Result<TreePath> {
    let name = Name::try_from(name).map_err(|_| errno(libc::EINVAL))?;
    Ok(parent.join(&name))
}

fn is_dir(located: Option<&Located>) -> bool {
    match located {
        Some(Located::Local(local)) => fs::symlink_metadata(local).is_ok_and(|meta| meta.is_dir()),
        Some(Located::Base(entry)) => matches!(entry, Entry::Tree(_)),
        None => false,
    }
}

/// The access and modification times a `setattr` asks for, if any.
fn set_times(attr: &stat64, valid: SetattrValid) -> Option<FileTimes> {
    let at = |seconds: i64, nanos: i64| {
        let since = Duration::new(
            u64::try_from(seconds).unwrap_or(0),
            u32::try_from(nanos).unwrap_or(0),
        );
        UNIX_EPOCH + since
    };
    let mut times = FileTimes::new();
    let mut any = false;
    if valid.contains(SetattrValid::ATIME_NOW) {
        times = times.set_accessed(SystemTime::now());
        any = true;
    } else if valid.contains(SetattrValid::ATIME) {
        times = times.set_accessed(at(attr.st_atime, attr.st_atime_nsec));
        any = true;
    }
    if valid.contains(SetattrValid::MTIME_NOW) {
        times = times.set_modified(SystemTime::now());
        any = true;
    } else if valid.contains(SetattrValid::MTIME) {
        times = times.set_modified(at(attr.st_mtime, attr.st_mtime_nsec));
        any = true;
    }
    any.then_some(times)
}

fn statvfs(dir: &Path) -> io::Result<statvfs64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
        .map_err(|_| errno(libc::EINVAL))?;
    // SAFETY: statvfs64 is a plain C struct for which all zeroes is a valid value.
    let mut out: statvfs64 = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string and `out` a valid statvfs64 to fill.
    if unsafe { libc::statvfs64(path.as_ptr(), &raw mut out) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out)
}

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

/// The error a space error means to a file system caller: the underlying one for a file
/// system failure, otherwise the error number for its kind.
fn to_errno(error: Error) -> io::Error {
    let code = match &error {
        Error::NotFound(_) | Error::NoSuchWorkspace(_) => libc::ENOENT,
        Error::Exists(_) => libc::EEXIST,
        Error::NotEmpty(_) => libc::ENOTEMPTY,
        Error::DirectoryRename(_) => libc::EXDEV,
        Error::WrongEntry {
            found: "directory", ..
        } => libc::EISDIR,
        Error::WrongEntry { .. } => libc::ENOTDIR,
        Error::InvalidName(_) | Error::InvalidPath(_) => libc::EINVAL,
        _ => libc::EIO,
    };
    match error {
        Error::Io { source, .. } => source,
        _ => errno(code),
    }
}

fn fuse_error(error: &fuse_backend_rs::transport::Error) -> io::Error {
    io::Error::other(error.to_string())
}

fn len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn to_usize_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
