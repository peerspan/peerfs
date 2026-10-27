//! The `peerfs` binary: concurrent commands sharing one server they start themselves.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

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
