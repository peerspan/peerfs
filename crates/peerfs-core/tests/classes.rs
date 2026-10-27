//! Path classes: derived and secret paths stay in the workspace and out of commits.

use std::fs;

use peerfs_core::{
    Author, Change, Class, CommitOutcome, Entry, MergeOutcome, Space, TreePath, Workspace,
};

fn author() -> Author {
    Author {
        host: "laptop".to_owned(),
        ..Author::default()
    }
}

fn path(text: &str) -> TreePath {
    TreePath::try_from(text).expect("valid path")
}

fn committed(space: &Space, revision: &str, text: &str) -> Option<Entry> {
    let head = space.resolve(revision).expect("resolve").expect("exists");
    let tree = space.commit(head).expect("commit").tree;
    space.entry(tree, &path(text)).expect("entry")
}

fn commit(space: &mut Space, ws: &mut Workspace) -> CommitOutcome {
    space
        .commit_workspace(ws, "work", &author())
        .expect("commit")
}

#[test]
fn derived_and_secret_paths_stay_in_the_workspace_and_out_of_commits() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &author()).unwrap();
    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.write(&space, "src/main.rs", b"fn main() {}\n").unwrap();
    ws.write(&space, "target/debug/app", b"binary").unwrap();
    ws.write(&space, "web/node_modules/x/index.js", b"x")
        .unwrap();
    ws.write(&space, ".env", b"TOKEN=abc\n").unwrap();
    ws.write(&space, ".env.example", b"TOKEN=\n").unwrap();

    assert_eq!(
        ws.changes(&space).unwrap(),
        vec![
            Change::Added(path(".env.example")),
            Change::Added(path("src/main.rs")),
        ]
    );
    assert_eq!(
        ws.kept_local(&space).unwrap(),
        vec![
            (path(".env"), Class::Secret),
            (path("target"), Class::Derived),
            (path("web/node_modules"), Class::Derived),
        ]
    );
    let snapshot = space
        .snapshot_workspace(&mut ws, "call", &author())
        .unwrap()
        .unwrap();
    let snapshot_tree = space.commit(snapshot).unwrap().tree;
    assert_eq!(space.entry(snapshot_tree, &path(".env")).unwrap(), None);

    assert!(matches!(
        commit(&mut space, &mut ws),
        CommitOutcome::Committed { .. }
    ));
    assert!(committed(&space, "main", "src/main.rs").is_some());
    assert!(committed(&space, "main", ".env.example").is_some());
    for local in [".env", "target", "web/node_modules", "web"] {
        assert_eq!(committed(&space, "main", local), None, "{local}");
    }
    // The commit kept them in the workspace.
    assert_eq!(ws.read(&space, "target/debug/app").unwrap(), b"binary");
    assert_eq!(ws.read(&space, ".env").unwrap(), b"TOKEN=abc\n");
    assert!(ws.changes(&space).unwrap().is_empty());

    // A change to derived or secret paths alone commits nothing.
    ws.write(&space, "target/debug/app", b"rebuilt").unwrap();
    ws.write(&space, ".env", b"TOKEN=rotated\n").unwrap();
    assert_eq!(commit(&mut space, &mut ws), CommitOutcome::Unchanged);
    assert_eq!(ws.read(&space, ".env").unwrap(), b"TOKEN=rotated\n");
}

#[test]
fn gitignore_adds_derived_paths_and_the_owner_can_name_source() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &author()).unwrap();
    fs::write(
        dir.path().join("space/classes"),
        "[source]\nbuild/\n[secret]\n*.token\n",
    )
    .unwrap();
    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.write(&space, ".gitignore", b"*.log\ngen/\n").unwrap();
    ws.write(&space, "app.log", b"log").unwrap();
    ws.write(&space, "gen/out.rs", b"generated").unwrap();
    ws.write(&space, "build/script.sh", b"echo build").unwrap();
    ws.write(&space, "ci/deploy.token", b"secret").unwrap();
    commit(&mut space, &mut ws);

    assert!(committed(&space, "main", ".gitignore").is_some());
    assert!(
        committed(&space, "main", "build/script.sh").is_some(),
        "the owner made build/ source"
    );
    for local in ["app.log", "gen", "ci"] {
        assert_eq!(committed(&space, "main", local), None, "{local}");
    }
    assert_eq!(
        ws.kept_local(&space).unwrap(),
        vec![
            (path("app.log"), Class::Derived),
            (path("ci/deploy.token"), Class::Secret),
            (path("gen"), Class::Derived),
        ]
    );
}

#[test]
fn local_paths_survive_conflicts_and_do_not_block_merges() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &author()).unwrap();
    let mut ws = space.create_workspace("w", "main").unwrap();
    let mut other = space.create_workspace("other", "main").unwrap();
    other.write(&space, "a.txt", b"theirs\n").unwrap();
    commit(&mut space, &mut other);

    ws.write(&space, "a.txt", b"ours\n").unwrap();
    ws.write(&space, "target/app", b"built").unwrap();
    assert!(matches!(
        commit(&mut space, &mut ws),
        CommitOutcome::Conflicted(_)
    ));
    assert_eq!(ws.read(&space, "target/app").unwrap(), b"built");
    ws.write(&space, "a.txt", b"both\n").unwrap();
    assert!(matches!(
        commit(&mut space, &mut ws),
        CommitOutcome::Committed { .. }
    ));
    assert_eq!(ws.read(&space, "target/app").unwrap(), b"built");

    // With only derived files in the overlay, the workspace is clean enough to merge.
    space
        .create_branch("side", space.branch("main").unwrap().unwrap())
        .unwrap();
    let mut side = space.create_workspace("side", "side").unwrap();
    side.write(&space, "b.txt", b"side\n").unwrap();
    commit(&mut space, &mut side);
    assert!(matches!(
        space
            .merge_into_workspace(&mut ws, "side", &author())
            .unwrap(),
        MergeOutcome::FastForward { .. }
    ));
    assert_eq!(ws.read(&space, "target/app").unwrap(), b"built");
}

#[test]
fn build_output_committed_before_classes_can_still_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut space = Space::init(dir.path().join("space"), &author()).unwrap();
    // A commit made by import, which takes a directory whole.
    let source = dir.path().join("old");
    fs::create_dir_all(source.join("target")).unwrap();
    fs::write(source.join("target/stale"), "old build").unwrap();
    fs::write(source.join("main.rs"), "fn main() {}\n").unwrap();
    let tree = space.import(&source).unwrap();
    space
        .commit_tree("main", tree, "import", &author())
        .unwrap();
    assert!(committed(&space, "main", "target/stale").is_some());

    let mut ws = space.create_workspace("w", "main").unwrap();
    ws.remove(&space, "target").unwrap();
    ws.write(&space, "target/fresh", b"new build").unwrap();
    assert!(matches!(
        commit(&mut space, &mut ws),
        CommitOutcome::Committed { .. }
    ));
    assert_eq!(committed(&space, "main", "target"), None);
    assert_eq!(ws.read(&space, "target/fresh").unwrap(), b"new build");
}
