//! Workspaces: reading, writing, committing, and merging parallel work.

use peerfs_core::{
    Author, Change, CommitOutcome, ConflictKind, EntryKind, Error, Merged, Space, TreePath,
};

fn author(agent: &str) -> Author {
    Author {
        host: "test-host".to_owned(),
        agent: Some(agent.to_owned()),
        session: None,
    }
}

fn committed(outcome: CommitOutcome) -> Merged {
    match outcome {
        CommitOutcome::Committed { merged, .. } => merged,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// A space whose `main` holds `files`.
fn space_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Space) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let mut space = Space::init(dir.path().join("s"), &author("setup")).expect("init");
    if files.is_empty() {
        return (dir, space);
    }
    let mut setup = space.create_workspace("setup", "main").expect("workspace");
    for (path, text) in files {
        setup.write(&space, path, text.as_bytes()).expect("write");
    }
    committed(
        space
            .commit_workspace(&mut setup, "setup", &author("setup"))
            .expect("commit"),
    );
    (dir, space)
}

#[test]
fn reads_see_the_overlay_over_the_base() {
    let (_dir, mut space) = space_with(&[("a.txt", "base\n"), ("dir/b.txt", "b\n")]);
    let mut ws = space.create_workspace("w", "main").unwrap();
    assert_eq!(ws.read(&space, "a.txt").unwrap(), b"base\n");

    ws.write(&space, "a.txt", b"mine\n").unwrap();
    ws.write(&space, "new/c.txt", b"c\n").unwrap();
    ws.remove(&space, "dir").unwrap();

    assert_eq!(ws.read(&space, "a.txt").unwrap(), b"mine\n");
    assert!(matches!(
        ws.read(&space, "dir/b.txt"),
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        ws.list(&space, "").unwrap(),
        vec![
            ("a.txt".to_owned(), EntryKind::File),
            ("new".to_owned(), EntryKind::Directory),
        ]
    );
    assert_eq!(
        ws.changes(&space).unwrap(),
        vec![
            Change::Written(TreePath::try_from("a.txt").unwrap()),
            Change::Added(TreePath::try_from("new/c.txt").unwrap()),
            Change::Removed(TreePath::try_from("dir").unwrap()),
        ]
    );
    // The branch is untouched until a commit.
    let fresh = space.create_workspace("fresh", "main").unwrap();
    assert_eq!(fresh.read(&space, "a.txt").unwrap(), b"base\n");
}

#[test]
fn a_removed_directory_can_be_written_into_again() {
    let (_dir, mut space) = space_with(&[("d/old.txt", "old\n")]);
    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.remove(&space, "d").unwrap();
    ws.write(&space, "d/new.txt", b"new\n").unwrap();
    committed(
        space
            .commit_workspace(&mut ws, "replace", &author("a"))
            .unwrap(),
    );

    let check = space.create_workspace("check", "main").unwrap();
    assert_eq!(
        check.list(&space, "d").unwrap(),
        vec![("new.txt".to_owned(), EntryKind::File)]
    );
}

#[test]
fn workspaces_survive_reopening_the_space() {
    let (dir, mut space) = space_with(&[("a.txt", "a\n")]);
    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.write(&space, "a.txt", b"changed\n").unwrap();
    ws.remove(&space, "a.txt").unwrap();
    ws.write(&space, "b.txt", b"b\n").unwrap();
    drop(ws);
    drop(space);

    let space = Space::open(dir.path().join("s")).unwrap();
    let ws = space.workspace("w").unwrap();
    assert!(matches!(ws.read(&space, "a.txt"), Err(Error::NotFound(_))));
    assert_eq!(ws.read(&space, "b.txt").unwrap(), b"b\n");
}

#[test]
fn an_unchanged_workspace_commits_nothing() {
    let (_dir, mut space) = space_with(&[("a.txt", "a\n")]);
    let mut ws = space.create_workspace("w", "main").unwrap();
    let head = space.branch("main").unwrap();
    assert_eq!(
        space
            .commit_workspace(&mut ws, "nothing", &author("a"))
            .unwrap(),
        CommitOutcome::Unchanged
    );
    assert_eq!(space.branch("main").unwrap(), head);
}

#[test]
fn parallel_agents_editing_different_lines_merge_cleanly() {
    let text = "one\ntwo\nthree\nfour\nfive\n";
    let (_dir, mut space) = space_with(&[("notes.txt", text), ("other.txt", "x\n")]);
    let mut first = space.create_workspace("first", "main").unwrap();
    let mut second = space.create_workspace("second", "main").unwrap();

    first
        .write(&space, "notes.txt", b"ONE\ntwo\nthree\nfour\nfive\n")
        .unwrap();
    second
        .write(&space, "notes.txt", b"one\ntwo\nthree\nfour\nFIVE\n")
        .unwrap();
    second.remove(&space, "other.txt").unwrap();

    assert_eq!(
        committed(
            space
                .commit_workspace(&mut first, "first", &author("a"))
                .unwrap()
        ),
        Merged::No
    );
    assert_eq!(
        committed(
            space
                .commit_workspace(&mut second, "second", &author("b"))
                .unwrap()
        ),
        Merged::Yes
    );

    let check = space.create_workspace("check", "main").unwrap();
    assert_eq!(
        check.read(&space, "notes.txt").unwrap(),
        b"ONE\ntwo\nthree\nfour\nFIVE\n"
    );
    assert!(matches!(
        check.read(&space, "other.txt"),
        Err(Error::NotFound(_))
    ));
    let head = space.branch("main").unwrap().unwrap();
    assert_eq!(space.history(head).unwrap().len(), 4);
}

#[test]
fn the_same_line_conflicts_and_leaves_markers_to_resolve() {
    let (_dir, mut space) = space_with(&[("f.txt", "shared line\n")]);
    let mut first = space.create_workspace("first", "main").unwrap();
    let mut second = space.create_workspace("second", "main").unwrap();
    first.write(&space, "f.txt", b"first's line\n").unwrap();
    second.write(&space, "f.txt", b"second's line\n").unwrap();
    second.write(&space, "clean.txt", b"kept\n").unwrap();

    committed(
        space
            .commit_workspace(&mut first, "first", &author("a"))
            .unwrap(),
    );
    let head = space.branch("main").unwrap();
    let CommitOutcome::Conflicted(conflicts) = space
        .commit_workspace(&mut second, "second", &author("b"))
        .unwrap()
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].kind, ConflictKind::Content);
    assert_eq!(conflicts[0].path.to_string(), "f.txt");
    assert_eq!(space.branch("main").unwrap(), head, "nothing was committed");

    // The workspace now follows the new head and holds the merge.
    assert_eq!(Some(second.base()), head);
    let marked = String::from_utf8(second.read(&space, "f.txt").unwrap()).unwrap();
    assert!(
        marked.contains("<<<<<<<") && marked.contains("first's line"),
        "{marked}"
    );
    assert_eq!(second.read(&space, "clean.txt").unwrap(), b"kept\n");

    second.write(&space, "f.txt", b"both lines\n").unwrap();
    assert_eq!(
        committed(
            space
                .commit_workspace(&mut second, "resolved", &author("b"))
                .unwrap()
        ),
        Merged::No
    );
    let check = space.create_workspace("check", "main").unwrap();
    assert_eq!(check.read(&space, "f.txt").unwrap(), b"both lines\n");
    assert_eq!(check.read(&space, "clean.txt").unwrap(), b"kept\n");
}

#[test]
fn binary_and_delete_conflicts_keep_this_sides_version() {
    let (_dir, mut space) = space_with(&[("image.bin", "\0base"), ("doc.txt", "doc\n")]);
    let mut first = space.create_workspace("first", "main").unwrap();
    let mut second = space.create_workspace("second", "main").unwrap();
    first.write(&space, "image.bin", b"\0first").unwrap();
    first.remove(&space, "doc.txt").unwrap();
    second.write(&space, "image.bin", b"\0second").unwrap();
    second.write(&space, "doc.txt", b"doc, edited\n").unwrap();

    committed(
        space
            .commit_workspace(&mut first, "first", &author("a"))
            .unwrap(),
    );
    let CommitOutcome::Conflicted(conflicts) = space
        .commit_workspace(&mut second, "second", &author("b"))
        .unwrap()
    else {
        panic!("expected conflicts");
    };
    let mut kinds: Vec<(String, ConflictKind)> = conflicts
        .iter()
        .map(|c| (c.path.to_string(), c.kind))
        .collect();
    kinds.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kinds,
        vec![
            ("doc.txt".to_owned(), ConflictKind::ModifyDelete),
            ("image.bin".to_owned(), ConflictKind::Binary),
        ]
    );
    assert_eq!(second.read(&space, "image.bin").unwrap(), b"\0second");
    assert_eq!(second.read(&space, "doc.txt").unwrap(), b"doc, edited\n");
}

#[test]
fn both_adding_the_same_path_is_an_add_add_conflict() {
    let (_dir, mut space) = space_with(&[("keep.txt", "k\n")]);
    let mut first = space.create_workspace("first", "main").unwrap();
    let mut second = space.create_workspace("second", "main").unwrap();
    first.write(&space, "new.txt", b"from first\n").unwrap();
    second.write(&space, "new.txt", b"from second\n").unwrap();
    committed(
        space
            .commit_workspace(&mut first, "first", &author("a"))
            .unwrap(),
    );
    let CommitOutcome::Conflicted(conflicts) = space
        .commit_workspace(&mut second, "second", &author("b"))
        .unwrap()
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflicts[0].kind, ConflictKind::AddAdd);
}

#[test]
fn a_workspace_needs_an_existing_branch_and_a_fresh_name() {
    let (_dir, mut space) = space_with(&[]);
    assert!(matches!(
        space.create_workspace("w", "nope"),
        Err(Error::NoSuchBranch(_))
    ));
    space.create_workspace("w", "main").unwrap();
    assert!(matches!(
        space.create_workspace("w", "main"),
        Err(Error::WorkspaceExists(_))
    ));
    space.discard_workspace("w").unwrap();
    assert!(matches!(
        space.workspace("w"),
        Err(Error::NoSuchWorkspace(_))
    ));
}
