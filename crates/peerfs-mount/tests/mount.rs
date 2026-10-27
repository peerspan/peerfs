//! A workspace mounted over FUSE, driven with ordinary file operations.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use peerfs_core::{Author, CommitOutcome, NewEntry, Space, TreePath};
use peerfs_mount::Mount;

fn author() -> Author {
    Author {
        host: "laptop".to_owned(),
        ..Author::default()
    }
}

/// Whether this machine lets the test mount FUSE file systems.
fn fuse_available() -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok()
}

struct Fixture {
    _dir: tempfile::TempDir,
    space: Arc<Mutex<Space>>,
    mount: Option<Mount>,
    root: std::path::PathBuf,
}

/// A space whose `main` holds `src/lib.rs`, `run.sh` (executable) and `docs/a.md`, with
/// workspace `w` mounted.
fn fixture() -> Option<Fixture> {
    if !fuse_available() {
        // This machine cannot mount FUSE file systems; there is nothing to test.
        return None;
    }
    let dir = tempfile::tempdir().expect("test setup");
    let mut space = Space::init(dir.path().join("space"), &author()).expect("test setup");
    let mut ws = space.create_workspace("w", "main").expect("test setup");
    let run = TreePath::try_from("run.sh").expect("test setup");
    ws.create_entry(&space, &run, NewEntry::File { executable: true })
        .expect("test setup");
    ws.write(&space, "run.sh", b"#!/bin/sh\necho hi\n")
        .expect("test setup");
    ws.write(&space, "src/lib.rs", b"pub fn one() {}\n")
        .expect("test setup");
    ws.write(&space, "docs/a.md", b"# A\n").expect("test setup");
    space
        .commit_workspace(&mut ws, "base", &author())
        .expect("test setup");

    let space = Arc::new(Mutex::new(space));
    let root = dir.path().join("mnt");
    fs::create_dir(&root).expect("test setup");
    let mount = Mount::new(Arc::clone(&space), "w", &root).expect("test setup");
    Some(Fixture {
        _dir: dir,
        space,
        mount: Some(mount),
        root,
    })
}

fn commit(fixture: &Fixture) -> CommitOutcome {
    let mut space = fixture.space.lock().expect("test setup");
    let mut ws = space.workspace("w").expect("test setup");
    space
        .commit_workspace(&mut ws, "through the mount", &author())
        .expect("test setup")
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("test setup")
        .map(|entry| {
            entry
                .expect("test setup")
                .file_name()
                .into_string()
                .expect("test setup")
        })
        .collect();
    names.sort();
    names
}

#[test]
fn files_read_write_and_commit_through_the_mount() {
    let Some(fixture) = fixture() else { return };
    let root = &fixture.root;

    assert_eq!(listing(root), ["docs", "run.sh", "src"]);
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn one() {}\n"
    );
    let mode = fs::metadata(root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    let out = Command::new(root.join("run.sh")).output().unwrap();
    assert_eq!(out.stdout, b"hi\n");

    // Edit in place, append, create, remove, rename, link.
    let lib = root.join("src/lib.rs");
    let mut text = fs::read_to_string(&lib).unwrap();
    text.push_str("pub fn two() {}\n");
    fs::write(&lib, &text).unwrap();
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&lib).unwrap();
        file.write_all(b"// end\n").unwrap();
        file.sync_all().unwrap();
    }
    fs::create_dir(root.join("tests")).unwrap();
    fs::create_dir(root.join("out")).unwrap();
    fs::write(root.join("tests/t.rs"), "#[test] fn t() {}\n").unwrap();
    fs::remove_file(root.join("docs/a.md")).unwrap();
    fs::rename(root.join("run.sh"), root.join("tests/run.sh")).unwrap();
    symlink("../src/lib.rs", root.join("tests/lib.rs")).unwrap();

    assert_eq!(
        fs::read_to_string(&lib).unwrap(),
        "pub fn one() {}\npub fn two() {}\n// end\n"
    );
    assert_eq!(listing(root), ["docs", "out", "src", "tests"]);
    assert!(listing(&root.join("docs")).is_empty());
    assert_eq!(listing(&root.join("tests")), ["lib.rs", "run.sh", "t.rs"]);
    assert_eq!(
        fs::read_link(root.join("tests/lib.rs")).unwrap(),
        Path::new("../src/lib.rs")
    );
    assert_eq!(
        fs::read_to_string(root.join("tests/lib.rs")).unwrap(),
        fs::read_to_string(&lib).unwrap()
    );
    assert!(fs::remove_dir(root.join("src")).is_err(), "not empty");
    assert_eq!(
        fs::rename(root.join("src"), root.join("source"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EXDEV),
        "a directory with committed content is copied, not renamed"
    );

    let CommitOutcome::Committed { commit, .. } = commit(&fixture) else {
        panic!("expected a commit");
    };
    // After the commit the overlay is empty and everything reads from the store.
    assert_eq!(
        fs::read_to_string(&lib).unwrap(),
        "pub fn one() {}\npub fn two() {}\n// end\n"
    );
    let mode = fs::metadata(root.join("tests/run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(
        listing(&root.join("out")).is_empty(),
        "a new empty directory survives the commit"
    );
    let space = fixture.space.lock().unwrap();
    let tree = space.commit(commit).unwrap().tree;
    let docs = space
        .entry(tree, &TreePath::try_from("docs").unwrap())
        .unwrap();
    assert!(
        matches!(docs, Some(peerfs_core::Entry::Tree(_))),
        "an emptied directory stays, as on any file system"
    );
}

#[test]
fn git_works_in_a_mounted_workspace() {
    let Some(mut fixture) = fixture() else { return };
    if Command::new("git").arg("--version").output().is_err() {
        return; // git is not installed
    }
    let root = fixture.root.clone();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    fs::write(root.join("src/lib.rs"), "changed\n").unwrap();
    assert_eq!(git(&["status", "--porcelain"]), " M src/lib.rs\n");
    git(&["commit", "-qam", "change"]);
    assert_eq!(git(&["status", "--porcelain"]), "");
    assert_eq!(git(&["log", "--format=%s"]), "change\ninit\n");

    fixture.mount.take().unwrap().unmount().unwrap();
    assert!(listing(&root).is_empty(), "unmounted");
}
