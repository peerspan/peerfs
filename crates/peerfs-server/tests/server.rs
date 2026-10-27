//! The local API end to end: a real server on a Unix socket, driven through the client.

use std::time::Duration;

use peerfs_api::{
    Caller, ChangeInfo, Client, CommitResponse, EntryKind, MergeResponse, Method, Revision,
    SnapshotResponse,
};
use peerfs_core::{Author, Space};
use peerfs_server::{Options, serve, socket_path};

struct Running {
    _dir: tempfile::TempDir,
    space_dir: std::path::PathBuf,
    client: Client,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn start(idle_timeout: Option<Duration>) -> Running {
    start_on("server-host", idle_timeout).await
}

async fn start_on(host: &str, idle_timeout: Option<Duration>) -> Running {
    let dir = tempfile::tempdir().expect("temporary directory");
    let space_dir = dir.path().join("s");
    let author = Author {
        host: host.to_owned(),
        ..Author::default()
    };
    let space = Space::init(&space_dir, &author).expect("init");
    let socket = socket_path(&space_dir);
    let options = Options {
        host: host.to_owned(),
        idle_timeout,
    };
    let server = tokio::spawn(serve(space, options));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Running {
        _dir: dir,
        space_dir,
        client: Client::new(socket),
        server,
    }
}

/// Has each server trust the other's key for the other's host, through the API.
async fn introduce(a: &Running, b: &Running) {
    for (from, to) in [(a, b), (b, a)] {
        let space = from.client.space().await.expect("space info");
        let key = space.key.expect("the space signs");
        to.client
            .add_host(&space.host, &key)
            .await
            .expect("add host");
    }
}

fn caller(agent: &str) -> Caller {
    Caller {
        agent: Some(agent.to_owned()),
        session: None,
    }
}

#[tokio::test]
async fn a_workspace_round_trip_over_the_socket() {
    let running = start(None).await;
    let client = &running.client;

    let info = client.space().await.unwrap();
    assert_eq!(info.host, "server-host");

    let workspace = client.create_workspace("w", "main").await.unwrap();
    assert_eq!(workspace.branch, "main");
    client.write("w", "src/a b.txt", "hello\n").await.unwrap();
    assert_eq!(
        &client.read("w", "src/a b.txt").await.unwrap()[..],
        b"hello\n"
    );
    let listing = client.list("w", "src").await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "a b.txt");
    assert_eq!(listing[0].kind, EntryKind::File);
    assert_eq!(
        client.changes("w").await.unwrap(),
        vec![ChangeInfo::Added {
            path: "src/a b.txt".to_owned()
        }]
    );

    let CommitResponse::Committed { commit, merged } = client
        .commit("w", "add a file", &caller("agent-1"))
        .await
        .unwrap()
    else {
        panic!("expected a commit");
    };
    assert!(!merged);
    let log = client.log("main", Some(1)).await.unwrap();
    assert_eq!(log[0].id, commit);
    assert_eq!(log[0].host, "server-host");
    assert_eq!(log[0].agent.as_deref(), Some("agent-1"));

    let target = running.space_dir.parent().unwrap().join("out");
    client
        .export(
            target.to_str().unwrap(),
            Revision::Branch("main".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(target.join("src/a b.txt")).unwrap(),
        b"hello\n"
    );
}

#[tokio::test]
async fn concurrent_agents_all_commit_through_one_server() {
    let running = start(None).await;
    let client = &running.client;
    let agents = 8;

    // A shared file with one line per agent, separated by unchanged lines: in a three-way
    // line merge, as in git, edits to adjacent lines overlap and conflict.
    let lines: Vec<String> = (0..agents)
        .map(|i| format!("line {i}\nunchanged\nunchanged\n"))
        .collect();
    client.create_workspace("setup", "main").await.unwrap();
    client
        .write("setup", "shared.txt", lines.concat())
        .await
        .unwrap();
    client
        .commit("setup", "setup", &caller("setup"))
        .await
        .unwrap();

    // Every workspace starts at the setup commit. Each agent writes the whole file from
    // `lines`, so a workspace created after another agent committed would revert that edit.
    for i in 0..agents {
        client
            .create_workspace(&format!("agent-{i}"), "main")
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for i in 0..agents {
        let client = client.clone();
        let mut mine = lines.clone();
        mine[i] = format!("line {i} edited by agent {i}\nunchanged\nunchanged\n");
        tasks.push(tokio::spawn(async move {
            let name = format!("agent-{i}");
            client
                .write(&name, &format!("agents/{i}.txt"), format!("from {i}\n"))
                .await
                .unwrap();
            client
                .write(&name, "shared.txt", mine.concat())
                .await
                .unwrap();
            client.commit(&name, "work", &caller(&name)).await.unwrap()
        }));
    }
    for task in tasks {
        let response = task.await.unwrap();
        assert!(
            matches!(response, CommitResponse::Committed { .. }),
            "{response:?}"
        );
    }

    client.create_workspace("check", "main").await.unwrap();
    let shared =
        String::from_utf8(client.read("check", "shared.txt").await.unwrap().to_vec()).unwrap();
    for i in 0..agents {
        assert!(
            shared.contains(&format!("line {i} edited by agent {i}")),
            "{shared}"
        );
        assert_eq!(
            &client
                .read("check", &format!("agents/{i}.txt"))
                .await
                .unwrap()[..],
            format!("from {i}\n").as_bytes()
        );
    }
    assert_eq!(client.log("main", None).await.unwrap().len(), agents + 2);
}

#[tokio::test]
async fn a_conflict_comes_back_as_an_outcome() {
    let running = start(None).await;
    let client = &running.client;
    client.create_workspace("a", "main").await.unwrap();
    client.create_workspace("b", "main").await.unwrap();
    client.write("a", "f.txt", "from a\n").await.unwrap();
    client.write("b", "f.txt", "from b\n").await.unwrap();
    client.commit("a", "a", &caller("a")).await.unwrap();
    let CommitResponse::Conflicted { conflicts } =
        client.commit("b", "b", &caller("b")).await.unwrap()
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "f.txt");
    assert_eq!(conflicts[0].kind, "add_add");
}

#[tokio::test]
async fn errors_use_the_admin_socket_codes() {
    let running = start(None).await;
    let client = &running.client;

    let missing = client.read("nope", "a").await.unwrap_err();
    assert_eq!(missing.code(), Some("not_found"));

    client.create_workspace("w", "main").await.unwrap();
    let duplicate = client.create_workspace("w", "main").await.unwrap_err();
    assert_eq!(duplicate.code(), Some("conflict"));

    let escape = client
        .write("w", "a/../../etc/passwd", "x")
        .await
        .unwrap_err();
    assert_eq!(escape.code(), Some("invalid"));

    let relative = client
        .import("relative/dir", "main", "m", &Caller::default())
        .await
        .unwrap_err();
    assert_eq!(relative.code(), Some("invalid"));

    let unknown_field = client
        .send(
            Method::POST,
            "/v1/workspaces",
            Some("application/json"),
            r#"{"name":"x","branch":"main","extra":1}"#.into(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_field.status, 400);
    assert!(String::from_utf8_lossy(&unknown_field.body).contains(r#""code":"invalid""#));

    let no_route = client
        .send(Method::GET, "/v1/nothing", None, "".into())
        .await
        .unwrap();
    assert_eq!(no_route.status, 404);
}

#[tokio::test]
async fn an_idle_server_stops_and_removes_its_socket() {
    let running = start(Some(Duration::from_millis(200))).await;
    running.client.space().await.unwrap();
    let served = tokio::time::timeout(Duration::from_secs(10), running.server)
        .await
        .expect("the server stops when idle")
        .unwrap();
    served.unwrap();
    assert!(!socket_path(&running.space_dir).exists());
    assert!(!peerfs_server::pid_path(&running.space_dir).exists());
    // The space is free for the next process.
    Space::open(&running.space_dir).unwrap();
}

async fn commit_files(client: &Client, workspace: &str, files: &[(&str, &str)]) {
    if client.workspace(workspace).await.is_err() {
        client
            .create_workspace(workspace, "main")
            .await
            .expect("create workspace");
    }
    for (path, text) in files {
        client
            .write(workspace, path, text.to_string())
            .await
            .expect("write");
    }
    let response = client
        .commit(workspace, "files", &caller("setup"))
        .await
        .expect("commit");
    assert!(
        matches!(response, CommitResponse::Committed { .. }),
        "{response:?}"
    );
}

#[tokio::test]
async fn a_server_pulls_another_servers_branch_and_works_from_it() {
    let server = start_on("server", None).await;
    let laptop = start_on("laptop", None).await;
    introduce(&server, &laptop).await;
    commit_files(
        &server.client,
        "w",
        &[("a.txt", "a\n"), ("src/b.rs", "fn b() {}\n")],
    )
    .await;

    let from = server.client.socket().to_str().unwrap().to_owned();
    let pulled = laptop.client.pull(&from, "main").await.unwrap();
    assert_eq!(pulled.remote, "server");
    assert!(pulled.objects > 0);
    let remotes = laptop.client.remotes().await.unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].head, pulled.head);

    // Work starts from a fork of the tracking reference.
    laptop.client.fork("work", "server/main").await.unwrap();
    laptop
        .client
        .create_workspace("mine", "work")
        .await
        .unwrap();
    assert_eq!(
        &laptop.client.read("mine", "src/b.rs").await.unwrap()[..],
        b"fn b() {}\n"
    );
    assert_eq!(
        laptop.client.log("server/main", None).await.unwrap()[0].id,
        pulled.head
    );

    let again = laptop.client.pull(&from, "main").await.unwrap();
    assert_eq!(again.objects, 0);
}

#[tokio::test]
async fn two_servers_pulling_from_each_other_at_once_both_finish() {
    let a = start_on("host-a", None).await;
    let b = start_on("host-b", None).await;
    introduce(&a, &b).await;
    let many: Vec<(String, String)> = (0..600)
        .map(|i| (format!("f/{i}.txt"), format!("file {i}\n")))
        .collect();
    let many: Vec<(&str, &str)> = many.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    commit_files(&a.client, "w", &many).await;
    commit_files(&b.client, "w", &many[..300]).await;

    let from_a = a.client.socket().to_str().unwrap().to_owned();
    let from_b = b.client.socket().to_str().unwrap().to_owned();
    let both = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            b.client.pull(&from_a, "main"),
            a.client.pull(&from_b, "main")
        )
    })
    .await
    .expect("neither pull waits on the other forever");
    both.0.unwrap();
    both.1.unwrap();
}

#[tokio::test]
async fn pull_and_fetch_reject_bad_requests() {
    let server = start_on("server", None).await;
    let own = server.client.socket().to_str().unwrap().to_owned();
    let itself = server.client.pull(&own, "main").await.unwrap_err();
    assert_eq!(itself.code(), Some("invalid"));

    let nowhere = server
        .client
        .pull("/nonexistent/peerfs.sock", "main")
        .await
        .unwrap_err();
    assert_eq!(nowhere.code(), Some("remote"));

    let too_many: Vec<String> = (0..300).map(|_| "00".repeat(33)).collect();
    let refused = server.client.fetch_objects(&too_many).await.unwrap_err();
    assert_eq!(refused.code(), Some("invalid"));
}

#[tokio::test]
async fn push_is_refused_when_the_home_moved_then_pull_merge_and_push_again() {
    let server = start_on("server", None).await;
    let laptop = start_on("laptop", None).await;
    introduce(&server, &laptop).await;
    commit_files(
        &server.client,
        "w",
        &[("shared.txt", "one\ntwo\nthree\nfour\nfive\n")],
    )
    .await;
    let server_socket = server.client.socket().to_str().unwrap().to_owned();

    laptop.client.pull(&server_socket, "main").await.unwrap();
    laptop.client.fork("work", "server/main").await.unwrap();
    laptop.client.create_workspace("lw", "work").await.unwrap();
    laptop
        .client
        .write("lw", "laptop.txt", "from the laptop\n")
        .await
        .unwrap();
    laptop
        .client
        .commit("lw", "laptop", &caller("laptop"))
        .await
        .unwrap();

    let pushed = laptop
        .client
        .push(&server_socket, "main", "work")
        .await
        .unwrap();
    assert_eq!(pushed.remote, "server");
    assert_eq!(pushed.objects, 3);
    assert_eq!(server.client.branches().await.unwrap()[0].head, pushed.head);
    assert_eq!(laptop.client.remotes().await.unwrap()[0].head, pushed.head);

    // The server moves on; the laptop's next push is refused rather than dropping it.
    commit_files(
        &server.client,
        "w",
        &[("shared.txt", "ONE\ntwo\nthree\nfour\nfive\n")],
    )
    .await;
    laptop
        .client
        .write("lw", "shared.txt", "one\ntwo\nthree\nfour\nFIVE\n")
        .await
        .unwrap();
    laptop
        .client
        .commit("lw", "laptop", &caller("laptop"))
        .await
        .unwrap();
    let refused = laptop
        .client
        .push(&server_socket, "main", "work")
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some("conflict"), "{refused}");
    assert!(
        refused.to_string().contains("pull server/main"),
        "{refused}"
    );

    // Pull, merge into the laptop's branch, push again.
    laptop.client.pull(&server_socket, "main").await.unwrap();
    let merged = laptop
        .client
        .merge("lw", "server/main", &caller("laptop"))
        .await
        .unwrap();
    assert!(matches!(merged, MergeResponse::Merged { .. }), "{merged:?}");
    let pushed = laptop
        .client
        .push(&server_socket, "main", "work")
        .await
        .unwrap();
    server
        .client
        .create_workspace("check", "main")
        .await
        .unwrap();
    assert_eq!(
        &server.client.read("check", "shared.txt").await.unwrap()[..],
        b"ONE\ntwo\nthree\nfour\nFIVE\n"
    );
    let log = server.client.log("main", Some(1)).await.unwrap();
    assert_eq!(log[0].id, pushed.head);
    assert_eq!(log[0].parents.len(), 2, "a merge commit");
}

#[tokio::test]
async fn two_hosts_pushing_to_one_home_at_once_cannot_both_win() {
    let server = start_on("server", None).await;
    let first = start_on("first", None).await;
    let second = start_on("second", None).await;
    introduce(&server, &first).await;
    introduce(&server, &second).await;
    commit_files(&server.client, "w", &[("base.txt", "base\n")]).await;
    let server_socket = server.client.socket().to_str().unwrap().to_owned();
    for (laptop, file) in [(&first, "first.txt"), (&second, "second.txt")] {
        laptop.client.pull(&server_socket, "main").await.unwrap();
        laptop.client.fork("work", "server/main").await.unwrap();
        laptop.client.create_workspace("lw", "work").await.unwrap();
        laptop.client.write("lw", file, "x\n").await.unwrap();
        laptop
            .client
            .commit("lw", "work", &caller(file))
            .await
            .unwrap();
    }
    let (a, b) = tokio::join!(
        first.client.push(&server_socket, "main", "work"),
        second.client.push(&server_socket, "main", "work")
    );
    let outcomes = [a.is_ok(), b.is_ok()];
    assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 1, "{a:?} {b:?}");
    let loser = if a.is_ok() { b } else { a };
    assert_eq!(loser.unwrap_err().code(), Some("conflict"));
}

#[tokio::test]
async fn commits_from_an_untrusted_host_are_forbidden_until_its_key_is_added() {
    let server = start_on("server", None).await;
    let laptop = start_on("laptop", None).await;
    commit_files(&server.client, "w", &[("a.txt", "a\n")]).await;
    let server_socket = server.client.socket().to_str().unwrap().to_owned();
    let server_info = server.client.space().await.unwrap();
    let server_key = server_info.key.expect("the space signs");
    assert!(laptop.client.hosts().await.unwrap().is_empty());

    let refused = laptop
        .client
        .pull(&server_socket, "main")
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some("forbidden"), "{refused}");
    assert!(refused.to_string().contains(&server_key), "{refused}");
    assert!(laptop.client.remotes().await.unwrap().is_empty());

    let bad = laptop
        .client
        .add_host("server", "not a key")
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Some("invalid"));
    let spaced = laptop
        .client
        .add_host("two words", &server_key)
        .await
        .unwrap_err();
    assert_eq!(spaced.code(), Some("invalid"));
    laptop.client.add_host("server", &server_key).await.unwrap();
    let hosts = laptop.client.hosts().await.unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(
        (hosts[0].host.as_str(), hosts[0].key.as_str()),
        ("server", server_key.as_str())
    );

    let pulled = laptop.client.pull(&server_socket, "main").await.unwrap();
    let log = laptop.client.log("server/main", None).await.unwrap();
    assert_eq!(log[0].id, pulled.head);
    assert!(
        log.iter()
            .all(|commit| commit.signed_by.as_deref() == Some(server_key.as_str()))
    );

    // The server has not added the laptop's key, so it refuses the laptop's push.
    laptop.client.fork("work", "server/main").await.unwrap();
    laptop.client.create_workspace("lw", "work").await.unwrap();
    commit_files(&laptop.client, "lw", &[("b.txt", "b\n")]).await;
    let refused = laptop
        .client
        .push(&server_socket, "main", "work")
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("not a known key of host laptop"),
        "{refused}"
    );
}

#[tokio::test]
async fn snapshots_over_the_api_are_replaced_by_one_signed_commit() {
    let running = start(None).await;
    let client = &running.client;
    client.create_workspace("w", "main").await.unwrap();
    assert_eq!(
        client.snapshot("w", None, &caller("a")).await.unwrap(),
        SnapshotResponse::Unchanged
    );
    let mut taken = Vec::new();
    for step in ["one", "two"] {
        client
            .write("w", "a.txt", format!("{step}\n"))
            .await
            .unwrap();
        let SnapshotResponse::Snapshot { snapshot } = client
            .snapshot("w", Some(&format!("Edit {step}")), &caller("a"))
            .await
            .unwrap()
        else {
            panic!("expected a snapshot");
        };
        taken.push(snapshot);
    }
    let listed = client.snapshots("w").await.unwrap();
    let ids: Vec<_> = listed.iter().map(|s| s.id.clone()).collect();
    assert_eq!(ids, vec![taken[1].clone(), taken[0].clone()]);
    assert_eq!(listed[0].message, "Edit two");
    assert_eq!(listed[0].agent.as_deref(), Some("a"));
    assert!(listed.iter().all(|s| s.signed_by.is_none()));
    assert_eq!(
        client.workspace("w").await.unwrap().snapshot,
        Some(taken[1].clone())
    );

    let CommitResponse::Committed { commit, .. } =
        client.commit("w", "Task", &caller("a")).await.unwrap()
    else {
        panic!("expected a commit");
    };
    let log = client.log("main", Some(1)).await.unwrap();
    assert_eq!(log[0].id, commit);
    assert_eq!(log[0].predecessor, Some(taken[1].clone()));
    assert!(log[0].signed_by.is_some());
    assert!(client.snapshots("w").await.unwrap().is_empty());
    assert_eq!(client.workspace("w").await.unwrap().snapshot, None);

    let unknown_field = client
        .send(
            Method::POST,
            "/v1/workspaces/w/snapshot",
            Some("application/json"),
            r#"{"label":"x"}"#.into(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_field.status, 400);
}
