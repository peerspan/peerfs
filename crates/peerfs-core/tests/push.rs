//! Pushing to a branch's home host, and merging another host's work back.

use peerfs_core::{
    Author, CommitOutcome, ConflictKind, Error, MergeOutcome, ObjectId, RawObject, Remote, Result,
    Space,
};

fn author(host: &str) -> Author {
    Author {
        host: host.to_owned(),
        agent: Some("agent".to_owned()),
        session: None,
    }
}

struct Direct<'space>(&'space Space);

impl Remote for Direct<'_> {
    fn branch(&mut self, branch: &str) -> Result<Option<ObjectId>> {
        self.0.branch(branch)
    }

    fn fetch(&mut self, ids: &[ObjectId]) -> Result<Vec<RawObject>> {
        let mut objects = Vec::new();
        for id in ids {
            objects.extend(self.0.raw_object(*id)?);
        }
        Ok(objects)
    }
}

/// Sends `head` and whatever of its history `to` lacks; returns the objects sent.
fn send(from: &Space, to: &mut Space, head: ObjectId) -> Vec<RawObject> {
    let mut push = from.start_push(head).expect("start");
    loop {
        let ask = push.to_ask();
        if ask.is_empty() {
            break;
        }
        let answers = to.have(&ask);
        from.answer_push(&mut push, &answers).expect("answer");
    }
    let objects: Vec<RawObject> = from
        .push_order(&push)
        .expect("order")
        .into_iter()
        .map(|id| from.raw_object(id).expect("read").expect("held"))
        .collect();
    to.accept_objects(&objects).expect("accept");
    objects
}

fn commit(space: &mut Space, workspace: &str, files: &[(&str, &str)]) -> ObjectId {
    let mut ws = space.workspace(workspace).expect("workspace");
    for (path, text) in files {
        ws.write(space, path, text.as_bytes()).expect("write");
    }
    match space
        .commit_workspace(&mut ws, "work", &author("test"))
        .expect("commit")
    {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// A server whose main has a file, and a laptop that pulled it and forked `work`.
fn pair(dir: &tempfile::TempDir) -> (Space, Space) {
    let mut server = Space::init(dir.path().join("server"), &author("server")).expect("init");
    server.create_workspace("w", "main").expect("workspace");
    commit(
        &mut server,
        "w",
        &[("shared.txt", "one\ntwo\nthree\nfour\nfive\n")],
    );
    let mut laptop = Space::init(dir.path().join("laptop"), &author("laptop")).expect("init");
    laptop
        .pull(&mut Direct(&server), "server", "main")
        .expect("pull");
    let head = laptop
        .resolve("server/main")
        .expect("resolve")
        .expect("pulled");
    laptop.create_branch("work", head).expect("fork");
    laptop.create_workspace("w", "work").expect("workspace");
    (server, laptop)
}

#[test]
fn a_push_sends_only_missing_objects_and_fast_forwards_the_home_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, mut laptop) = pair(&dir);
    let server_head = server.branch("main").unwrap();
    let mine = commit(&mut laptop, "w", &[("new.txt", "from the laptop\n")]);

    let sent = send(&laptop, &mut server, mine);
    // The commit, the root tree and the new file: nothing the server had.
    assert_eq!(sent.len(), 3, "{sent:?}");
    server.advance_branch("main", server_head, mine).unwrap();
    assert_eq!(server.branch("main").unwrap(), Some(mine));
    laptop.record_tracking("server", "main", mine).unwrap();
    assert_eq!(laptop.resolve("server/main").unwrap(), Some(mine));

    let check = server.create_workspace("check", "main").unwrap();
    assert_eq!(
        check.read(&server, "new.txt").unwrap(),
        b"from the laptop\n"
    );
    // Sending again sends nothing.
    assert!(send(&laptop, &mut server, mine).is_empty());
}

#[test]
fn the_home_host_refuses_a_push_that_would_drop_commits() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, mut laptop) = pair(&dir);
    let seen = server.branch("main").unwrap();
    let theirs = commit(&mut server, "w", &[("server.txt", "server work\n")]);
    let mine = commit(&mut laptop, "w", &[("laptop.txt", "laptop work\n")]);
    send(&laptop, &mut server, mine);

    // Against the head the laptop last saw: the branch moved.
    assert!(matches!(
        server.advance_branch("main", seen, mine),
        Err(Error::BranchMoved { .. })
    ));
    // Against the current head: the laptop's commit does not contain it.
    assert!(matches!(
        server.advance_branch("main", Some(theirs), mine),
        Err(Error::NotFastForward { .. })
    ));
    assert_eq!(server.branch("main").unwrap(), Some(theirs));
}

#[test]
fn receiving_refuses_objects_that_would_leave_a_hole() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, mut laptop) = pair(&dir);
    let mine = commit(&mut laptop, "w", &[("new.txt", "x\n")]);

    // The commit alone, without the tree it references.
    let commit_only = vec![laptop.raw_object(mine).unwrap().unwrap()];
    assert!(matches!(
        server.accept_objects(&commit_only),
        Err(Error::RemoteObject {
            problem: "references an object this space does not hold",
            ..
        })
    ));
    assert!(!server.have(&[mine])[0]);

    let mut tampered = commit_only;
    tampered[0].bytes.push(1);
    assert!(matches!(
        server.accept_objects(&tampered),
        Err(Error::RemoteObject {
            problem: "does not match its identity",
            ..
        })
    ));
}

#[test]
fn merging_another_hosts_branch_fast_forwards_or_merges_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, mut laptop) = pair(&dir);
    let mut ws = laptop.workspace("w").unwrap();
    assert_eq!(
        laptop
            .merge_into_workspace(&mut ws, "server/main", &author("laptop"))
            .unwrap(),
        MergeOutcome::UpToDate
    );

    // Only the server moved: a fast-forward.
    let theirs = commit(&mut server, "w", &[("a.txt", "a\n")]);
    laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    let mut ws = laptop.workspace("w").unwrap();
    assert_eq!(
        laptop
            .merge_into_workspace(&mut ws, "server/main", &author("laptop"))
            .unwrap(),
        MergeOutcome::FastForward { commit: theirs }
    );

    // Both moved, on different lines: a merge commit with both parents.
    commit(
        &mut server,
        "w",
        &[("shared.txt", "ONE\ntwo\nthree\nfour\nfive\n")],
    );
    let mine = commit(
        &mut laptop,
        "w",
        &[("shared.txt", "one\ntwo\nthree\nfour\nFIVE\n")],
    );
    laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    let server_head = laptop.resolve("server/main").unwrap().unwrap();
    let mut ws = laptop.workspace("w").unwrap();
    let MergeOutcome::Merged { commit: merge } = laptop
        .merge_into_workspace(&mut ws, "server/main", &author("laptop"))
        .unwrap()
    else {
        panic!("expected a clean merge");
    };
    assert_eq!(
        laptop.commit(merge).unwrap().parents,
        vec![mine, server_head]
    );
    assert_eq!(
        ws.read(&laptop, "shared.txt").unwrap(),
        b"ONE\ntwo\nthree\nfour\nFIVE\n"
    );

    // The merge contains the server's head, so pushing it fast-forwards there.
    send(&laptop, &mut server, merge);
    server
        .advance_branch("main", Some(server_head), merge)
        .unwrap();
}

#[test]
fn a_conflicted_merge_is_resolved_in_the_workspace_with_both_parents() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, mut laptop) = pair(&dir);
    commit(&mut server, "w", &[("shared.txt", "server's line\n")]);
    let mine = commit(&mut laptop, "w", &[("shared.txt", "laptop's line\n")]);
    laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    let server_head = laptop.resolve("server/main").unwrap().unwrap();

    let mut ws = laptop.workspace("w").unwrap();
    let MergeOutcome::Conflicted(conflicts) = laptop
        .merge_into_workspace(&mut ws, "server/main", &author("laptop"))
        .unwrap()
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflicts[0].kind, ConflictKind::Content);
    assert_eq!(ws.merging(), Some(server_head));
    assert_eq!(
        laptop.branch("work").unwrap(),
        Some(mine),
        "nothing committed"
    );
    assert!(matches!(
        laptop.merge_into_workspace(&mut ws, "server/main", &author("laptop")),
        Err(Error::WorkspaceNotClean(_))
    ));

    // The merge survives reopening the workspace.
    let mut ws = laptop.workspace("w").unwrap();
    assert_eq!(ws.merging(), Some(server_head));
    ws.write(&laptop, "shared.txt", b"both lines\n").unwrap();
    let CommitOutcome::Committed { commit, .. } = laptop
        .commit_workspace(&mut ws, "resolve", &author("laptop"))
        .unwrap()
    else {
        panic!("expected a commit");
    };
    assert_eq!(
        laptop.commit(commit).unwrap().parents,
        vec![mine, server_head]
    );
    assert_eq!(ws.merging(), None);
    assert!(laptop.is_ancestor(server_head, commit).unwrap());
    assert_eq!(
        laptop.merge_base(commit, server_head).unwrap(),
        Some(server_head)
    );
}
