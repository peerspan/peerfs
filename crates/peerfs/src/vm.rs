//! `ws vhost-user`: serves a workspace to one virtual machine from a confined process, as
//! `virtiofsd` serves a directory.
//!
//! The command borrows the workspace from the space's server, binds the socket, and
//! executes itself again as `serve-vm`, a fresh process with no threads, handing on the
//! lease and the socket. That process waits for the VMM to connect, removes the socket's
//! file, confines itself (see [`crate::confine`]), and serves the guest until the VMM
//! disconnects. Then it gives the workspace back and exits.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use peerfs_api::{Client, Lease};
use peerfs_core::Space;
use peerfs_mount::{WorkspaceFs, serve_vm};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::{FdFlags, fcntl_setfd};

use crate::confine;

/// The longest virtio-fs tag.
const TAG_LEN: usize = 36;

/// `ws vhost-user`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct VhostUserArgs {
    name: String,
    /// The socket to create, which the VMM connects to as it would to `virtiofsd`.
    socket: PathBuf,
    /// The tag the guest mounts it by; the workspace's name if not given.
    #[arg(long)]
    tag: Option<String>,
}

/// `serve-vm`'s arguments, given by `ws vhost-user`.
#[derive(Debug, clap::Args)]
pub(crate) struct ServeVmArgs {
    #[arg(long)]
    workspace: String,
    #[arg(long)]
    tag: String,
    /// The socket's path, removed once the VMM has connected.
    #[arg(long)]
    socket: PathBuf,
    /// The descriptor of the lease's connection to the space's server.
    #[arg(long)]
    lease_fd: RawFd,
    /// The descriptor of the bound socket.
    #[arg(long)]
    listener_fd: RawFd,
}

/// Borrows the workspace, binds the socket, and becomes the process that serves it. Returns
/// only on failure.
pub(crate) async fn lend_and_serve(
    client: &Client,
    space_dir: &Path,
    args: VhostUserArgs,
) -> Result<()> {
    let tag = args.tag.unwrap_or_else(|| args.name.clone());
    if tag.is_empty() || tag.len() > TAG_LEN {
        bail!("a virtio-fs tag is 1 to {TAG_LEN} bytes");
    }
    let socket = std::path::absolute(&args.socket)
        .with_context(|| format!("resolving {}", args.socket.display()))?;
    let lease = client.lend(&args.name).await?;
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let error = (|| -> Result<()> {
        // Inherited by the program executed, and by nothing else.
        fcntl_setfd(lease.as_fd(), FdFlags::empty())?;
        fcntl_setfd(listener.as_fd(), FdFlags::empty())?;
        let error = Process::new(std::env::current_exe().context("finding this program")?)
            .arg("--space")
            .arg(space_dir)
            .arg("serve-vm")
            .arg("--workspace")
            .arg(&args.name)
            .arg("--tag")
            .arg(&tag)
            .arg("--socket")
            .arg(&socket)
            .arg("--lease-fd")
            .arg(lease.as_fd().as_raw_fd().to_string())
            .arg("--listener-fd")
            .arg(listener.as_raw_fd().to_string())
            .exec();
        Err(error).context("executing this program to serve the workspace")
    })();
    let _ = std::fs::remove_file(&socket);
    error
}

/// `serve-vm`: confines this process and serves the workspace until the VMM disconnects.
pub(crate) fn serve(space_dir: &Path, args: &ServeVmArgs) -> Result<()> {
    // SAFETY: `ws vhost-user` executed this program with this descriptor open, and nothing
    // else in this process uses it.
    let lease = unsafe { OwnedFd::from_raw_fd(args.lease_fd) };
    // SAFETY: as for the lease.
    let listener = unsafe { OwnedFd::from_raw_fd(args.listener_fd) };
    fcntl_setfd(&lease, FdFlags::CLOEXEC)?;
    fcntl_setfd(&listener, FdFlags::CLOEXEC)?;
    let lease = Lease::from_fd(lease);
    let listener = UnixListener::from(listener);

    // Nothing the VMM sends is read until the process is confined.
    loop {
        match poll(&mut [PollFd::new(&listener, PollFlags::IN)], None) {
            Ok(_) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e).context("waiting for the VMM"),
        }
    }
    // Its connection is queued; no other VMM can connect now, and nothing is left behind.
    let _ = std::fs::remove_file(&args.socket);
    confine::enter(space_dir, &args.workspace).context("confining this process")?;

    let space = Space::open_lent("/").context("opening the lent workspace")?;
    let fs = WorkspaceFs::new(Arc::new(Mutex::new(space)), &args.workspace);
    serve_vm(fs, listener, &args.tag).context("serving the virtual machine")?;
    lease.give_back().context("giving the workspace back")
}
