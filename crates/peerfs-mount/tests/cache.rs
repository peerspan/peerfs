//! A workspace holding a build cache version, mounted over FUSE and over loopback NFS: the
//! cached files read through the mount with their own modes and dates, copy up on the
//! first write, stay removed, and a real `cargo build` finds its output fresh.

#![expect(
    clippy::unwrap_used,
    reason = "test bodies are plain functions, run per front end by generated tests"
)]

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use peerfs_core::{Author, CommitOutcome, Located, Space, TreePath, Workspace};
use peerfs_mount::{Frontend, Mount};

fn author() -> Author {
    Author {
        host: "laptop".to_owned(),
        ..Author::default()
    }
}

/// Whether this machine lets the test mount through `frontend`.
fn available(frontend: Frontend) -> bool {
    match frontend {
        Frontend::Fuse => fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
            .is_ok(),
        Frontend::Nfs => {
            // SAFETY: geteuid has no preconditions and cannot fail.
            let root = unsafe { libc::geteuid() } == 0;
            root && ["/sbin/mount.nfs", "/usr/sbin/mount.nfs"]
                .iter()
                .any(|helper| Path::new(helper).exists())
        }
    }
}

macro_rules! per_frontend {
    ($($test:ident),* $(,)?) => {
        mod fuse {
            $(#[test] fn $test() { super::$test(super::Frontend::Fuse) })*
        }
        mod nfs {
            $(#[test] fn $test() { super::$test(super::Frontend::Nfs) })*
        }
    };
}

per_frontend!(
    a_mount_shows_cached_files_read_only_with_their_dates,
    cargo_finds_a_cached_build_fresh_and_rebuilds_only_what_changed,
);

fn path(text: &str) -> TreePath {
    TreePath::try_from(text).unwrap()
}

fn long_ago() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn modified(local: &Path) -> SystemTime {
    fs::symlink_metadata(local).unwrap().modified().unwrap()
}

/// A space whose `main` holds `files`.
fn space(dir: &Path, files: &[(&str, &str)]) -> Space {
    let mut space = Space::init(dir.join("space"), &author()).unwrap();
    let mut setup = space.create_workspace("setup", "main").unwrap();
    for (file, text) in files {
        setup.write(&space, file, text.as_bytes()).unwrap();
    }
    let outcome = space
        .commit_workspace(&mut setup, "setup", &author())
        .unwrap();
    assert!(matches!(outcome, CommitOutcome::Committed { .. }));
    space.discard_workspace("setup").unwrap();
    space
}

/// Where the version `workspace` holds keeps `at`.
fn in_version(space: &Space, workspace: &Workspace, at: &str) -> PathBuf {
    let held = workspace.cache().unwrap();
    space
        .path()
        .join("caches/versions")
        .join(&held.version)
        .join("files")
        .join(at)
}

fn a_mount_shows_cached_files_read_only_with_their_dates(frontend: Frontend) {
    if !available(frontend) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut space = space(
        dir.path(),
        &[
            ("Cargo.toml", "[package]\n"),
            ("src/lib.rs", "pub fn one() {}\n"),
        ],
    );
    let mut build = space.create_workspace("build", "main").unwrap();
    let Some(Located::Base(entry)) = build.locate(&space, &path("src/lib.rs")).unwrap() else {
        panic!("src/lib.rs is in the base");
    };
    let seen = build.stamp(&path("src/lib.rs"), &entry).unwrap();
    for (at, mode) in [
        ("target/debug/run", 0o755),
        ("target/debug/deps/x", 0o644),
        ("target/debug/deps/y", 0o644),
    ] {
        build.write(&space, at, at.as_bytes()).unwrap();
        let Some(Located::Local(local)) = build.locate(&space, &path(at)).unwrap() else {
            panic!("{at} is in the overlay");
        };
        fs::set_permissions(&local, fs::Permissions::from_mode(mode)).unwrap();
        File::options()
            .write(true)
            .open(&local)
            .unwrap()
            .set_times(FileTimes::new().set_modified(long_ago()))
            .unwrap();
    }
    space.save_cache("bc", "build").unwrap();
    let ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    let version_run = in_version(&space, &ws, "target/debug/run");
    let version_x = in_version(&space, &ws, "target/debug/deps/x");

    let space = Arc::new(Mutex::new(space));
    let root = dir.path().join("mnt");
    fs::create_dir(&root).unwrap();
    let _mount = Mount::new(Arc::clone(&space), "w", &root, frontend).unwrap();

    let listing = |at: &Path| {
        let mut names: Vec<String> = fs::read_dir(at)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    assert_eq!(listing(&root), ["Cargo.toml", "src", "target"]);
    assert_eq!(listing(&root.join("target/debug/deps")), ["x", "y"]);
    let run = root.join("target/debug/run");
    assert_eq!(fs::read_to_string(&run).unwrap(), "target/debug/run");
    let meta = fs::metadata(&run).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    assert_eq!(meta.modified().unwrap(), long_ago());
    // The unchanged source is dated as the build saw it, before its output.
    assert_eq!(modified(&root.join("src/lib.rs")), seen);

    // Writing and changing the mode copy up; the version stays as saved.
    fs::write(&run, "rebuilt").unwrap();
    assert_eq!(fs::read_to_string(&run).unwrap(), "rebuilt");
    assert_eq!(
        fs::read_to_string(&version_run).unwrap(),
        "target/debug/run"
    );
    let x = root.join("target/debug/deps/x");
    fs::set_permissions(&x, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fs::metadata(&x).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_to_string(&x).unwrap(), "target/debug/deps/x");
    assert_eq!(
        fs::metadata(&version_x).unwrap().permissions().mode() & 0o777,
        0o644
    );

    // Removing and renaming leave whiteouts; a cached directory is copied, not renamed.
    fs::remove_file(root.join("target/debug/deps/y")).unwrap();
    fs::rename(&x, root.join("target/x")).unwrap();
    assert_eq!(
        listing(&root.join("target/debug/deps")),
        Vec::<String>::new()
    );
    assert_eq!(listing(&root.join("target")), ["debug", "x"]);
    assert_eq!(
        fs::rename(root.join("target/debug"), root.join("target/other"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EXDEV)
    );
    fs::write(root.join("target/debug/deps/z"), "new").unwrap();
    assert_eq!(listing(&root.join("target/debug/deps")), ["z"]);
    assert!(
        version_x.exists()
            && in_version(&space.lock().unwrap(), &ws, "target/debug/deps/y").exists()
    );
}

/// Runs `cargo build` in `dir` and returns its standard error and how long it took.
fn cargo_build(dir: &Path) -> (String, Duration) {
    let started = Instant::now();
    let stderr = cargo(dir, &["build", "--offline", "--verbose"]);
    (stderr, started.elapsed())
}

/// Runs cargo with `args` in `dir` and returns its standard error.
fn cargo(dir: &Path, args: &[&str]) -> String {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .args(args)
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTC_WRAPPER")
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "cargo {args:?}: {stderr}");
    stderr
}

/// The crates `cargo build` compiled, as its `Compiling` lines name them.
fn compiled(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Compiling "))
        .filter_map(|rest| rest.split(' ').next())
        .map(ToOwned::to_owned)
        .collect()
}

#[expect(
    clippy::print_stdout,
    reason = "the timings are what this test measures, shown with --nocapture"
)]
fn cargo_finds_a_cached_build_fresh_and_rebuilds_only_what_changed(frontend: Frontend) {
    if !available(frontend) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &author()).unwrap();
    // The repository, with the `Cargo.lock` it commits, so a build changes no source.
    let repo = dir.path().join("repo");
    for (file, text) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
        ),
        (".gitignore", "/target\n"),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("a/src/lib.rs", "pub fn answer() -> u32 {\n    42\n}\n"),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\na = { path = \"../a\" }\n",
        ),
        (
            "b/src/main.rs",
            "fn main() {\n    println!(\"{}\", a::answer());\n}\n",
        ),
    ] {
        let at = repo.join(file);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(at, text).unwrap();
    }
    cargo(&repo, &["generate-lockfile", "--offline"]);
    let tree = space.import(&repo).unwrap();
    space
        .commit_tree("main", tree, "the repository", &author())
        .unwrap();
    space.create_workspace("build", "main").unwrap();
    let space = Arc::new(Mutex::new(space));
    // Both builds run at the same directory, as the runtime mounts every task at one path.
    let root = dir.path().join("mnt");
    fs::create_dir(&root).unwrap();

    let mut mount = Mount::new(Arc::clone(&space), "build", &root, frontend).unwrap();
    let (cold, cold_took) = cargo_build(&root);
    assert_eq!(compiled(&cold), ["a", "b"], "{cold}");
    mount.unmount().unwrap();
    drop(mount);
    let saved = space.lock().unwrap().save_cache("bc", "build").unwrap();
    space
        .lock()
        .unwrap()
        .create_workspace_with_cache("task", "main", Some("bc"))
        .unwrap();

    let mut mount = Mount::new(Arc::clone(&space), "task", &root, frontend).unwrap();
    let (warm, warm_took) = cargo_build(&root);
    assert_eq!(compiled(&warm), Vec::<String>::new(), "{warm}");
    assert!(
        warm.contains("Fresh a") && warm.contains("Fresh b"),
        "{warm}"
    );

    fs::write(
        root.join("b/src/main.rs"),
        "fn main() {\n    println!(\"answer: {}\", a::answer());\n}\n",
    )
    .unwrap();
    let (changed, changed_took) = cargo_build(&root);
    assert_eq!(compiled(&changed), ["b"], "{changed}");
    assert!(changed.contains("Fresh a"), "{changed}");
    let run = Command::new(root.join("target/debug/b")).output().unwrap();
    assert_eq!(run.stdout, b"answer: 42\n");
    mount.unmount().unwrap();
    println!(
        "{frontend:?}: cold {cold_took:?}, with the cache {warm_took:?}, after a one-line \
         change {changed_took:?}; the cache holds {} bytes",
        saved.bytes
    );
}
