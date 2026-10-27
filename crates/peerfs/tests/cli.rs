//! The `peerfs` binary: concurrent commands sharing one server they start themselves.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_peerfs");

fn peerfs(space: &Path, agent: &str, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(BINARY);
    command
        .arg("--space")
        .arg(space)
        .args(["--host", "test-host", "--agent", agent])
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("the binary starts");
    if let Some(input) = stdin {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("the binary finishes")
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("UTF-8 output")
}

struct Space {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Drop for Space {
    fn drop(&mut self) {
        // Stop the server the commands started, so it does not outlive the test.
        if let Ok(pid) = std::fs::read_to_string(self.path.join("serve.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn new_space() -> Space {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("space");
    ok(&peerfs(&path, "setup", &["init"], None));
    Space { _dir: dir, path }
}

#[test]
fn eight_processes_start_one_server_and_all_commit() {
    let space = new_space();
    let agents: Vec<_> = (0..8)
        .map(|i| {
            let path = space.path.clone();
            thread::spawn(move || {
                let name = format!("agent-{i}");
                ok(&peerfs(&path, &name, &["ws", "create", &name], None));
                ok(&peerfs(
                    &path,
                    &name,
                    &["ws", "write", &name, &format!("agents/{i}.txt")],
                    Some(&format!("from {i}\n")),
                ));
                ok(&peerfs(
                    &path,
                    &name,
                    &["ws", "commit", &name, "-m", "work"],
                    None,
                ))
            })
        })
        .collect();
    for agent in agents {
        let out = agent.join().expect("agent thread");
        assert!(!out.contains("conflict"), "{out}");
    }

    ok(&peerfs(
        &space.path,
        "check",
        &["ws", "create", "check"],
        None,
    ));
    let listing = ok(&peerfs(
        &space.path,
        "check",
        &["ws", "ls", "check", "agents"],
        None,
    ));
    assert_eq!(listing.lines().count(), 8, "{listing}");
    let log = ok(&peerfs(&space.path, "check", &["log"], None));
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("agent-") && line.contains("@test-host"))
            .count(),
        8
    );
}

#[test]
fn a_conflict_exits_with_status_one() {
    let space = new_space();
    for name in ["a", "b"] {
        ok(&peerfs(&space.path, name, &["ws", "create", name], None));
        ok(&peerfs(
            &space.path,
            name,
            &["ws", "write", name, "f.txt"],
            Some(&format!("from {name}\n")),
        ));
    }
    ok(&peerfs(
        &space.path,
        "a",
        &["ws", "commit", "a", "-m", "a"],
        None,
    ));
    let conflicted = peerfs(&space.path, "b", &["ws", "commit", "b", "-m", "b"], None);
    assert_eq!(conflicted.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&conflicted.stdout).contains("conflict: f.txt"));
}

#[test]
fn stop_stops_an_autostarted_server_and_the_next_command_starts_another() {
    let space = new_space();
    ok(&peerfs(&space.path, "a", &["branches"], None));
    let socket = space.path.join("peerfs.sock");
    let pid_file = space.path.join("serve.pid");
    let pid = std::fs::read_to_string(&pid_file).expect("the server wrote its pid");
    let pid = pid.trim().to_owned();
    assert!(socket.exists());

    let stopped = ok(&peerfs(&space.path, "a", &["stop"], None));
    assert!(
        stopped.contains(&format!("stopped the server (process {pid})")),
        "{stopped}"
    );
    assert!(!socket.exists());
    assert!(!pid_file.exists());
    let started = Instant::now();
    while alive(&pid) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "process {pid} outlived its socket"
        );
        thread::sleep(Duration::from_millis(20));
    }

    ok(&peerfs(&space.path, "a", &["branches"], None));
    assert!(socket.exists());
    let again = std::fs::read_to_string(&pid_file).expect("a new server wrote its pid");
    assert_ne!(again.trim(), pid);
    ok(&peerfs(&space.path, "a", &["stop"], None));
    assert!(!socket.exists());
    assert!(!pid_file.exists());
}

#[test]
fn stop_without_a_server_says_so_and_starts_none() {
    let space = new_space();
    let out = ok(&peerfs(&space.path, "a", &["stop"], None));
    assert!(out.contains("no server is running"), "{out}");
    assert!(!space.path.join("peerfs.sock").exists());
    assert!(!space.path.join("serve.pid").exists());
}

#[test]
fn stop_removes_a_pid_file_naming_a_dead_process() {
    let space = new_space();
    let mut child = Command::new("true").spawn().expect("true starts");
    let pid = child.id();
    child.wait().expect("true exits");
    let pid_file = space.path.join("serve.pid");
    std::fs::write(&pid_file, format!("{pid}\n")).expect("write pid file");

    let out = ok(&peerfs(&space.path, "a", &["stop"], None));
    assert!(out.contains("stale"), "{out}");
    assert!(out.contains(&format!("process {pid} is gone")), "{out}");
    assert!(!pid_file.exists());
    assert!(!space.path.join("peerfs.sock").exists());
}

#[test]
fn stop_does_not_signal_an_unrelated_process() {
    let space = new_space();
    let mut sleeper = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("sleep starts");
    let pid_file = space.path.join("serve.pid");
    std::fs::write(&pid_file, format!("{}\n", sleeper.id())).expect("write pid file");

    let out = peerfs(&space.path, "a", &["stop"], None);
    let still_running = sleeper.try_wait().expect("poll sleep").is_none();
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let out = ok(&out);
    assert!(still_running, "sleep was signalled");
    assert!(out.contains("was not signalled"), "{out}");
    assert!(!pid_file.exists());
}

/// Whether `pid` is a live process this user can signal.
fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn a_pull_needs_the_other_spaces_key_which_key_prints_for_hosts_add() {
    let ours = new_space();
    let theirs = new_space();
    let theirs_path = theirs.path.to_str().expect("UTF-8 path");

    let refused = peerfs(&ours.path, "a", &["pull", theirs_path], None);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("not a known key of host test-host"),
        "{stderr}"
    );

    let key_line = ok(&peerfs(&theirs.path, "a", &["key"], None));
    let (host, key) = key_line
        .trim()
        .split_once(' ')
        .expect("`key` prints `<host> <key>`");
    assert_eq!(host, "test-host");
    ok(&peerfs(&ours.path, "a", &["hosts", "add", host, key], None));
    assert_eq!(ok(&peerfs(&ours.path, "a", &["hosts"], None)), key_line);

    ok(&peerfs(&ours.path, "a", &["pull", theirs_path], None));
    let log = ok(&peerfs(
        &ours.path,
        "a",
        &["log", "--branch", "test-host/main"],
        None,
    ));
    assert!(log.contains("Create the space"), "{log}");
    assert!(!log.contains("unsigned"), "{log}");
}

#[test]
fn ws_snapshot_prints_each_snapshot_and_commit_replaces_them() {
    let space = new_space();
    let run = |args: &[&str], stdin: Option<&str>| ok(&peerfs(&space.path, "a", args, stdin));
    run(&["ws", "create", "w"], None);
    assert_eq!(
        run(&["ws", "snapshot", "w"], None),
        "",
        "nothing to snapshot"
    );
    run(&["ws", "write", "w", "a.txt"], Some("one\n"));
    let first = run(&["ws", "snapshot", "w", "-m", "Write a.txt"], None);
    assert_eq!(first.trim().len(), 66, "{first}");
    assert_eq!(run(&["ws", "snapshot", "w"], None), "", "unchanged");
    run(&["ws", "write", "w", "a.txt"], Some("two\n"));
    let second = run(&["ws", "snapshot", "w"], None);

    let listed = run(&["ws", "snapshots", "w"], None);
    let ids: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.split(' ').next())
        .collect();
    assert_eq!(ids, vec![second.trim(), first.trim()]);
    assert!(listed.contains("Write a.txt"), "{listed}");

    run(&["ws", "commit", "w", "-m", "Task"], None);
    let log = run(&["log", "-n", "1"], None);
    assert!(
        log.contains(&format!("replaces snapshot {}", second.trim())),
        "{log}"
    );
    assert_eq!(run(&["ws", "snapshots", "w"], None), "");
}

#[test]
fn a_mounted_workspace_is_edited_as_a_directory_and_committed() {
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_err()
    {
        return; // This machine cannot mount FUSE file systems.
    }
    let space = new_space();
    let run = |args: &[&str]| ok(&peerfs(&space.path, "a", args, None));
    let mountpoint = space.path.parent().expect("parent").join("mnt");
    std::fs::create_dir(&mountpoint).expect("mountpoint");
    let mnt = mountpoint.to_str().expect("UTF-8");

    run(&["ws", "create", "w"]);
    assert!(run(&["ws", "mount", "w", mnt]).contains("w mounted at"));
    assert_eq!(run(&["mounts"]), format!("w {mnt}\n"));

    std::fs::create_dir(mountpoint.join("src")).expect("mkdir");
    std::fs::write(mountpoint.join("src/main.rs"), "fn main() {}\n").expect("write");
    assert_eq!(run(&["ws", "status", "w"]).trim(), "A src/main.rs");
    run(&["ws", "commit", "w", "-m", "From the mount"]);
    assert_eq!(
        std::fs::read_to_string(mountpoint.join("src/main.rs")).expect("read"),
        "fn main() {}\n",
        "still there, now from the store"
    );
    assert!(run(&["log", "-n", "1"]).contains("From the mount"));

    let discard = peerfs(&space.path, "a", &["ws", "discard", "w"], None);
    assert!(
        !discard.status.success(),
        "a mounted workspace is not discarded"
    );
    let import = peerfs(&space.path, "a", &["import", mnt], None);
    assert!(!import.status.success(), "the server would wait on itself");

    run(&["ws", "unmount", "w"]);
    assert_eq!(run(&["mounts"]), "");
    assert!(
        std::fs::read_dir(&mountpoint)
            .expect("listing")
            .next()
            .is_none()
    );
}

#[test]
fn the_claude_code_hook_snapshots_a_mounted_workspace_after_a_tool_call() {
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_err()
    {
        return; // This machine cannot mount FUSE file systems.
    }
    let space = new_space();
    let run = |args: &[&str]| ok(&peerfs(&space.path, "a", args, None));
    let outside = space.path.parent().expect("parent");
    let mountpoint = outside.join("mnt");
    std::fs::create_dir(&mountpoint).expect("mountpoint");
    let mnt = mountpoint.to_str().expect("UTF-8");
    run(&["ws", "create", "w"]);
    run(&["ws", "mount", "w", mnt]);
    let hook = |space: &Path, cwd: &Path, input: &str| {
        let call = format!(
            r#"{{"session_id": "s1", "hook_event_name": "PostToolUse", "cwd": "{}", {input}}}"#,
            cwd.display()
        );
        peerfs(space, "a", &["hook", "claude-code"], Some(&call))
    };
    let write = format!(
        r#""tool_name": "Write", "tool_use_id": "toolu_1",
           "tool_input": {{"file_path": "{mnt}/notes.txt", "content": "one"}}"#
    );

    std::fs::write(mountpoint.join("notes.txt"), "one\n").expect("write");
    let snapped = hook(&space.path, &mountpoint, &write);
    assert_eq!(ok(&snapped), "", "nothing on standard output");
    let snapshots = run(&["ws", "snapshots", "w"]);
    assert_eq!(snapshots.lines().count(), 1, "{snapshots}");
    assert!(
        snapshots.contains("Write: notes.txt (toolu_1)"),
        "{snapshots}"
    );

    // Nothing changed since: no new snapshot.
    ok(&hook(
        &space.path,
        &mountpoint,
        r#""tool_name": "Read", "tool_input": {"file_path": "notes.txt"}"#,
    ));
    assert_eq!(run(&["ws", "snapshots", "w"]).lines().count(), 1);

    // A shell command changes a file.
    std::fs::write(mountpoint.join("notes.txt"), "two\n").expect("write");
    ok(&hook(
        &space.path,
        &mountpoint.join("."),
        r#""tool_name": "Bash", "tool_input": {"command": "echo two > notes.txt"}"#,
    ));
    let snapshots = run(&["ws", "snapshots", "w"]);
    assert_eq!(snapshots.lines().count(), 2, "{snapshots}");
    assert!(
        snapshots
            .lines()
            .next()
            .expect("newest")
            .contains("Bash: echo two")
    );

    // Outside any peerfs mount the hook does nothing, even for a space that has no server.
    let elsewhere = outside.join("no-such-space");
    ok(&hook(
        &elsewhere,
        outside,
        r#""tool_name": "Bash", "tool_input": {}"#,
    ));
    assert!(!elsewhere.exists(), "no server or space was created");

    // In a mount another space serves, it says how to fix it, without starting a server.
    std::fs::write(mountpoint.join("notes.txt"), "three\n").expect("write");
    let wrong = hook(&elsewhere, &mountpoint, &write);
    assert_eq!(wrong.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("PEERFS_SPACE"),
        "{}",
        String::from_utf8_lossy(&wrong.stderr)
    );
    assert!(!elsewhere.exists());

    run(&["ws", "unmount", "w"]);
}

#[test]
fn restore_goes_back_to_a_snapshot_by_the_start_of_its_id() {
    let space = new_space();
    let run = |args: &[&str], stdin: Option<&str>| ok(&peerfs(&space.path, "a", args, stdin));
    run(&["ws", "create", "w"], None);
    run(&["ws", "write", "w", "notes.txt"], Some("one\n"));
    let first = run(&["ws", "snapshot", "w", "-m", "one"], None);
    run(&["ws", "write", "w", "notes.txt"], Some("two\n"));

    let prefix = &first.trim()[..20];
    let out = run(&["ws", "restore", "w", prefix], None);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].starts_with("saved w first as snapshot "), "{out}");
    assert!(
        lines[1].starts_with(&format!("restored w to {}; now snapshot ", first.trim())),
        "{out}"
    );
    assert_eq!(run(&["ws", "read", "w", "notes.txt"], None), "one\n");
    assert_eq!(run(&["ws", "snapshots", "w"], None).lines().count(), 3);

    let again = run(&["ws", "restore", "w", prefix], None);
    assert_eq!(again, format!("w already matches {}\n", first.trim()));

    let missing = peerfs(&space.path, "a", &["ws", "restore", "w", "01ffff"], None);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no snapshot 01ffff"));
}
