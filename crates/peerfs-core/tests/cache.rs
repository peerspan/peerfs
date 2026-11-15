//! Build caches: saving a workspace's derived files, holding versions, the third read layer
//! and the dating rule.

#![expect(
    clippy::unwrap_used,
    reason = "the helpers are part of the tests, where a panic is the assertion"
)]

use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use peerfs_core::{
    Author, CommitOutcome, EntryKind, Error, GcOptions, Located, Space, TreePath, Workspace,
};

fn author() -> Author {
    Author {
        host: "test-host".to_owned(),
        ..Author::default()
    }
}

fn path(text: &str) -> TreePath {
    TreePath::try_from(text).unwrap()
}

/// A date well before the test runs, as a build long ago would have left.
fn long_ago(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_000_000_000 + seconds)
}

/// A space whose `main` holds a small crate and a `.gitignore` naming `gen/` derived.
fn space() -> (tempfile::TempDir, Space) {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("s"), &author()).unwrap();
    let mut setup = space.create_workspace("setup", "main").unwrap();
    for (file, text) in [
        ("Cargo.toml", "[package]\nname = \"a\"\n"),
        ("src/lib.rs", "pub fn one() {}\n"),
        (".gitignore", "gen/\n"),
    ] {
        setup.write(&space, file, text.as_bytes()).unwrap();
    }
    commit(&mut space, &mut setup);
    space.discard_workspace("setup").unwrap();
    (dir, space)
}

fn commit(space: &mut Space, workspace: &mut Workspace) {
    let outcome = space
        .commit_workspace(workspace, "change", &author())
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::Committed { .. }),
        "{outcome:?}"
    );
}

/// Writes `text` at `at` in `workspace`, dated `when`, as a build writes its output.
fn build_output(space: &Space, workspace: &mut Workspace, at: &str, text: &str, when: SystemTime) {
    workspace.write(space, at, text.as_bytes()).unwrap();
    let Some(Located::Local(local)) = workspace.locate(space, &path(at)).unwrap() else {
        panic!("{at} is not in the overlay");
    };
    File::options()
        .write(true)
        .open(local)
        .unwrap()
        .set_times(FileTimes::new().set_modified(when))
        .unwrap();
}

/// Builds in a new workspace `name`, optionally holding `cache`, and saves the cache `bc`.
fn build_and_save(space: &mut Space, name: &str, cache: Option<&str>) {
    let mut workspace = space
        .create_workspace_with_cache(name, "main", cache)
        .unwrap();
    // The build looks at its sources, then writes its output.
    for source in ["Cargo.toml", "src/lib.rs"] {
        stamp(space, &mut workspace, source);
    }
    build_output(
        space,
        &mut workspace,
        &format!("target/debug/{name}.rlib"),
        name,
        long_ago(100),
    );
    space.save_cache("bc", name).unwrap();
}

/// The date `workspace` shows the base file at `at`.
fn stamp(space: &Space, workspace: &mut Workspace, at: &str) -> SystemTime {
    let Some(Located::Base(entry)) = workspace.locate(space, &path(at)).unwrap() else {
        panic!("{at} is not in the base");
    };
    workspace.stamp(&path(at), &entry).unwrap()
}

fn modified(local: &Path) -> SystemTime {
    fs::symlink_metadata(local).unwrap().modified().unwrap()
}

/// Where the cache version `workspace` holds keeps the file at `at`.
fn in_version(space: &Space, workspace: &Workspace, at: &str) -> PathBuf {
    let held = workspace.cache().expect("a held version");
    space
        .path()
        .join("caches/versions")
        .join(&held.version)
        .join("files")
        .join(at)
}

fn names(listing: Vec<(String, EntryKind)>) -> Vec<String> {
    listing.into_iter().map(|(name, _)| name).collect()
}

#[test]
fn saving_moves_the_derived_files_into_a_version_and_discards_the_workspace() {
    let (_dir, mut space) = space();
    let main = space.branch("main").unwrap().unwrap();
    let mut build = space.create_workspace("build", "main").unwrap();
    build_output(&space, &mut build, "target/debug/a", "binary", long_ago(1));
    build_output(
        &space,
        &mut build,
        "target/debug/key.pem",
        "inside",
        long_ago(1),
    );
    build_output(
        &space,
        &mut build,
        "gen/out.rs",
        "// generated",
        long_ago(1),
    );
    build_output(
        &space,
        &mut build,
        ".peerspan/cache/cargo/x",
        "crate",
        long_ago(1),
    );
    build.write(&space, ".env", b"TOKEN=1\n").unwrap();
    let saved = space.save_cache("bc-1", "build").unwrap();

    assert_eq!(saved.name, "bc-1");
    assert_eq!(saved.commit, main);
    assert_eq!(saved.versions, 1);
    assert_eq!(saved.bytes, 6 + 6 + 12 + 5);
    assert!(matches!(
        space.workspace("build"),
        Err(Error::NoSuchWorkspace(_))
    ));
    assert_eq!(space.caches().unwrap(), vec![saved]);

    let ws = space
        .create_workspace_with_cache("w", "main", Some("bc-1"))
        .unwrap();
    let held = ws.cache().unwrap();
    assert_eq!((held.name.as_str(), held.commit), ("bc-1", main));
    assert_eq!(ws.read(&space, "target/debug/a").unwrap(), b"binary");
    // A secret inside a derived directory goes with it; a secret path does not.
    assert_eq!(ws.read(&space, "target/debug/key.pem").unwrap(), b"inside");
    assert_eq!(ws.read(&space, "gen/out.rs").unwrap(), b"// generated");
    assert_eq!(
        ws.read(&space, ".peerspan/cache/cargo/x").unwrap(),
        b"crate"
    );
    assert!(matches!(ws.read(&space, ".env"), Err(Error::NotFound(_))));
    assert_eq!(
        names(ws.list(&space, "").unwrap()),
        [
            ".gitignore",
            ".peerspan",
            "Cargo.toml",
            "gen",
            "src",
            "target"
        ]
    );
    assert_eq!(
        ws.list(&space, "target/debug").unwrap(),
        vec![
            ("a".to_owned(), EntryKind::File),
            ("key.pem".to_owned(), EntryKind::File)
        ]
    );
    // Cached files are no change to commit.
    assert_eq!(ws.changes(&space).unwrap(), Vec::new());
}

#[test]
fn a_cache_that_does_not_exist_gives_the_workspace_none() {
    let (_dir, mut space) = space();
    let ws = space
        .create_workspace_with_cache("w", "main", Some("absent"))
        .unwrap();
    assert_eq!(ws.cache(), None);
    assert!(matches!(
        space.create_workspace_with_cache("x", "main", Some("Bad/Name")),
        Err(Error::InvalidCacheName(_))
    ));
    assert!(matches!(
        space.remove_cache("absent"),
        Err(Error::NoSuchCache(_))
    ));
}

#[test]
fn saving_is_refused_with_uncommitted_source_changes() {
    let (_dir, mut space) = space();
    build_and_save(&mut space, "first", None);
    let mut ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    build_output(&space, &mut ws, "target/debug/b", "b", long_ago(2));
    ws.write(&space, "src/lib.rs", b"pub fn two() {}\n")
        .unwrap();
    assert!(matches!(
        space.save_cache("bc", "w"),
        Err(Error::WorkspaceNotClean(_))
    ));
    ws.remove(&space, "Cargo.toml").unwrap();
    ws.write(&space, "src/lib.rs", b"pub fn one() {}\n")
        .unwrap();
    // A removed source file is a change too.
    assert!(matches!(
        space.save_cache("bc", "w"),
        Err(Error::WorkspaceNotClean(_))
    ));
    // Nothing moved: the workspace and the cache are as they were.
    let ws = space.workspace("w").unwrap();
    assert_eq!(ws.read(&space, "target/debug/b").unwrap(), b"b");
    let cache = space.cache("bc").unwrap().unwrap();
    assert_eq!(cache.versions, 1);
    assert_eq!(Some(&cache.version), ws.cache().map(|held| &held.version));
}

#[test]
fn cache_files_never_show_at_source_or_secret_paths_nor_over_the_base() {
    let (dir, mut space) = space();
    let mut build = space.create_workspace("build", "main").unwrap();
    build_output(
        &space,
        &mut build,
        "gen/out.rs",
        "derived by .gitignore",
        long_ago(1),
    );
    build_output(&space, &mut build, "target/x", "x", long_ago(1));
    space.save_cache("bc", "build").unwrap();

    // `gen/` stops being derived: the cache no longer shows there.
    let mut edit = space.create_workspace("edit", "main").unwrap();
    edit.write(&space, ".gitignore", b"# nothing\n").unwrap();
    commit(&mut space, &mut edit);
    let ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    assert!(matches!(
        ws.read(&space, "gen/out.rs"),
        Err(Error::NotFound(_))
    ));
    // Its directory still shows, as a directory leading to derived files would, but empty.
    assert_eq!(ws.list(&space, "gen").unwrap(), Vec::new());
    assert_eq!(ws.read(&space, "target/x").unwrap(), b"x");

    // The owner makes `target/` secret, then source: the cache shows there neither time.
    fs::write(space.path().join("classes"), "[secret]\ntarget/\n").unwrap();
    let ws = space.workspace("w").unwrap();
    assert!(matches!(
        ws.read(&space, "target/x"),
        Err(Error::NotFound(_))
    ));
    fs::write(space.path().join("classes"), "[source]\ntarget/\n").unwrap();
    let ws = space.workspace("w").unwrap();
    assert!(ws.locate(&space, &path("target/x")).unwrap().is_none());
    fs::remove_file(space.path().join("classes")).unwrap();

    // A base holding a derived path shows the base's file, not the cache's.
    let import = dir.path().join("import");
    fs::create_dir_all(import.join("target")).unwrap();
    fs::write(import.join("target/x"), "committed").unwrap();
    let tree = space.import(&import).unwrap();
    space
        .commit_tree("main", tree, "import a target", &author())
        .unwrap();
    let ws = space
        .create_workspace_with_cache("v", "main", Some("bc"))
        .unwrap();
    assert_eq!(ws.read(&space, "target/x").unwrap(), b"committed");
    assert!(matches!(
        ws.locate(&space, &path("target/x")).unwrap(),
        Some(Located::Base(_))
    ));
}

#[test]
fn a_gitignore_written_through_another_value_reclassifies_cached_paths() {
    let (_dir, mut space) = space();
    let mut build = space.create_workspace("build", "main").unwrap();
    build_output(&space, &mut build, "gen/out.rs", "generated", long_ago(1));
    space.save_cache("bc", "build").unwrap();

    // A mount's value reads a cached file, so it has worked out that `gen/` is derived.
    let mut mounted = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    assert_eq!(mounted.read(&space, "gen/out.rs").unwrap(), b"generated");

    // The local API, with its own value, stops `gen/` being derived.
    let mut api = space.workspace("w").unwrap();
    api.write(&space, ".gitignore", b"# nothing\n").unwrap();

    // The mount sees the overlay changed and no longer shows the cached file.
    assert!(mounted.overlay_changed_elsewhere());
    assert!(matches!(
        mounted.read(&space, "gen/out.rs"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn removing_a_committed_directory_keeps_its_cached_files_gone_after_commit() {
    let (_dir, mut space) = space();
    let mut setup = space.create_workspace("config", "main").unwrap();
    setup.write(&space, ".peerspan/config", b"x").unwrap();
    commit(&mut space, &mut setup);
    space.discard_workspace("config").unwrap();
    let mut build = space.create_workspace("build", "main").unwrap();
    build_output(
        &space,
        &mut build,
        ".peerspan/cache/output",
        "o",
        long_ago(1),
    );
    space.save_cache("bc", "build").unwrap();

    let mut ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    assert_eq!(ws.read(&space, ".peerspan/cache/output").unwrap(), b"o");
    ws.remove(&space, ".peerspan").unwrap();
    commit(&mut space, &mut ws);
    assert!(matches!(
        ws.read(&space, ".peerspan/cache/output"),
        Err(Error::NotFound(_))
    ));
    let reopened = space.workspace("w").unwrap();
    assert!(
        reopened
            .locate(&space, &path(".peerspan"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_edit_of_the_owner_rules_reclassifies_cached_paths_of_a_value_in_use() {
    let (_dir, mut space) = space();
    let mut build = space.create_workspace("build", "main").unwrap();
    build_output(&space, &mut build, "target/x", "x", long_ago(1));
    space.save_cache("bc", "build").unwrap();

    let ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    assert_eq!(ws.read(&space, "target/x").unwrap(), b"x");
    fs::write(space.path().join("classes"), "[secret]\ntarget/\n").unwrap();
    assert!(matches!(
        ws.read(&space, "target/x"),
        Err(Error::NotFound(_))
    ));
    fs::remove_file(space.path().join("classes")).unwrap();
    assert_eq!(ws.read(&space, "target/x").unwrap(), b"x");
}

#[test]
fn cached_files_are_copied_up_on_write_and_stay_removed() {
    let (_dir, mut space) = space();
    let mut build = space.create_workspace("build", "main").unwrap();
    build_output(&space, &mut build, "target/debug/a", "a", long_ago(1));
    build_output(&space, &mut build, "target/debug/b", "b", long_ago(1));
    build_output(&space, &mut build, "target/debug/deps/c", "c", long_ago(1));
    build_output(&space, &mut build, "target/release/d", "d", long_ago(1));
    space.save_cache("bc", "build").unwrap();
    let mut ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    let version_a = in_version(&space, &ws, "target/debug/a");
    assert!(matches!(
        ws.locate(&space, &path("target/debug/a")).unwrap(),
        Some(Located::Cached(local)) if local == version_a
    ));

    // The first write copies up, keeping the mode and date; the version is unchanged.
    let local = ws.copy_up(&space, &path("target/debug/a")).unwrap();
    assert_ne!(local, version_a);
    assert_eq!(modified(&local), long_ago(1));
    fs::write(&local, "rebuilt").unwrap();
    assert_eq!(ws.read(&space, "target/debug/a").unwrap(), b"rebuilt");
    assert_eq!(fs::read(&version_a).unwrap(), b"a");
    ws.write(&space, "target/debug/deps/c", b"new c").unwrap();
    assert_eq!(
        fs::read(in_version(&space, &ws, "target/debug/deps/c")).unwrap(),
        b"c"
    );

    // Removing and renaming leave whiteouts, kept across reopening and commits.
    ws.remove(&space, "target/debug/b").unwrap();
    ws.rename(&space, &path("target/release/d"), &path("target/moved"))
        .unwrap();
    assert!(matches!(
        ws.rename(&space, &path("target/release"), &path("target/r2")),
        Err(Error::DirectoryRename(_))
    ));
    ws.remove(&space, "target/release").unwrap();
    ws.create_entry(
        &space,
        &path("target/release"),
        peerfs_core::NewEntry::Directory,
    )
    .unwrap();
    assert_eq!(ws.changes(&space).unwrap(), Vec::new());
    let mut edit = space.workspace("w").unwrap();
    edit.write(&space, "README", b"readme\n").unwrap();
    commit(&mut space, &mut edit);
    let ws = space.workspace("w").unwrap();
    assert!(matches!(
        ws.read(&space, "target/debug/b"),
        Err(Error::NotFound(_))
    ));
    assert_eq!(ws.read(&space, "target/moved").unwrap(), b"d");
    assert_eq!(ws.list(&space, "target/release").unwrap(), Vec::new());
    assert_eq!(
        names(ws.list(&space, "target/debug").unwrap()),
        ["a", "deps"]
    );
    assert!(in_version(&space, &ws, "target/debug/b").exists());
    assert!(in_version(&space, &ws, "target/release/d").exists());

    // A cached file holding a mode keeps it through the copy.
    let exe = in_version(&space, &ws, "target/debug/deps/c");
    assert_eq!(
        fs::metadata(exe).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn an_old_version_stays_while_a_workspace_holds_it() {
    let (_dir, mut space) = space();
    build_and_save(&mut space, "first", None);
    let holder = space
        .create_workspace_with_cache("holder", "main", Some("bc"))
        .unwrap();
    let first = holder.cache().unwrap().version.clone();
    let first_file = in_version(&space, &holder, "target/debug/first.rlib");

    // The refresh holds the current version and saves the next; the holder keeps the first.
    build_and_save(&mut space, "second", Some("bc"));
    let cache = space.cache("bc").unwrap().unwrap();
    assert_ne!(cache.version, first);
    assert_eq!(cache.versions, 2);
    assert_eq!(fs::read(&first_file).unwrap(), b"first");
    let holder = space.workspace("holder").unwrap();
    assert_eq!(
        holder.read(&space, "target/debug/first.rlib").unwrap(),
        b"first"
    );
    assert!(matches!(
        holder.read(&space, "target/debug/second.rlib"),
        Err(Error::NotFound(_))
    ));
    // The new version holds both builds' output.
    let next = space
        .create_workspace_with_cache("next", "main", Some("bc"))
        .unwrap();
    assert_eq!(
        next.read(&space, "target/debug/first.rlib").unwrap(),
        b"first"
    );
    assert_eq!(
        next.read(&space, "target/debug/second.rlib").unwrap(),
        b"second"
    );

    // Released, it goes at the next collection.
    space.discard_workspace("holder").unwrap();
    assert!(first_file.exists());
    let report = space.collect_garbage(&GcOptions::default()).unwrap();
    assert_eq!(report.cache_versions_dropped, 1);
    assert!(!first_file.exists());
    assert_eq!(space.cache("bc").unwrap().unwrap().versions, 1);

    // Removing the cache keeps what a workspace holds, and new workspaces get none.
    space.remove_cache("bc").unwrap();
    assert_eq!(space.caches().unwrap(), Vec::new());
    assert_eq!(
        next.read(&space, "target/debug/second.rlib").unwrap(),
        b"second"
    );
    let none = space
        .create_workspace_with_cache("none", "main", Some("bc"))
        .unwrap();
    assert_eq!(none.cache(), None);
    space.discard_workspace("next").unwrap();
    // The next save removes what no workspace holds any more.
    build_and_save(&mut space, "third", None);
    assert_eq!(
        fs::read_dir(space.path().join("caches/versions"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn unchanged_sources_are_dated_as_the_cache_had_them_and_cached_files_keep_their_dates() {
    let (_dir, mut space) = space();
    let mut build = space.create_workspace("build", "main").unwrap();
    let cargo_toml = stamp(&space, &mut build, "Cargo.toml");
    let lib = stamp(&space, &mut build, "src/lib.rs");
    build_output(&space, &mut build, "target/debug/a.rlib", "a", long_ago(5));
    space.save_cache("bc", "build").unwrap();

    // The branch moves on: `src/lib.rs` changes, `Cargo.toml` does not.
    std::thread::sleep(Duration::from_millis(20));
    let mut edit = space.create_workspace("edit", "main").unwrap();
    edit.write(&space, "src/lib.rs", b"pub fn two() {}\n")
        .unwrap();
    commit(&mut space, &mut edit);
    std::thread::sleep(Duration::from_millis(20));
    let before = SystemTime::now();
    let mut ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    assert_eq!(stamp(&space, &mut ws, "Cargo.toml"), cargo_toml);
    let changed = stamp(&space, &mut ws, "src/lib.rs");
    assert_ne!(changed, lib);
    assert!(
        changed >= before,
        "a changed source is newer than the cache"
    );
    // The cached file keeps the date it was saved with.
    let Some(Located::Cached(cached)) = ws.locate(&space, &path("target/debug/a.rlib")).unwrap()
    else {
        panic!("not cached");
    };
    assert_eq!(modified(&cached), long_ago(5));

    // A workspace without the cache dates the unchanged file afresh.
    let mut plain = space.create_workspace("plain", "main").unwrap();
    assert_ne!(stamp(&space, &mut plain, "Cargo.toml"), cargo_toml);

    // Saved again, the chain holds: the unchanged file keeps the first build's date.
    space.save_cache("bc", "w").unwrap();
    let mut third = space
        .create_workspace_with_cache("third", "main", Some("bc"))
        .unwrap();
    assert_eq!(stamp(&space, &mut third, "Cargo.toml"), cargo_toml);
    assert_eq!(stamp(&space, &mut third, "src/lib.rs"), changed);
}

#[test]
fn a_version_left_half_saved_or_never_named_is_not_current_and_is_collected() {
    let (_dir, mut space) = space();
    build_and_save(&mut space, "first", None);
    let current = space.cache("bc").unwrap().unwrap();
    let versions = space.path().join("caches/versions");
    // As a crash leaves them: a version still being saved, and one saved but never named.
    let staging = versions.join(".new-0123456789abcdef0123456789abcdef");
    fs::create_dir_all(staging.join("files/target")).unwrap();
    let orphan = versions.join("fedcba9876543210fedcba9876543210");
    fs::create_dir_all(orphan.join("files")).unwrap();
    fs::write(
        orphan.join("meta"),
        format!(
            "cache bc\ncommit {}\nsaved 1.000000000\nbytes 0\n",
            current.commit
        ),
    )
    .unwrap();
    assert_eq!(space.cache("bc").unwrap().unwrap().version, current.version);
    let ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    assert_eq!(ws.cache().unwrap().version, current.version);

    let report = space
        .collect_garbage(&GcOptions {
            dry_run: true,
            ..GcOptions::default()
        })
        .unwrap();
    assert_eq!(report.cache_versions_dropped, 1);
    assert!(orphan.exists());
    let report = space.collect_garbage(&GcOptions::default()).unwrap();
    assert_eq!(report.cache_versions_dropped, 1);
    assert!(!orphan.exists() && !staging.exists());
    assert_eq!(space.cache("bc").unwrap().unwrap().version, current.version);
}

#[test]
fn the_held_version_is_found_from_the_workspace_state_alone() {
    let (_dir, mut space) = space();
    build_and_save(&mut space, "first", None);
    let ws = space
        .create_workspace_with_cache("w", "main", Some("bc"))
        .unwrap();
    let version = ws.cache().unwrap().version.clone();
    assert_eq!(
        peerfs_core::held_cache_version(space.path(), "w").unwrap(),
        Some(Path::new("caches/versions").join(version))
    );
    space.create_workspace("plain", "main").unwrap();
    assert_eq!(
        peerfs_core::held_cache_version(space.path(), "plain").unwrap(),
        None
    );
}
