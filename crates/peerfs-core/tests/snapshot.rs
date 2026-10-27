//! Unsigned snapshots per tool call, replaced by one signed commit per change.

use peerfs_core::{Author, CommitOutcome, Error, Space, Workspace};

fn author() -> Author {
    Author {
        host: "laptop".to_owned(),
        agent: Some("agent".to_owned()),
        session: None,
    }
}

fn space(dir: &tempfile::TempDir) -> (Space, Workspace) {
    let mut space = Space::init(dir.path().join("space"), &author()).expect("init");
    let workspace = space.create_workspace("w", "main").expect("workspace");
    (space, workspace)
}

#[test]
fn snapshots_chain_within_a_change_and_touch_neither_branch_nor_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let (mut space, mut ws) = space(&dir);
    let head = space.branch("main").unwrap();
    assert_eq!(
        space
            .snapshot_workspace(&mut ws, "nothing", &author())
            .unwrap(),
        None
    );

    ws.write(&space, "a.txt", b"one\n").unwrap();
    let first = space
        .snapshot_workspace(&mut ws, "Write", &author())
        .unwrap()
        .unwrap();
    assert_eq!(
        space
            .snapshot_workspace(&mut ws, "again", &author())
            .unwrap(),
        None,
        "unchanged since the last snapshot"
    );
    ws.write(&space, "a.txt", b"two\n").unwrap();
    let second = space
        .snapshot_workspace(&mut ws, "Edit", &author())
        .unwrap()
        .unwrap();

    let snapshots = space.snapshots(&ws).unwrap();
    let ids: Vec<_> = snapshots.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![second, first]);
    let (_, newest) = &snapshots[0];
    assert_eq!(newest.parents, vec![first]);
    assert_eq!(newest.change, ws.change());
    assert_eq!(newest.signature, None);
    assert_eq!(snapshots[1].1.parents, vec![ws.base()]);

    assert_eq!(space.branch("main").unwrap(), head);
    assert_eq!(ws.read(&space, "a.txt").unwrap(), b"two\n");
    // The workspace remembers its latest snapshot.
    let reopened = space.workspace("w").unwrap();
    assert_eq!(reopened.snapshot(), Some(second));
}

#[test]
fn committing_replaces_the_snapshots_with_one_signed_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (mut space, mut ws) = space(&dir);
    let head = space.branch("main").unwrap().unwrap();
    let change = ws.change();
    ws.write(&space, "a.txt", b"one\n").unwrap();
    space
        .snapshot_workspace(&mut ws, "Write", &author())
        .unwrap();
    // Changed after the last snapshot: the commit takes a final one.
    ws.write(&space, "b.txt", b"two\n").unwrap();

    let CommitOutcome::Committed { commit, .. } =
        space.commit_workspace(&mut ws, "Task", &author()).unwrap()
    else {
        panic!("expected a commit");
    };
    let signed = space.commit(commit).unwrap();
    assert!(signed.signature.is_some());
    assert_eq!(signed.parents, vec![head]);
    assert_eq!(signed.change, change);
    let last = space
        .commit(signed.predecessor.expect("a predecessor"))
        .unwrap();
    assert_eq!(last.tree, signed.tree);
    assert_eq!(last.message, "Task");
    assert_eq!(last.signature, None);

    // A new change starts, with no snapshots.
    assert_ne!(ws.change(), change);
    assert_eq!(ws.snapshot(), None);
    assert!(space.snapshots(&ws).unwrap().is_empty());
    assert_eq!(
        space.history(commit).unwrap().len(),
        2,
        "snapshots are not history"
    );

    // A change committed without snapshots names no predecessor.
    ws.write(&space, "c.txt", b"three\n").unwrap();
    let CommitOutcome::Committed { commit, .. } =
        space.commit_workspace(&mut ws, "Quick", &author()).unwrap()
    else {
        panic!("expected a commit");
    };
    assert_eq!(space.commit(commit).unwrap().predecessor, None);
}

#[test]
fn a_conflict_keeps_the_change_and_its_snapshots_until_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let (mut space, mut ws) = space(&dir);
    let mut other = space.create_workspace("other", "main").unwrap();
    other.write(&space, "a.txt", b"theirs\n").unwrap();
    space
        .commit_workspace(&mut other, "Theirs", &author())
        .unwrap();

    let change = ws.change();
    ws.write(&space, "a.txt", b"ours\n").unwrap();
    let first = space
        .snapshot_workspace(&mut ws, "Write", &author())
        .unwrap()
        .unwrap();
    assert!(matches!(
        space.commit_workspace(&mut ws, "Task", &author()).unwrap(),
        CommitOutcome::Conflicted(_)
    ));
    assert_eq!(ws.change(), change);
    assert_eq!(space.snapshots(&ws).unwrap()[0].0, first);

    ws.write(&space, "a.txt", b"both\n").unwrap();
    let CommitOutcome::Committed { commit, .. } =
        space.commit_workspace(&mut ws, "Task", &author()).unwrap()
    else {
        panic!("expected a commit");
    };
    let signed = space.commit(commit).unwrap();
    assert_eq!(signed.change, change);
    let resolved = space.commit(signed.predecessor.unwrap()).unwrap();
    assert_eq!(
        resolved.parents,
        vec![first],
        "the chain runs through the conflict"
    );
    assert_eq!(resolved.tree, signed.tree);
}

#[test]
fn restoring_a_snapshot_keeps_every_step_and_the_local_paths() {
    let dir = tempfile::tempdir().unwrap();
    let (mut space, mut ws) = space(&dir);
    ws.write(&space, "a.txt", b"one\n").unwrap();
    let first = space
        .snapshot_workspace(&mut ws, "Write a", &author())
        .unwrap()
        .unwrap();
    ws.write(&space, "a.txt", b"two\n").unwrap();
    ws.write(&space, "b.txt", b"new\n").unwrap();
    let second = space
        .snapshot_workspace(&mut ws, "Edit a, write b", &author())
        .unwrap()
        .unwrap();
    // Unsnapshotted work, and build output no snapshot holds.
    ws.write(&space, "a.txt", b"three\n").unwrap();
    ws.write(&space, "target/out.bin", b"built\n").unwrap();
    let (base, change) = (ws.base(), ws.change());

    let prefix: String = first.to_string().chars().take(20).collect();
    let restored = space
        .restore_workspace(&mut ws, &prefix, &author())
        .unwrap();
    assert_eq!(restored.target, first);
    assert_eq!(ws.read(&space, "a.txt").unwrap(), b"one\n");
    assert!(ws.read(&space, "b.txt").is_err(), "b.txt came later");
    assert_eq!(ws.read(&space, "target/out.bin").unwrap(), b"built\n");
    assert_eq!((ws.base(), ws.change()), (base, change));

    let snapshots = space.snapshots(&ws).unwrap();
    let messages: Vec<_> = snapshots
        .iter()
        .map(|(_, commit)| commit.message.as_str())
        .collect();
    let short: String = first.to_string().chars().take(16).collect();
    assert_eq!(
        messages,
        [
            format!("Restore {short}").as_str(),
            &format!("Before restoring {short}"),
            "Edit a, write b",
            "Write a",
        ]
    );
    let before = restored.before.expect("the unsnapshotted edit was saved");
    assert_eq!(snapshots[1].0, before);
    assert_eq!(snapshots[0].0, restored.snapshot.unwrap());
    assert_eq!(snapshots[0].1.tree, space.commit(first).unwrap().tree);

    // Forward again, to the saved state.
    space
        .restore_workspace(&mut ws, &before.to_string(), &author())
        .unwrap();
    assert_eq!(ws.read(&space, "a.txt").unwrap(), b"three\n");
    assert_eq!(ws.read(&space, "b.txt").unwrap(), b"new\n");
    let again = space
        .restore_workspace(&mut ws, &before.to_string(), &author())
        .unwrap();
    assert_eq!(
        (again.before, again.snapshot),
        (None, None),
        "already there"
    );

    // Committing takes the restored state.
    space
        .restore_workspace(&mut ws, &second.to_string(), &author())
        .unwrap();
    let CommitOutcome::Committed { commit, .. } =
        space.commit_workspace(&mut ws, "Task", &author()).unwrap()
    else {
        panic!("expected a commit");
    };
    assert_eq!(
        space.commit(commit).unwrap().tree,
        space.commit(second).unwrap().tree
    );
    assert_eq!(ws.read(&space, "target/out.bin").unwrap(), b"built\n");
}

#[test]
fn restore_names_one_snapshot_of_the_current_change() {
    let dir = tempfile::tempdir().unwrap();
    let (mut space, mut ws) = space(&dir);
    let restore = |space: &mut Space, ws: &mut Workspace, snapshot: &str| {
        space
            .restore_workspace(ws, snapshot, &author())
            .unwrap_err()
    };
    assert!(matches!(
        restore(&mut space, &mut ws, "01"),
        Error::NoSuchSnapshot { .. }
    ));
    ws.write(&space, "a.txt", b"one\n").unwrap();
    space.snapshot_workspace(&mut ws, "1", &author()).unwrap();
    ws.write(&space, "a.txt", b"two\n").unwrap();
    space.snapshot_workspace(&mut ws, "2", &author()).unwrap();
    // Every ID starts with its algorithm code.
    assert!(matches!(
        restore(&mut space, &mut ws, "01"),
        Error::AmbiguousSnapshot(_)
    ));
    assert!(matches!(
        restore(&mut space, &mut ws, ""),
        Error::NoSuchSnapshot { .. }
    ));
    let base = ws.base().to_string();
    assert!(
        matches!(
            restore(&mut space, &mut ws, &base),
            Error::NoSuchSnapshot { .. }
        ),
        "the base is not a snapshot"
    );

    // A conflicted commit moves the workspace onto the branch's head; its earlier snapshots
    // were taken over the old base, and restoring one would undo the head's changes.
    let mut other = space.create_workspace("other", "main").unwrap();
    other.write(&space, "a.txt", b"theirs\n").unwrap();
    space
        .commit_workspace(&mut other, "Theirs", &author())
        .unwrap();
    assert!(matches!(
        space.commit_workspace(&mut ws, "Task", &author()).unwrap(),
        CommitOutcome::Conflicted(_)
    ));
    let latest = space.snapshots(&ws).unwrap()[0].0.to_string();
    assert!(matches!(
        restore(&mut space, &mut ws, &latest),
        Error::BaseMoved(_)
    ));
}
