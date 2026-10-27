//! The local API end to end: a real server on a Unix socket, driven through the client.

use std::time::Duration;

use peerfs_api::{Caller, ChangeInfo, Client, CommitResponse, EntryKind, Method, Revision};
use peerfs_core::{Author, Space};
use peerfs_server::{Options, serve, socket_path};

struct Running {
    _dir: tempfile::TempDir,
    space_dir: std::path::PathBuf,
    client: Client,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn start(idle_timeout: Option<Duration>) -> Running {
    let dir = tempfile::tempdir().expect("temporary directory");
    let space_dir = dir.path().join("s");
    let author = Author {
        host: "server-host".to_owned(),
        ..Author::default()
    };
    let space = Space::init(&space_dir, &author).expect("init");
    let socket = socket_path(&space_dir);
    let options = Options {
        host: "server-host".to_owned(),
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
        vec![ChangeInfo::Written {
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

    let mut tasks = Vec::new();
    for i in 0..agents {
        let client = client.clone();
        let mut mine = lines.clone();
        mine[i] = format!("line {i} edited by agent {i}\nunchanged\nunchanged\n");
        tasks.push(tokio::spawn(async move {
            let name = format!("agent-{i}");
            client.create_workspace(&name, "main").await.unwrap();
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
