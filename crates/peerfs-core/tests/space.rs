//! Spaces, content, branches and import/export through the public API.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use peerfs_core::{Author, Content, Entry, Error, FileMode, Space, TreePath};

fn author() -> Author {
    Author {
        host: "test-host".to_owned(),
        ..Author::default()
    }
}

/// Deterministic pseudo-random bytes, so chunk boundaries are reproducible.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed.to_le_bytes()[0]
        })
        .collect()
}

#[test]
fn small_files_are_one_blob_and_large_files_are_chunked() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("s"), &author()).unwrap();

    assert_eq!(space.write_content(b"").unwrap(), Content::Empty);
    let small = space.write_content(&[1; 1000]).unwrap();
    assert!(matches!(small, Content::Blob(_)));

    let big = noise(3 * 1024 * 1024, 1);
    let chunked = space.write_content(&big).unwrap();
    assert!(matches!(chunked, Content::Chunked(_)));
    assert_eq!(space.read_content(&chunked).unwrap(), big);
}

#[test]
fn an_edit_in_a_large_file_stores_only_nearby_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("s"), &author()).unwrap();
    let mut file = noise(4 * 1024 * 1024, 7);
    space.write_content(&file).unwrap();
    let before = space.object_count();

    file[2 * 1024 * 1024] ^= 0xff;
    space.write_content(&file).unwrap();
    let added = space.object_count() - before;

    // The changed chunk, perhaps a neighbour, and the new chunk list.
    assert!(added <= 4, "a one-byte edit added {added} objects");
}

#[test]
fn identical_content_is_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("s"), &author()).unwrap();
    let a = space.write_content(b"same bytes").unwrap();
    let count = space.object_count();
    let b = space.write_content(b"same bytes").unwrap();
    assert_eq!(a, b);
    assert_eq!(space.object_count(), count);
}

#[test]
fn import_and_export_round_trip_modes_links_and_empty_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    fs::create_dir_all(source.join("src/bin")).unwrap();
    fs::write(source.join("README.md"), "hello\n").unwrap();
    fs::write(source.join("empty"), "").unwrap();
    fs::write(source.join("src/bin/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(
        source.join("src/bin/run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("README.md", source.join("link")).unwrap();
    fs::write(source.join("big.bin"), noise(300_000, 3)).unwrap();

    let mut space = Space::init(dir.path().join("s"), &author()).unwrap();
    let tree = space.import(&source).unwrap();
    let script = space
        .entry(tree, &TreePath::try_from("src/bin/run.sh").unwrap())
        .unwrap();
    assert!(matches!(
        script,
        Some(Entry::File {
            mode: FileMode::Executable,
            ..
        })
    ));

    let target = dir.path().join("target");
    space.export(tree, &target).unwrap();
    assert_eq!(fs::read(target.join("README.md")).unwrap(), b"hello\n");
    assert_eq!(fs::read(target.join("empty")).unwrap(), b"");
    assert_eq!(fs::read(target.join("big.bin")).unwrap(), noise(300_000, 3));
    assert_eq!(
        fs::read_link(target.join("link")).unwrap().to_str(),
        Some("README.md")
    );
    let mode = fs::metadata(target.join("src/bin/run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);

    // Importing the export again gives the same tree: identity depends only on content.
    assert_eq!(space.import(&target).unwrap(), tree);
}

#[test]
fn branches_fork_without_copying_and_move_only_by_compare_and_swap() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("s"), &author()).unwrap();
    let main = space.branch("main").unwrap().unwrap();
    let count = space.object_count();

    space.create_branch("experiment", main).unwrap();
    assert_eq!(space.object_count(), count);
    assert!(matches!(
        space.create_branch("experiment", main),
        Err(Error::BranchExists(_))
    ));

    let tree = space.import(dir.path().join("s/refs")).unwrap();
    let next = space
        .commit_tree("experiment", tree, "next", &author())
        .unwrap();
    let moved = space.update_branch("experiment", Some(main), next);
    assert!(matches!(moved, Err(Error::BranchMoved { .. })));
    assert_eq!(space.history(next).unwrap().len(), 2);
}

#[test]
fn a_space_is_held_by_one_process_and_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s");
    let space = Space::init(&path, &author()).unwrap();
    let head = space.branch("main").unwrap();
    assert!(matches!(Space::open(&path), Err(Error::SpaceInUse { .. })));
    drop(space);
    let space = Space::open(&path).unwrap();
    assert_eq!(space.branch("main").unwrap(), head);
}

#[test]
fn init_refuses_a_directory_with_files_in_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("keep"), "x").unwrap();
    assert!(matches!(
        Space::init(dir.path(), &author()),
        Err(Error::NotASpace { .. })
    ));
}
