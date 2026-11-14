//! What a branch reaches: the objects a host may serve to a reader of that branch.

use peerfs_core::{Author, CommitOutcome, Content, Entry, ObjectId, Space};

fn author() -> Author {
    Author {
        host: "laptop".to_owned(),
        agent: None,
        session: None,
    }
}

fn commit(space: &mut Space, ws: &mut peerfs_core::Workspace, message: &str) -> ObjectId {
    match space
        .commit_workspace(ws, message, &author())
        .expect("commit")
    {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("not committed: {other:?}"),
    }
}

/// The ids a commit's tree names directly: its file contents.
fn contents(space: &Space, commit: ObjectId) -> Vec<ObjectId> {
    let tree = space
        .tree(space.commit(commit).expect("commit").tree)
        .expect("tree");
    tree.iter()
        .filter_map(|(_, entry)| match entry {
            Entry::File {
                content: Content::Blob(id) | Content::Chunked(id),
                ..
            } => Some(*id),
            _ => None,
        })
        .collect()
}

#[test]
fn a_branch_reaches_its_history_but_not_snapshots_or_other_branches() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &author()).unwrap();
    let first_head = space.branch("main").unwrap().unwrap();

    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.write(&space, "small.txt", b"one\n").unwrap();
    // Large enough to be stored in chunks.
    ws.write(&space, "big.bin", &vec![7u8; 300 * 1024]).unwrap();
    let snapshot = space
        .snapshot_workspace(&mut ws, "while working", &author())
        .unwrap()
        .unwrap();
    ws.write(&space, "small.txt", b"two\n").unwrap();
    let older = commit(&mut space, &mut ws, "first change");
    ws.write(&space, "small.txt", b"three\n").unwrap();
    let newer = commit(&mut space, &mut ws, "second change");

    let older_contents = contents(&space, older);
    assert_eq!(older_contents.len(), 2);
    let mut reached = vec![newer, older, first_head, space.commit(older).unwrap().tree];
    reached.extend(&older_contents);
    reached.extend(contents(&space, newer));
    assert!(space.branch_reaches("main", &reached).unwrap());

    // The snapshot is the older commit's change's predecessor link, which is not followed,
    // and the snapshot's own small.txt ("one") is reached by nothing else.
    assert!(!space.branch_reaches("main", &[snapshot]).unwrap());
    let snapshot_only = contents(&space, snapshot)
        .into_iter()
        .filter(|id| !older_contents.contains(id) && !contents(&space, newer).contains(id))
        .collect::<Vec<_>>();
    assert_ne!(snapshot_only, Vec::<ObjectId>::new());
    assert!(!space.branch_reaches("main", &snapshot_only).unwrap());
    // One unreached id makes the whole set unreached.
    assert!(!space.branch_reaches("main", &[newer, snapshot]).unwrap());

    // Another branch's own objects are not reached by main, and main's are by it only
    // where its history includes them.
    space.create_branch("side", first_head).unwrap();
    let mut side = space.create_workspace("s", "side").unwrap();
    side.write(&space, "other.txt", b"side only\n").unwrap();
    let side_head = commit(&mut space, &mut side, "side change");
    assert!(!space.branch_reaches("main", &[side_head]).unwrap());
    assert!(
        !space
            .branch_reaches("main", &contents(&space, side_head))
            .unwrap()
    );
    assert!(
        space
            .branch_reaches("side", &[side_head, first_head])
            .unwrap()
    );
    assert!(!space.branch_reaches("side", &[older]).unwrap());

    // A branch that does not exist reaches nothing.
    assert!(!space.branch_reaches("gone", &[first_head]).unwrap());
}
