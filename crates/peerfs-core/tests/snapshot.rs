//! Unsigned snapshots per tool call, replaced by one signed commit per change.

use peerfs_core::{Author, CommitOutcome, Space, Workspace};

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
