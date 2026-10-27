//! `peerfs stop`: stop the space's server by signalling the process in `serve.pid`.
//!
//! The pid file is only trusted after checking that the process it names is a peerfs
//! server for this space, so a stale file never leads to signalling an unrelated process.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use peerfs_api::Client;
use peerfs_server::{pid_path, socket_path};

/// How long `stop` waits for the server to exit after the signal.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// What the process named by `serve.pid` turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Owner {
    /// A peerfs server for this space.
    Server,
    /// No such process, or one this user cannot signal.
    Gone,
    /// A live process that is not this space's server.
    Unrelated,
}

/// Stops the server of the space in `space_dir`, if one is running. Never starts one.
pub(crate) async fn stop(space_dir: &Path, out: &mut impl Write) -> Result<ExitCode> {
    let pid_file = pid_path(space_dir);
    let socket = socket_path(space_dir);
    let pid_text = match fs::read_to_string(&pid_file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if answers(space_dir).await {
                bail!(
                    "a server answers on {} but {} is missing; stop it by hand",
                    socket.display(),
                    pid_file.display()
                );
            }
            writeln!(out, "no server is running for {}", space_dir.display())?;
            return Ok(ExitCode::SUCCESS);
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", pid_file.display())),
    };
    // Process IDs 0 and 1 are never a server's, and `kill` treats 0 as the process group.
    let Some(pid) = pid_text.trim().parse::<u32>().ok().filter(|&pid| pid > 1) else {
        remove_stale(&pid_file)?;
        writeln!(
            out,
            "no server is running for {}; removed {}: it held no process ID",
            space_dir.display(),
            pid_file.display()
        )?;
        return Ok(ExitCode::SUCCESS);
    };
    match owner(pid, space_dir).await {
        Owner::Server => {}
        Owner::Gone => {
            remove_stale(&pid_file)?;
            writeln!(
                out,
                "no server is running for {}; removed the stale {} (process {pid} is gone)",
                space_dir.display(),
                pid_file.display()
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        Owner::Unrelated => {
            remove_stale(&pid_file)?;
            writeln!(
                out,
                "no server is running for {}; removed the stale {} (process {pid} is not \
                 its server and was not signalled)",
                space_dir.display(),
                pid_file.display()
            )?;
            return Ok(ExitCode::SUCCESS);
        }
    }

    if !signal(pid, "-TERM") {
        // It exited between the check and the signal.
        if !alive(pid) {
            remove_stale(&pid_file)?;
            writeln!(out, "the server (process {pid}) had already exited")?;
            return Ok(ExitCode::SUCCESS);
        }
        bail!("could not signal the server (process {pid})");
    }
    // Wait for the process itself: it removes its files before exiting, so only once it
    // has exited do the files say whether it cleaned up.
    let started = Instant::now();
    while alive(pid) {
        if started.elapsed() > STOP_WAIT {
            bail!(
                "the server (process {pid}) did not stop within {}s of SIGTERM; see {}",
                STOP_WAIT.as_secs(),
                space_dir.join("serve.log").display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if socket.exists() || pid_file.exists() {
        // It died without cleaning up; a later server would replace both files anyway.
        remove_stale(&socket)?;
        remove_stale(&pid_file)?;
        writeln!(
            out,
            "the server (process {pid}) exited without removing its files; removed them"
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(
        out,
        "stopped the server (process {pid}) for {}",
        space_dir.display()
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Whether a server for this space answers on its socket.
async fn answers(space_dir: &Path) -> bool {
    match Client::new(socket_path(space_dir)).space().await {
        Ok(info) => same_path(Path::new(&info.path), space_dir),
        Err(_) => false,
    }
}

/// What process `pid` is. On Linux, its command line says whether it is `peerfs serve`
/// for this space; elsewhere, the socket answering stands in for that check.
async fn owner(pid: u32, space_dir: &Path) -> Owner {
    if !alive(pid) {
        return Owner::Gone;
    }
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let is_server = match fs::read(proc_dir.join("cmdline")) {
        Ok(cmdline) => serves(&proc_dir, &cmdline, space_dir),
        Err(_) => answers(space_dir).await,
    };
    if is_server {
        Owner::Server
    } else {
        Owner::Unrelated
    }
}

/// Whether the command line `cmdline` of the process at `proc_dir` is `peerfs serve` for
/// the space in `space_dir`.
fn serves(proc_dir: &Path, cmdline: &[u8], space_dir: &Path) -> bool {
    let args: Vec<&OsStr> = cmdline
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(OsStr::from_bytes)
        .collect();
    let Some((program, rest)) = args.split_first() else {
        return false;
    };
    if Path::new(program).file_name() != Some(OsStr::new("peerfs"))
        || !rest.contains(&OsStr::new("serve"))
    {
        return false;
    }
    let space = space_arg(rest)
        .or_else(|| space_env(proc_dir))
        .unwrap_or_else(|| PathBuf::from(".peerfs"));
    let space = if space.is_absolute() {
        space
    } else {
        match fs::read_link(proc_dir.join("cwd")) {
            Ok(cwd) => cwd.join(space),
            Err(_) => return false,
        }
    };
    same_path(&space, space_dir)
}

/// The value of `--space` among `args`.
fn space_arg(args: &[&OsStr]) -> Option<PathBuf> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if *arg == "--space" {
            return args.next().map(PathBuf::from);
        }
        if let Some(value) = arg.as_bytes().strip_prefix(b"--space=") {
            return Some(PathBuf::from(OsStr::from_bytes(value)));
        }
    }
    None
}

/// `PEERFS_SPACE` in the environment the process at `proc_dir` started with.
fn space_env(proc_dir: &Path) -> Option<PathBuf> {
    let environ = fs::read(proc_dir.join("environ")).ok()?;
    environ.split(|&byte| byte == 0).find_map(|entry| {
        entry
            .strip_prefix(b"PEERFS_SPACE=")
            .map(|value| PathBuf::from(OsStr::from_bytes(value)))
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Runs `kill <signal> <pid>`, `signal` being an option such as `-TERM`; whether the
/// signal was delivered.
fn signal(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether `pid` is a running process this user can signal. A zombie, exited but not yet
/// reaped by its parent, does not count.
fn alive(pid: u32) -> bool {
    signal(pid, "-0") && !zombie(pid)
}

/// Whether `pid` has exited and waits for its parent to reap it. Only Linux says, through
/// `/proc`; elsewhere this is always false.
fn zombie(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // The state follows the command name, which is in parentheses and may hold any
        // character, so look after the last `)`.
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            == Some("Z")
    })
}

fn remove_stale(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn an_exited_unreaped_child_is_not_alive() {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        let started = Instant::now();
        while !zombie(pid) {
            assert!(started.elapsed() < Duration::from_secs(5), "never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(signal(pid, "-0"), "a zombie still takes signals");
        assert!(!alive(pid));
        child.wait().expect("reap");
        assert!(!alive(pid));
    }
}
