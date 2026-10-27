//! A peerfs workspace as a file system, served over FUSE.
//!
//! [`WorkspaceFs`] implements fuse-backend-rs's `FileSystem` on top of a [`Workspace`], so
//! the same code serves `/dev/fuse` on Linux and, later, vhost-user-fs for VMs. Every
//! operation takes the space's lock, so the mount and the local API see one workspace.
//!
//! - Reads come from the overlay's real file, or from the store for a file only in the
//!   base. Recently read base files are kept decoded.
//! - The first write, truncation, `chmod` or `touch` copies a base file into the overlay;
//!   later ones go straight to the real file. Data is durable once `fsync` returns, not
//!   after each `write`.
//! - Removing a base entry records a whiteout. Renaming a directory that holds base
//!   content fails with `EXDEV`, as on kernel overlay file systems, so `mv` copies instead.
//! - Inode numbers are given per path and follow renames.
//! - A base file's modification time is when this mount first saw its content at that
//!   path, so committing or pulling changes it only where the content changed.
//!
//! Not supported: hard links, special files, extended attributes, and file locks beyond
//! what the kernel does itself.

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

/// How long the kernel may cache names and attributes. Short, because the local API can
/// change the workspace underneath the mount.
const TTL: Duration = Duration::from_secs(1);

/// Base files kept decoded for reads, and their total size.
const CACHED_FILES: usize = 16;
const CACHED_BYTES: usize = 256 * 1024 * 1024;

/// Threads serving FUSE requests. Requests serialize on the space's lock anyway.
const THREADS: usize = 2;

/// The root's inode number, fixed by FUSE.
const ROOT: u64 = 1;

/// Inode numbers by path, and back.
#[derive(Debug, Default)]
struct Inodes {
    paths: HashMap<u64, TreePath>,
    numbers: HashMap<TreePath, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut inodes = Self {
            next: ROOT + 1,
            ..Self::default()
        };
        inodes.paths.insert(ROOT, TreePath::root());
        inodes.numbers.insert(TreePath::root(), ROOT);
        inodes
    }

    fn path(&self, inode: u64) -> io::Result<TreePath> {
        self.paths
            .get(&inode)
            .cloned()
            .ok_or_else(|| errno(libc::ENOENT))
    }

    fn number(&mut self, path: &TreePath) -> u64 {
        if let Some(number) = self.numbers.get(path) {
            return *number;
        }
        let number = self.next;
        self.next += 1;
        self.paths.insert(number, path.clone());
        self.numbers.insert(path.clone(), number);
        number
    }

    /// Moves `from` and everything under it to `to`; whatever was at `to` is forgotten.
    fn rename(&mut self, from: &TreePath, to: &TreePath) {
        let moved: Vec<(TreePath, u64)> = self
            .numbers
            .iter()
            .filter(|(path, _)| path.starts_with(from) || path.starts_with(to))
            .map(|(path, number)| (path.clone(), *number))
            .collect();
        for (path, number) in &moved {
            self.numbers.remove(path);
            self.paths.remove(number);
        }
        for (path, number) in moved {
            if !path.starts_with(from) {
                continue;
            }
            let mut renamed = to.clone();
            for name in &path.components()[from.components().len()..] {
                renamed = renamed.join(name);
            }
            self.paths.insert(number, renamed.clone());
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

/// What the mount remembers between requests.
#[derive(Debug)]
struct State {
    inodes: Inodes,
    /// When each base entry was first seen at its path.
    seen: HashMap<TreePath, (Entry, SystemTime)>,
    cache: Cache,
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
                seen: HashMap::new(),
                cache: Cache::default(),
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
        let mut workspace = space.workspace(&self.workspace).map_err(to_errno)?;
        let mut state = lock(&self.state);
        work(&mut space, &mut workspace, &mut state)
    }

    fn attr(
        &self,
        space: &Space,
        workspace: &Workspace,
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
                let seen = match state.seen.get(path) {
                    Some((known, at)) if *known == entry => *at,
                    _ => {
                        let now = SystemTime::now();
                        state.seen.insert(path.clone(), (entry, now));
                        now
                    }
                };
                let since = seen.duration_since(UNIX_EPOCH).unwrap_or_default();
                st.st_mode = mode;
                st.st_size = to_i64(size);
                st.st_mtime = to_i64(since.as_secs());
                st.st_mtime_nsec = i64::from(since.subsec_nanos());
            }
        }
        st.st_nlink = if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            2
        } else {
            1
        };
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
        workspace: &Workspace,
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

    /// Creates `name` in `parent` and returns its entry.
    fn make(&self, parent: u64, name: &CStr, new: NewEntry<'_>) -> io::Result<FuseEntry> {
        self.with(|space, workspace, state| {
            let path = child(&state.inodes.path(parent)?, name)?;
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

    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        Ok(FsOptions::empty())
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
            workspace.remove(space, &path.to_string()).map_err(to_errno)
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
            workspace.rename(space, &from, &to).map_err(to_errno)?;
            state.inodes.rename(&from, &to);
            Ok(())
        })
    }

    fn link(
        &self,
        _ctx: &Context,
        _inode: u64,
        _newparent: u64,
        _newname: &CStr,
    ) -> io::Result<FuseEntry> {
        Err(errno(libc::EPERM))
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
        let bytes = self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            match workspace.locate(space, &path).map_err(to_errno)? {
                Some(Located::Local(local)) => {
                    let file = File::open(local)?;
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
        })?;
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
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            let local = workspace.copy_up(space, &path).map_err(to_errno)?;
            if flags & u32::try_from(libc::O_APPEND).unwrap_or(0) != 0 {
                FsOpenOptions::new()
                    .append(true)
                    .open(&local)?
                    .write_all(&data)?;
            } else {
                FsOpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&local)?
                    .write_all_at(&data, offset)?;
            }
            Ok(data.len())
        })
    }

    fn flush(&self, _ctx: &Context, _inode: u64, _handle: u64, _lock_owner: u64) -> io::Result<()> {
        Ok(())
    }

    fn fsync(&self, _ctx: &Context, inode: u64, datasync: bool, _handle: u64) -> io::Result<()> {
        self.with(|space, workspace, state| {
            let path = state.inodes.path(inode)?;
            if let Some(Located::Local(local)) = workspace.locate(space, &path).map_err(to_errno)? {
                let file = File::open(&local)?;
                if datasync {
                    file.sync_data()?;
                } else {
                    file.sync_all()?;
                }
                if let Some(parent) = local.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Ok(())
        })
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
        let entries = self.with(|space, workspace, state| {
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
        })?;
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

/// A workspace mounted at a directory, served by threads of this process. Dropping it
/// unmounts.
pub struct Mount {
    path: PathBuf,
    session: Option<FuseSession>,
    threads: Vec<JoinHandle<()>>,
}

impl Mount {
    /// Mounts `workspace` of `space` at `path`, an existing empty directory.
    ///
    /// # Errors
    ///
    /// Fails if `path` is not an empty directory, or the kernel refuses the mount.
    pub fn new(space: Arc<Mutex<Space>>, workspace: &str, path: &Path) -> io::Result<Self> {
        if fs::read_dir(path)?.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                format!("{} is not empty", path.display()),
            ));
        }
        let server = Arc::new(Server::new(Arc::new(WorkspaceFs::new(space, workspace))));
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
        Ok(Self {
            path: path.to_owned(),
            session: Some(session),
            threads,
        })
    }

    /// Where the workspace is mounted.
    pub fn path(&self) -> &Path {
        &self.path
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
        if let Some(mut session) = self.session.take() {
            let unmounted = session.umount().map_err(|e| fuse_error(&e));
            if let Err(e) = unmounted {
                self.session = Some(session);
                return Err(e);
            }
            session.wake().map_err(|e| fuse_error(&e))?;
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        Ok(())
    }
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mount")
            .field("path", &self.path)
            .field("mounted", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = self.stop();
    }
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
