//! A workspace served over NFS version 3 on the loopback interface, for macOS, where there is no
//! FUSE without a kernel extension; it runs on Linux too, which is where it is tested.
//!
//! [`NfsFs`] answers the vendored `nfsserve`'s calls with the same [`WorkspaceFs`] the FUSE
//! mount uses, so the two behave alike. What differs:
//!
//! - NFS is stateless: there is no `forget` or `open`, so inode numbers, which are the
//!   file handles, stay known for as long as the mount is served.
//! - Writes are acknowledged as unstable and a COMMIT syncs the file. Clients send one when
//!   a written file is closed as well as on `fsync`.
//! - The server listens on `127.0.0.1` at a port of its own and accepts connections only
//!   from reserved ports, which only privileged processes such as the kernel's NFS client
//!   can bind. The export is named after the workspace.
//! - The client is mounted `soft`, so if this process dies, calls in the mount fail after a
//!   timeout rather than hang.

use std::ffi::CString;
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use async_trait::async_trait;
use fuse_backend_rs::abi::fuse_abi::stat64;
use fuse_backend_rs::api::filesystem::{Context, FileSystem, SetattrValid};
use nfsserve::nfs::{
    FSF_CANSETTIME, FSF_HOMOGENEOUS, FSF_LINK, FSF_SYMLINK, fattr3, fileid3, filename3, fsinfo3,
    ftype3, nfspath3, nfsstat3, nfstime3, post_op_attr, sattr3, set_atime, set_mode3, set_mtime,
    set_size3, specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use peerfs_core::NewEntry;
use tokio::sync::oneshot;

use crate::{ROOT, Sync, WorkspaceFs, WriteAt, len_u64};

/// How long the client may cache attributes, in seconds; as short as FUSE's, because the
/// local API can change the workspace underneath the mount.
const ATTRIBUTE_SECONDS: u32 = 1;

/// A workspace as an NFS file system.
#[derive(Debug)]
pub(crate) struct NfsFs {
    fs: WorkspaceFs,
}

impl NfsFs {
    pub(crate) fn new(fs: WorkspaceFs) -> Self {
        Self { fs }
    }

    fn attr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let (st, _) = self
            .fs
            .getattr(&Context::new(), id, None)
            .map_err(|e| to_status(&e))?;
        Ok(fattr(&st))
    }

    /// Creates `name` in `dirid` and returns its number and attributes.
    fn make(
        &self,
        dirid: fileid3,
        name: &filename3,
        new: NewEntry<'_>,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let entry = self
            .fs
            .make(dirid, &c_name(name)?, new)
            .map_err(|e| to_status(&e))?;
        Ok((entry.inode, fattr(&entry.attr)))
    }
}

#[async_trait]
impl NFSFileSystem for NfsFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        match filename.as_ref() {
            b"." => Ok(dirid),
            b".." => self.fs.parent(dirid).map_err(|e| to_status(&e)),
            _ => {
                let entry = self
                    .fs
                    .lookup(&Context::new(), dirid, &c_name(filename)?)
                    .map_err(|e| to_status(&e))?;
                Ok(entry.inode)
            }
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.attr(id)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let (st, valid) = to_setattr(&setattr);
        if valid.is_empty() {
            return self.attr(id);
        }
        let (st, _) = self
            .fs
            .setattr(&Context::new(), id, st, None, valid)
            .map_err(|e| to_status(&e))?;
        Ok(fattr(&st))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let bytes = self
            .fs
            .read_bytes(id, count, offset)
            .map_err(|e| to_status(&e))?;
        // A read comes back short only at the end of the file.
        let end = len_u64(bytes.len()) < u64::from(count);
        Ok((bytes, end))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.fs
            .write_bytes(id, WriteAt::Offset(offset), data)
            .map_err(|e| to_status(&e))?;
        self.attr(id)
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let executable = matches!(attr.mode, set_mode3::mode(mode) if mode & 0o111 != 0);
        let id = match self.make(dirid, filename, NewEntry::File { executable }) {
            Ok((id, _)) => id,
            // An unchecked create of an existing file sets its attributes.
            Err(nfsstat3::NFS3ERR_EXIST) => self.lookup(dirid, filename).await?,
            Err(status) => return Err(status),
        };
        let attr = sattr3 {
            mode: set_mode3::Void,
            ..attr
        };
        Ok((id, self.setattr(id, attr).await?))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let (id, _) = self.make(dirid, filename, NewEntry::File { executable: false })?;
        Ok(id)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.make(dirid, dirname, NewEntry::Directory)
    }

    /// Serves both REMOVE and RMDIR.
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = c_name(filename)?;
        let id = self.lookup(dirid, filename).await?;
        let context = Context::new();
        if matches!(self.attr(id)?.ftype, ftype3::NF3DIR) {
            self.fs.rmdir(&context, dirid, &name)
        } else {
            self.fs.unlink(&context, dirid, &name)
        }
        .map_err(|e| to_status(&e))
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.fs
            .rename(
                &Context::new(),
                from_dirid,
                &c_name(from_filename)?,
                to_dirid,
                &c_name(to_filename)?,
                0,
            )
            .map_err(|e| to_status(&e))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let entries = self.fs.entries(dirid).map_err(|e| to_status(&e))?;
        // `.` and `..` come first; NFS clients make their own.
        let mut entries = entries.into_iter().skip(2).peekable();
        if start_after != 0 {
            // The cookie is the last number the client saw. The paths of a hard link in one
            // directory share a number, so a page ending between them repeats the second.
            if !entries.any(|(number, _, _)| number == start_after) {
                return Err(nfsstat3::NFS3ERR_BAD_COOKIE);
            }
        }
        let mut result = ReadDirResult::default();
        while result.entries.len() < max_entries {
            let Some((fileid, _, name)) = entries.next() else {
                break;
            };
            result.entries.push(DirEntry {
                fileid,
                name: name.into(),
                attr: self.attr(fileid)?,
            });
        }
        result.end = entries.peek().is_none();
        Ok(result)
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let target = std::str::from_utf8(symlink).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        self.make(dirid, linkname, NewEntry::Symlink(target))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let target = self
            .fs
            .readlink(&Context::new(), id)
            .map_err(|e| to_status(&e))?;
        Ok(target.into())
    }

    async fn link(
        &self,
        id: fileid3,
        dirid: fileid3,
        linkname: &filename3,
    ) -> Result<fattr3, nfsstat3> {
        let entry = self
            .fs
            .link(&Context::new(), id, dirid, &c_name(linkname)?)
            .map_err(|e| to_status(&e))?;
        Ok(fattr(&entry.attr))
    }

    fn unstable_writes(&self) -> bool {
        true
    }

    async fn commit(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.fs.sync(id, Sync::All).map_err(|e| to_status(&e))?;
        self.attr(id)
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let size = 1024 * 1024;
        Ok(fsinfo3 {
            obj_attributes: post_op_attr::attributes(self.attr(root_fileid)?),
            rtmax: size,
            rtpref: size,
            rtmult: 4096,
            wtmax: size,
            wtpref: size,
            wtmult: 4096,
            dtpref: 64 * 1024,
            maxfilesize: u64::MAX >> 1,
            time_delta: nfstime3 {
                seconds: 0,
                nseconds: 1,
            },
            properties: FSF_LINK | FSF_SYMLINK | FSF_HOMOGENEOUS | FSF_CANSETTIME,
        })
    }
}

/// A workspace's NFS server, on a thread of its own, and the client mount of it.
#[derive(Debug)]
pub(crate) struct NfsMount {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    mounted: bool,
}

impl NfsMount {
    /// Serves `fs` as the export `/<workspace>` and mounts it at `path`.
    pub(crate) fn new(fs: WorkspaceFs, workspace: &str, path: &Path) -> io::Result<Self> {
        let (port_sender, port) = std_mpsc::channel();
        let (shutdown, stop) = oneshot::channel();
        let export = workspace.to_owned();
        let thread = thread::Builder::new()
            .name("peerfs-nfs".to_owned())
            .spawn(move || serve(NfsFs::new(fs), &export, &port_sender, stop))?;
        let mut mount = Self {
            shutdown: Some(shutdown),
            thread: Some(thread),
            mounted: false,
        };
        let port = port
            .recv()
            .map_err(|_| io::Error::other("the NFS server stopped before listening"))??;
        run(&mut mount_command(port, workspace, path))?;
        mount.mounted = true;
        Ok(mount)
    }

    /// Unmounts, then stops the server.
    pub(crate) fn stop(&mut self, path: &Path) -> io::Result<()> {
        if self.mounted {
            run(Command::new("umount").arg(path))?;
            self.mounted = false;
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Ok(())
    }
}

/// Listens on a free loopback port, reports it, and serves until told to stop.
fn serve(
    fs: NfsFs,
    export: &str,
    port: &std_mpsc::Sender<io::Result<u16>>,
    stop: oneshot::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = port.send(Err(e));
            return;
        }
    };
    runtime.block_on(async move {
        let mut listener = match NFSTcpListener::bind("127.0.0.1:0", fs).await {
            Ok(listener) => listener,
            Err(e) => {
                let _ = port.send(Err(e));
                return;
            }
        };
        listener.with_export_name(export);
        listener.with_reserved_ports_only();
        let _ = port.send(Ok(listener.get_listen_port()));
        tokio::select! {
            _ = listener.handle_forever() => {}
            _ = stop => {}
        }
    });
}

/// The command that mounts the export `/<workspace>`, served at `port`, on `path`.
fn mount_command(port: u16, workspace: &str, path: &Path) -> Command {
    let source = format!("127.0.0.1:/{workspace}");
    let timeouts = format!("actimeo={ATTRIBUTE_SECONDS}");
    if cfg!(target_os = "macos") {
        let mut command = Command::new("mount_nfs");
        command.args([
            "-o",
            &format!("vers=3,tcp,port={port},mountport={port},nolocks,soft,{timeouts}"),
            &source,
        ]);
        command.arg(path);
        command
    } else {
        let mut command = Command::new("mount");
        command.args([
            "-t",
            "nfs",
            "-o",
            &format!(
                "vers=3,proto=tcp,port={port},mountport={port},mountproto=tcp,nolock,soft,{timeouts}"
            ),
            &source,
        ]);
        command.arg(path);
        command
    }
}

/// Runs `command`, failing with its standard error if it does not succeed.
fn run(command: &mut Command) -> io::Result<()> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let kind = if message.contains("busy") {
        io::ErrorKind::ResourceBusy
    } else if output.status.code() == Some(32) && message.contains("ermission") {
        io::ErrorKind::PermissionDenied
    } else {
        io::ErrorKind::Other
    };
    Err(io::Error::new(
        kind,
        format!("{command:?} failed: {message}"),
    ))
}

fn c_name(name: &filename3) -> Result<CString, nfsstat3> {
    CString::new(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

/// What `setattr` asks for, as FUSE's `setattr` takes it.
fn to_setattr(attr: &sattr3) -> (stat64, SetattrValid) {
    // SAFETY: stat64 is a plain C struct for which all zeroes is a valid value.
    let mut st: stat64 = unsafe { std::mem::zeroed() };
    let mut valid = SetattrValid::empty();
    if let set_mode3::mode(mode) = attr.mode {
        st.st_mode = mode;
        valid |= SetattrValid::MODE;
    }
    if let set_size3::size(size) = attr.size {
        st.st_size = i64::try_from(size).unwrap_or(i64::MAX);
        valid |= SetattrValid::SIZE;
    }
    match attr.atime {
        set_atime::DONT_CHANGE => {}
        set_atime::SET_TO_SERVER_TIME => valid |= SetattrValid::ATIME | SetattrValid::ATIME_NOW,
        set_atime::SET_TO_CLIENT_TIME(time) => {
            st.st_atime = i64::from(time.seconds);
            st.st_atime_nsec = i64::from(time.nseconds);
            valid |= SetattrValid::ATIME;
        }
    }
    match attr.mtime {
        set_mtime::DONT_CHANGE => {}
        set_mtime::SET_TO_SERVER_TIME => valid |= SetattrValid::MTIME | SetattrValid::MTIME_NOW,
        set_mtime::SET_TO_CLIENT_TIME(time) => {
            st.st_mtime = i64::from(time.seconds);
            st.st_mtime_nsec = i64::from(time.nseconds);
            valid |= SetattrValid::MTIME;
        }
    }
    // Owners are the server's user, so a request to change them is ignored.
    (st, valid)
}

fn fattr(st: &stat64) -> fattr3 {
    let ftype = match st.st_mode & libc::S_IFMT {
        libc::S_IFDIR => ftype3::NF3DIR,
        libc::S_IFLNK => ftype3::NF3LNK,
        _ => ftype3::NF3REG,
    };
    let time = |seconds: i64, nanos: i64| nfstime3 {
        seconds: u32::try_from(seconds).unwrap_or(0),
        nseconds: u32::try_from(nanos).unwrap_or(0),
    };
    let size = u64::try_from(st.st_size).unwrap_or(0);
    fattr3 {
        ftype,
        mode: st.st_mode & 0o7777,
        nlink: u32::try_from(st.st_nlink).unwrap_or(u32::MAX),
        uid: st.st_uid,
        gid: st.st_gid,
        size,
        used: u64::try_from(st.st_blocks).unwrap_or(0) * 512,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: st.st_ino,
        atime: time(st.st_atime, st.st_atime_nsec),
        mtime: time(st.st_mtime, st.st_mtime_nsec),
        ctime: time(st.st_ctime, st.st_ctime_nsec),
    }
}

/// The NFS status for a file system error.
fn to_status(error: &io::Error) -> nfsstat3 {
    match error.raw_os_error() {
        Some(libc::ENOENT) => nfsstat3::NFS3ERR_NOENT,
        Some(libc::EEXIST) => nfsstat3::NFS3ERR_EXIST,
        Some(libc::ENOTEMPTY) => nfsstat3::NFS3ERR_NOTEMPTY,
        Some(libc::EXDEV) => nfsstat3::NFS3ERR_XDEV,
        Some(libc::EISDIR) => nfsstat3::NFS3ERR_ISDIR,
        Some(libc::ENOTDIR) => nfsstat3::NFS3ERR_NOTDIR,
        Some(libc::EINVAL) => nfsstat3::NFS3ERR_INVAL,
        Some(libc::EPERM) => nfsstat3::NFS3ERR_PERM,
        Some(libc::EACCES) => nfsstat3::NFS3ERR_ACCES,
        Some(libc::ENOSPC) => nfsstat3::NFS3ERR_NOSPC,
        Some(libc::ENAMETOOLONG) => nfsstat3::NFS3ERR_NAMETOOLONG,
        Some(libc::EROFS) => nfsstat3::NFS3ERR_ROFS,
        Some(libc::EFBIG) => nfsstat3::NFS3ERR_FBIG,
        _ => nfsstat3::NFS3ERR_IO,
    }
}
