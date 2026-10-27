//! Pulling branches between spaces through the `Remote` interface.

use peerfs_core::{Author, CommitOutcome, Error, ObjectId, RawObject, Remote, Result, Space};

fn author(host: &str) -> Author {
    Author {
        host: host.to_owned(),
        agent: Some("agent".to_owned()),
        session: None,
    }
}

/// Another space, read directly.
struct Direct<'space>(&'space Space);

impl Remote for Direct<'_> {
    fn branch(&mut self, branch: &str) -> Result<Option<ObjectId>> {
        self.0.branch(branch)
    }

    fn fetch(&mut self, ids: &[ObjectId]) -> Result<Vec<RawObject>> {
        let mut objects = Vec::new();
        for id in ids {
            if let Some(object) = self.0.raw_object(*id)? {
                objects.push(object);
            }
        }
        Ok(objects)
    }
}

/// A remote that fails after answering `remaining` fetches.
struct Flaky<'space> {
    inner: Direct<'space>,
    remaining: usize,
}

impl Remote for Flaky<'_> {
    fn branch(&mut self, branch: &str) -> Result<Option<ObjectId>> {
        self.inner.branch(branch)
    }

    fn fetch(&mut self, ids: &[ObjectId]) -> Result<Vec<RawObject>> {
        if self.remaining == 0 {
            return Err(Error::Remote("connection lost".to_owned()));
        }
        self.remaining -= 1;
        self.inner.fetch(ids)
    }
}

/// A remote that alters what it sends.
struct Tampering<'space> {
    inner: Direct<'space>,
    edit: fn(&mut Vec<RawObject>),
}

impl Remote for Tampering<'_> {
    fn branch(&mut self, branch: &str) -> Result<Option<ObjectId>> {
        self.inner.branch(branch)
    }

    fn fetch(&mut self, ids: &[ObjectId]) -> Result<Vec<RawObject>> {
        let mut objects = self.inner.fetch(ids)?;
        (self.edit)(&mut objects);
        Ok(objects)
    }
}

/// A space on `host` whose `main` has a few commits, including a chunked file.
fn source(dir: &tempfile::TempDir, host: &str) -> Space {
    let mut space = Space::init(dir.path().join(host), &author(host)).expect("test setup");
    let mut ws = space.create_workspace("w", "main").expect("test setup");
    ws.write(&space, "README.md", b"hello\n")
        .expect("test setup");
    ws.write(&space, "src/lib.rs", b"pub fn one() {}\n")
        .expect("test setup");
    let big: Vec<u8> = (0..400_000u32)
        .map(|i| u8::try_from(i * 7 % 251).expect("below 251"))
        .collect();
    ws.write(&space, "data/big.bin", &big).expect("test setup");
    assert!(matches!(
        space
            .commit_workspace(&mut ws, "first", &author(host))
            .expect("test setup"),
        CommitOutcome::Committed { .. }
    ));
    ws.write(&space, "src/lib.rs", b"pub fn one() {}\npub fn two() {}\n")
        .expect("test setup");
    space
        .commit_workspace(&mut ws, "second", &author(host))
        .expect("test setup");
    space
}

fn exported(space: &Space, revision: &str, dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let commit = space
        .resolve(revision)
        .expect("test setup")
        .expect("test setup");
    space
        .export(space.commit(commit).expect("test setup").tree, dir)
        .expect("test setup");
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).expect("test setup") {
            let entry = entry.expect("test setup").path();
            if entry.is_dir() {
                stack.push(entry);
            } else {
                let name = entry
                    .strip_prefix(dir)
                    .expect("test setup")
                    .display()
                    .to_string();
                files.push((name, std::fs::read(&entry).expect("test setup")));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn a_pull_brings_the_whole_history_and_records_a_tracking_reference() {
    let dir = tempfile::tempdir().unwrap();
    let server = source(&dir, "server");
    let mut laptop = Space::init(dir.path().join("laptop"), &author("laptop")).unwrap();

    let pulled = laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    let head = server.branch("main").unwrap().unwrap();
    assert_eq!(pulled.head, head);
    assert!(pulled.objects > 5, "{pulled:?}");
    assert_eq!(laptop.resolve("server/main").unwrap(), Some(head));
    assert_eq!(laptop.history(head).unwrap().len(), 3);
    let tracking = laptop.tracking().unwrap();
    assert_eq!(tracking.len(), 1);
    assert_eq!(
        (tracking[0].remote.as_str(), tracking[0].branch.as_str()),
        ("server", "main")
    );

    assert_eq!(
        exported(&laptop, "server/main", &dir.path().join("a")),
        exported(&server, "main", &dir.path().join("b"))
    );
    // The laptop's own main is untouched; work starts from a fork of the tracking head.
    assert_ne!(laptop.branch("main").unwrap(), Some(head));
    laptop.create_branch("from-server", head).unwrap();
}

#[test]
fn a_second_pull_fetches_only_what_is_new() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = source(&dir, "server");
    let mut laptop = Space::init(dir.path().join("laptop"), &author("laptop")).unwrap();
    laptop.pull(&mut Direct(&server), "server", "main").unwrap();

    let again = laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    assert_eq!(again.objects, 0);

    let mut ws = server.workspace("w").unwrap();
    ws.write(&server, "src/lib.rs", b"changed\n").unwrap();
    server
        .commit_workspace(&mut ws, "third", &author("server"))
        .unwrap();
    let next = laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    // A commit, the root tree, src/, and the new file: nothing of the big file again.
    assert_eq!(next.objects, 4, "{next:?}");
    assert_eq!(
        laptop.resolve("server/main").unwrap(),
        server.branch("main").unwrap()
    );
}

#[test]
fn a_pull_cut_off_part_way_changes_no_reference_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let server = source(&dir, "server");
    let mut laptop = Space::init(dir.path().join("laptop"), &author("laptop")).unwrap();

    let mut flaky = Flaky {
        inner: Direct(&server),
        remaining: 2,
    };
    let failed = laptop.pull(&mut flaky, "server", "main");
    assert!(matches!(failed, Err(Error::Remote(_))), "{failed:?}");
    assert_eq!(laptop.resolve("server/main").unwrap(), None);

    // The store stayed closed: what is present is complete, so resuming fetches the rest.
    let pulled = laptop.pull(&mut Direct(&server), "server", "main").unwrap();
    assert!(pulled.objects > 0);
    assert_eq!(
        exported(&laptop, "server/main", &dir.path().join("a")),
        exported(&server, "main", &dir.path().join("b"))
    );
}

#[test]
fn tampered_or_missing_objects_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = source(&dir, "server");

    let mut laptop = Space::init(dir.path().join("laptop"), &author("laptop")).unwrap();
    let mut flipped = Tampering {
        inner: Direct(&server),
        edit: |objects| {
            if let Some(object) = objects.last_mut() {
                object.bytes.push(0);
            }
        },
    };
    let refused = laptop.pull(&mut flipped, "server", "main");
    assert!(
        matches!(
            refused,
            Err(Error::RemoteObject {
                problem: "does not match its identity",
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(laptop.resolve("server/main").unwrap(), None);

    let mut dropped = Tampering {
        inner: Direct(&server),
        edit: |objects| {
            objects.pop();
        },
    };
    let refused = laptop.pull(&mut dropped, "server", "main");
    assert!(
        matches!(
            refused,
            Err(Error::RemoteObject {
                problem: "is missing on the remote",
                ..
            })
        ),
        "{refused:?}"
    );
}

#[test]
fn a_missing_remote_branch_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let server = source(&dir, "server");
    let mut laptop = Space::init(dir.path().join("laptop"), &author("laptop")).unwrap();
    assert!(matches!(
        laptop.pull(&mut Direct(&server), "server", "nope"),
        Err(Error::NoSuchBranch(_))
    ));
}

#[test]
fn object_batches_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let server = source(&dir, "server");
    let head = server.branch("main").unwrap().unwrap();
    let object = server.raw_object(head).unwrap().unwrap();
    let encoded = peerfs_core::encode_objects(std::slice::from_ref(&object));
    assert_eq!(peerfs_core::decode_objects(&encoded).unwrap(), vec![object]);
}
