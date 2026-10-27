//! Garbage collection with snapshot retention: what is kept, what goes, and when.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use peerfs_core::{Author, CommitOutcome, GcOptions, GcReport, ObjectId, Space};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn author() -> Author {
    Author {
        host: "laptop".to_owned(),
        agent: Some("agent".to_owned()),
        session: None,
    }
}

fn options(days_later: u32) -> GcOptions {
    GcOptions {
        now: SystemTime::now() + DAY * days_later,
        ..GcOptions::default()
    }
}

fn gc(space: &mut Space, days_later: u32) -> GcReport {
    space
        .collect_garbage(&options(days_later))
        .expect("test setup")
}

fn dry_run(space: &mut Space, days_later: u32) -> GcReport {
    let options = GcOptions {
        dry_run: true,
        ..options(days_later)
    };
    space.collect_garbage(&options).expect("test setup")
}

fn packs(space: &Path) -> Vec<(String, u64)> {
    let mut packs: Vec<_> = fs::read_dir(space.join("objects"))
        .expect("test setup")
        .map(|entry| {
            let entry = entry.expect("test setup");
            let len = entry.metadata().expect("test setup").len();
            (entry.file_name().into_string().expect("test setup"), len)
        })
        .collect();
    packs.sort();
    packs
}

struct History {
    /// The committed change's snapshots, which its commit names as predecessor.
    committed: Vec<ObjectId>,
    /// Snapshots of a workspace since discarded.
    discarded: Vec<ObjectId>,
    /// The open workspace's current snapshots.
    current: Vec<ObjectId>,
}

/// A space with a committed change that had snapshots, a discarded workspace's snapshots,
/// and an open workspace with snapshots of its own.
fn history(space: &mut Space) -> History {
    let snap = |space: &mut Space, ws: &mut peerfs_core::Workspace, path: &str, text: &str| {
        ws.write(space, path, text.as_bytes()).expect("test setup");
        space
            .snapshot_workspace(ws, path, &author())
            .expect("test setup")
            .expect("test setup")
    };
    let mut task = space.create_workspace("task", "main").expect("test setup");
    let committed = vec![
        snap(space, &mut task, "a.txt", "draft\n"),
        snap(space, &mut task, "a.txt", "final\n"),
    ];
    let CommitOutcome::Committed { .. } = space
        .commit_workspace(&mut task, "Task", &author())
        .expect("test setup")
    else {
        panic!("expected a commit");
    };

    let mut scratch = space
        .create_workspace("scratch", "main")
        .expect("test setup");
    let discarded = vec![
        snap(space, &mut scratch, "big.bin", &"x".repeat(200_000)),
        snap(space, &mut scratch, "tmp.txt", "gone soon\n"),
    ];
    drop(scratch);
    space.discard_workspace("scratch").expect("test setup");

    let mut open = space.workspace("task").expect("test setup");
    let current = vec![snap(space, &mut open, "b.txt", "in progress\n")];
    History {
        committed,
        discarded,
        current,
    }
}

#[test]
fn snapshots_are_kept_like_a_reflog_and_the_rest_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("space");
    let mut space = Space::init(&path, &author()).unwrap();
    let history = history(&mut space);
    let all = history.committed.len() + history.discarded.len() + history.current.len();

    // Within the grace period nothing is touched.
    let report = gc(&mut space, 0);
    assert_eq!(report.objects_dropped, 0);
    assert_eq!(report.packs_rewritten, 0);
    assert_eq!(report.snapshots_kept, all);

    // A dry run says what would go and changes nothing.
    let before = packs(&path);
    let report = dry_run(&mut space, 40);
    assert_eq!(packs(&path), before);
    assert_eq!(report.snapshots_dropped, history.discarded.len());
    assert!(report.bytes_freed > 0);

    // After 40 days: the discarded workspace's snapshots go, with the content only they
    // held; the committed change's stay for 90 days; the open workspace's always stay.
    let report = gc(&mut space, 40);
    assert_eq!(report.snapshots_dropped, history.discarded.len());
    assert_eq!(report.packs_rewritten, 1);
    assert!(report.objects_dropped >= 3, "{report:?}");
    for id in &history.discarded {
        assert!(space.commit(*id).is_err(), "dropped");
    }
    for id in history.committed.iter().chain(&history.current) {
        space.commit(*id).unwrap();
    }
    assert!(
        packs(&path).iter().map(|(_, len)| len).sum::<u64>()
            < before.iter().map(|(_, len)| len).sum::<u64>()
    );

    // After 100 days the committed change's snapshots go too.
    let report = gc(&mut space, 100);
    assert_eq!(report.snapshots_dropped, history.committed.len());
    for id in &history.committed {
        assert!(space.commit(*id).is_err(), "dropped");
    }

    // What is still in use reads back, here and after reopening.
    for _ in 0..2 {
        let ws = space.workspace("task").unwrap();
        assert_eq!(ws.read(&space, "a.txt").unwrap(), b"final\n");
        assert_eq!(ws.read(&space, "b.txt").unwrap(), b"in progress\n");
        let snapshots = space.snapshots(&ws).unwrap();
        let ids: Vec<_> = snapshots.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, history.current);
        let head = space.branch("main").unwrap().unwrap();
        let commit = space.commit(head).unwrap();
        assert_eq!(commit.message, "Task");
        assert!(
            commit.predecessor.is_some(),
            "the weak link outlives its target"
        );
        drop(ws);
        drop(space);
        space = Space::open(&path).unwrap();
    }

    // Collecting again finds nothing more.
    let report = gc(&mut space, 100);
    assert_eq!((report.objects_dropped, report.packs_rewritten), (0, 0));
    assert_eq!(report.snapshots_kept, history.current.len());
}

#[test]
fn objects_written_since_the_grace_cutoff_wait() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("space");
    let mut space = Space::init(&path, &author()).unwrap();
    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.write(&space, "a.txt", b"one\n").unwrap();
    space.snapshot_workspace(&mut ws, "one", &author()).unwrap();
    drop(ws);
    space.discard_workspace("w").unwrap();

    // Thirty-one days on, but with a grace period long enough to cover the pack.
    let options = GcOptions {
        now: SystemTime::now() + DAY * 31,
        grace: DAY * 40,
        ..GcOptions::default()
    };
    let report = space.collect_garbage(&options).unwrap();
    assert_eq!(report.objects_dropped, 0);
    assert!(report.objects_waiting >= 1, "{report:?}");
    assert_eq!(report.snapshots_dropped, 0);
}
