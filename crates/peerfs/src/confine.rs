//! Confines a process that serves one lent workspace to a virtual machine, before it reads
//! anything the guest sends.
//!
//! The process moves into new user, mount, network, IPC and UTS namespaces and pivots into a
//! root that holds only what serving the workspace needs: the space's objects, read-only,
//! its `classes` file, read-only, and the workspace's own directory. It keeps one
//! capability, to override file permissions on its own user's files, and a seccomp filter
//! kills it for any system call beyond those it was seen to make.
//!
//! What a fault in serving the guest could then reach: the workspace, and the history the
//! space stores, which never holds secret-class files. Not the space's signing key, its
//! other workspaces, the network, or other processes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{CWD, Mode, OFlags, StatVfsMountFlags, fstatvfs, openat};
use rustix::mount::{
    MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_bind, mount_change,
    mount_remount, unmount,
};
use rustix::process::{getegid, geteuid, getpid, pivot_root};
use rustix::thread::{
    CapabilitySet, CapabilitySets, UnshareFlags, set_capabilities, set_no_new_privs, unshare_unsafe,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The space's objects, which every workspace reads its base from.
const OBJECTS: &str = "objects";

/// The owner's path-class rules, read when the workspace is opened.
const CLASSES: &str = "classes";

/// Moves this process into a root holding only `workspace` of the space in `space_dir`, laid
/// out as the space is, so that [`peerfs_core::Space::open_lent`] opens `/`.
///
/// # Errors
///
/// Fails if any step fails, or if the process has other threads; it must then exit rather
/// than serve unconfined.
pub(crate) fn enter(space_dir: &Path, workspace: &str) -> Result<()> {
    let threads = fs::read_dir("/proc/self/task")
        .context("counting threads")?
        .count();
    if threads != 1 {
        bail!("cannot confine a process with {threads} threads");
    }
    let (uid, gid) = (geteuid().as_raw(), getegid().as_raw());
    // SAFETY: none of these flags unshares anything other code in the process relies on
    // sharing, such as descriptors or the filesystem context, and the process has no other
    // threads.
    unsafe {
        unshare_unsafe(
            UnshareFlags::NEWUSER
                | UnshareFlags::NEWNS
                | UnshareFlags::NEWNET
                | UnshareFlags::NEWIPC
                | UnshareFlags::NEWUTS,
        )
    }
    .context("creating namespaces")?;
    // The same user inside as out, so files keep their owner. Capabilities in this
    // namespace act only on that user's files.
    fs::write("/proc/self/setgroups", "deny").context("denying setgroups")?;
    fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")).context("mapping the user")?;
    fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n")).context("mapping the group")?;

    mount_change(
        "/",
        MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
    )
    .context("making mounts private")?;
    // Opened in this mount namespace, which binds can only come from, and before anything
    // is mounted over the space; bound from these descriptors.
    let objects = open_path(&space_dir.join(OBJECTS))?;
    let workspace_dir = open_path(&space_dir.join("workspaces").join(workspace))?;
    let classes = match open_path(&space_dir.join(CLASSES)) {
        Ok(classes) => Some(classes),
        Err(_) if !space_dir.join(CLASSES).exists() => None,
        Err(e) => return Err(e),
    };
    // The new root goes over the space's directory, in this mount namespace only.
    let root = space_dir;
    mount(
        "tmpfs",
        root,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        Some(c"size=64k,mode=0755"),
    )
    .context("mounting the new root")?;
    bind(&objects, &root.join(OBJECTS), Access::ReadOnly)?;
    let workspaces = root.join("workspaces");
    fs::create_dir(&workspaces)?;
    bind(
        &workspace_dir,
        &workspaces.join(workspace),
        Access::ReadWrite,
    )?;
    if let Some(classes) = &classes {
        bind(classes, &root.join(CLASSES), Access::ReadOnly)?;
    }
    drop((objects, workspace_dir, classes));

    let old = root.join(".old");
    fs::DirBuilder::new().mode(0o700).create(&old)?;
    pivot_root(root, &old).context("pivot_root")?;
    std::env::set_current_dir("/")?;
    unmount("/.old", UnmountFlags::DETACH).context("detaching the old root")?;
    fs::remove_dir("/.old")?;
    mount_remount(
        "/",
        MountFlags::BIND
            | MountFlags::RDONLY
            | MountFlags::NOSUID
            | MountFlags::NODEV
            | MountFlags::NOEXEC,
        "",
    )
    .context("making the new root read-only")?;

    // Writing a read-only file the guest's root may write needs overriding permissions.
    let kept = CapabilitySet::DAC_OVERRIDE;
    set_capabilities(
        None,
        CapabilitySets {
            effective: kept,
            permitted: kept,
            inheritable: CapabilitySet::empty(),
        },
    )
    .context("dropping capabilities")?;
    set_no_new_privs(true).context("no_new_privs")?;
    for program in programs()? {
        seccompiler::apply_filter(&program).context("installing the seccomp filter")?;
    }
    Ok(())
}

fn open_path(path: &Path) -> Result<OwnedFd> {
    openat(
        CWD,
        path,
        OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("opening {}", path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    ReadOnly,
    ReadWrite,
}

/// Bind-mounts what `source` refers to at `target`, which is created, with nothing setuid,
/// no devices and no programs.
fn bind(source: &OwnedFd, target: &Path, access: Access) -> Result<()> {
    let is_dir = rustix::fs::fstat(source)
        .map(|stat| rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir())
        .context("inspecting a bind source")?;
    if is_dir {
        fs::create_dir(target)?;
    } else {
        File::create(target)?;
    }
    let what = || format!("binding {}", target.display());
    let path = format!("/proc/self/fd/{}", source.as_raw_fd());
    mount_bind(path.as_str(), target).with_context(what)?;
    // A bind mount takes flags only when remounted, and in a user namespace it must keep the
    // atime flags of the mount it came from.
    let from = fstatvfs(source.as_fd()).with_context(what)?.f_flag;
    let mut flags = MountFlags::BIND | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    if access == Access::ReadOnly {
        flags |= MountFlags::RDONLY;
    }
    for (statvfs, mount) in [
        (StatVfsMountFlags::NOATIME, MountFlags::NOATIME),
        (StatVfsMountFlags::NODIRATIME, MountFlags::NODIRATIME),
        (StatVfsMountFlags::RELATIME, MountFlags::RELATIME),
    ] {
        if from.contains(statvfs) {
            flags |= mount;
        }
    }
    mount_remount(target, flags, "").with_context(what)
}

/// Calls the process may make with any arguments: those of the C and Rust runtimes, its
/// threads and event loops, the vhost-user socket, and the file operations serving the
/// workspace takes.
const ALLOWED: &[libc::c_long] = &[
    // Memory.
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_mprotect,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_madvise,
    // Threads, signals and time.
    libc::SYS_futex,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_restart_syscall,
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_getrandom,
    libc::SYS_getuid,
    libc::SYS_getgid,
    libc::SYS_geteuid,
    libc::SYS_getegid,
    libc::SYS_exit,
    libc::SYS_exit_group,
    // Descriptors, events and the vhost-user socket.
    libc::SYS_eventfd2,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_ppoll,
    libc::SYS_read,
    libc::SYS_readv,
    libc::SYS_write,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_lseek,
    libc::SYS_close,
    libc::SYS_fcntl,
    libc::SYS_dup,
    libc::SYS_accept4,
    libc::SYS_recvmsg,
    libc::SYS_sendmsg,
    libc::SYS_recvfrom,
    libc::SYS_sendto,
    libc::SYS_shutdown,
    // Files, inside the new root.
    libc::SYS_openat,
    libc::SYS_newfstatat,
    libc::SYS_fstat,
    libc::SYS_statx,
    libc::SYS_fstatfs,
    libc::SYS_statfs,
    libc::SYS_getdents64,
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_renameat2,
    libc::SYS_linkat,
    libc::SYS_symlinkat,
    libc::SYS_readlinkat,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_utimensat,
    libc::SYS_ftruncate,
    libc::SYS_fallocate,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_umask,
];

/// Calls only some architectures have, which the C library uses there.
#[cfg(target_arch = "x86_64")]
const ALLOWED_HERE: &[libc::c_long] = &[
    libc::SYS_epoll_wait,
    libc::SYS_poll,
    libc::SYS_accept,
    libc::SYS_renameat,
    libc::SYS_open,
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_access,
    libc::SYS_mkdir,
    libc::SYS_rmdir,
    libc::SYS_unlink,
    libc::SYS_rename,
    libc::SYS_link,
    libc::SYS_symlink,
    libc::SYS_readlink,
    libc::SYS_chmod,
];
#[cfg(not(target_arch = "x86_64"))]
const ALLOWED_HERE: &[libc::c_long] = &[];

/// clone flags that create namespaces.
fn clone_namespaces() -> u64 {
    u64::from(
        (libc::CLONE_NEWNS
            | libc::CLONE_NEWCGROUP
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUSER
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWNET)
            .cast_unsigned(),
    )
}

/// The filters in the order they are applied: the refusal of clone3, then the allow-list.
fn programs() -> Result<[BpfProgram; 2]> {
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| anyhow::anyhow!("seccomp target {}: {e}", std::env::consts::ARCH))?;
    let arg = |index, op, value| SeccompCondition::new(index, SeccompCmpArgLen::Qword, op, value);
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = ALLOWED
        .iter()
        .chain(ALLOWED_HERE)
        .map(|&call| (call, Vec::new()))
        .collect();
    // Threads, but not in new namespaces.
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![arg(
            0,
            SeccompCmpOp::MaskedEq(clone_namespaces()),
            0,
        )?])?],
    );
    // Allowed here so that the second filter's ENOSYS, not this one's kill, decides it.
    rules.insert(libc::SYS_clone3, Vec::new());
    // Signals to this process's own threads only, as abort and thread naming need.
    let pid = u64::from(getpid().as_raw_nonzero().get().unsigned_abs());
    rules.insert(
        libc::SYS_tgkill,
        vec![SeccompRule::new(vec![arg(0, SeccompCmpOp::Eq, pid)?])?],
    );
    rules.insert(
        libc::SYS_prctl,
        vec![SeccompRule::new(vec![arg(
            0,
            SeccompCmpOp::Eq,
            u64::from(libc::PR_SET_NAME.cast_unsigned()),
        )?])?],
    );
    let allow_list = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        arch,
    )
    .context("building the seccomp allow-list")?;

    // clone3 passes its flags in memory, where seccomp cannot check them. With ENOSYS, the C
    // library falls back to clone, whose flags the allow-list checks.
    let enosys = u32::try_from(libc::ENOSYS).context("ENOSYS")?;
    let no_clone3 = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(enosys),
        arch,
    )
    .context("building the clone3 filter")?;

    Ok([
        no_clone3
            .try_into()
            .context("compiling the clone3 filter")?,
        allow_list
            .try_into()
            .context("compiling the seccomp allow-list")?,
    ])
}
