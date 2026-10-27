//! The peerfs local API server: one space, served over a Unix socket.
//!
//! Requests run one at a time against the space, so agents working concurrently serialize
//! here instead of contending for the space's lock. The routes and bodies are those of
//! [`peerfs_api`].

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use peerfs_api::{
    AdvanceRequest, BranchInfo, Caller, ChangeInfo, Client, ClientError, CommitInfo, CommitRequest,
    CommitResponse, ConflictInfo, CreateWorkspaceRequest, DirEntry, EntryKind, ErrorBody,
    ExportRequest, FetchRequest, ForkRequest, HaveRequest, HaveResponse, HostKey, ImportRequest,
    MergeRequest, MergeResponse, MountInfo, PullRequest, PullResponse, PushRequest, PushResponse,
    ReceiveResponse, Revision, SOCKET_NAME, SnapshotRequest, SnapshotResponse, SpaceInfo,
    TrackingInfo, WorkspaceInfo,
};
use peerfs_core::{
    Author, Change, Commit, CommitOutcome, Conflict, ConflictKind, Error, FETCH_BATCH,
    MergeOutcome, Merged, ObjectId, PublicKey, Space, Workspace, decode_objects, encode_objects,
};
use peerfs_mount::Mount;
use serde::Deserialize;
use tokio::net::UnixListener;
use tokio::sync::Notify;

/// The largest JSON request body, as on the host daemon's `admin.sock`.
const JSON_LIMIT: usize = 1024 * 1024;

/// The largest batch of objects received in one request: [`FETCH_BATCH`] objects of at
/// most 256 KiB, with room for framing.
const OBJECTS_LIMIT: usize = 128 * 1024 * 1024;

/// The largest file written in one request.
const FILE_LIMIT: usize = 1024 * 1024 * 1024;

/// How often an idle server checks whether its timeout has passed.
const IDLE_CHECK: Duration = Duration::from_secs(5);

/// How a server runs.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The host recorded on commits made through this server.
    pub host: String,
    /// Shut down after this long without a request. `None` serves until stopped.
    pub idle_timeout: Option<Duration>,
}

#[derive(Debug)]
struct Shared {
    space: Arc<Mutex<Space>>,
    /// Mounted workspaces, by name.
    mounts: Mutex<BTreeMap<String, Mount>>,
    host: String,
    started: Instant,
    last_request_ms: AtomicU64,
}

type AppState = Arc<Shared>;

/// The socket path of the space in `space_dir`.
pub fn socket_path(space_dir: &Path) -> PathBuf {
    space_dir.join(SOCKET_NAME)
}

/// The file holding the process ID of the server of the space in `space_dir`.
pub fn pid_path(space_dir: &Path) -> PathBuf {
    space_dir.join("serve.pid")
}

/// The API's routes over `space`.
pub fn router(space: Space, host: String) -> Router {
    router_with(Arc::new(Shared {
        space: Arc::new(Mutex::new(space)),
        mounts: Mutex::new(BTreeMap::new()),
        host,
        started: Instant::now(),
        last_request_ms: AtomicU64::new(0),
    }))
}

fn router_with(state: AppState) -> Router {
    let files = get(read_file)
        .put(write_file)
        .delete(remove_file)
        .layer(DefaultBodyLimit::max(FILE_LIMIT));
    Router::new()
        .route("/v1/space", get(space_info))
        .route("/v1/branches", get(branches).post(fork))
        .route("/v1/branches/{name}/log", get(log))
        .route("/v1/import", post(import))
        .route("/v1/export", post(export))
        .route("/v1/workspaces", get(workspaces).post(create_workspace))
        .route(
            "/v1/workspaces/{name}",
            get(workspace_info_route).delete(discard_workspace),
        )
        .route("/v1/workspaces/{name}/files/{*path}", files)
        .route("/v1/workspaces/{name}/list", get(list))
        .route("/v1/workspaces/{name}/changes", get(changes))
        .route("/v1/workspaces/{name}/commit", post(commit))
        .route("/v1/workspaces/{name}/snapshot", post(snapshot))
        .route("/v1/workspaces/{name}/snapshots", get(snapshots))
        .route("/v1/objects/fetch", post(fetch_objects))
        .route("/v1/objects/have", post(have_objects))
        .route(
            "/v1/objects/receive",
            post(receive_objects).layer(DefaultBodyLimit::max(OBJECTS_LIMIT)),
        )
        .route("/v1/branches/{name}/advance", post(advance))
        .route("/v1/push", post(push))
        .route("/v1/workspaces/{name}/merge", post(merge))
        .route("/v1/pull", post(pull))
        .route("/v1/remotes", get(remotes))
        .route("/v1/hosts", get(hosts).post(add_host))
        .route(
            "/v1/workspaces/{name}/mount",
            post(mount_workspace).delete(unmount_workspace),
        )
        .route("/v1/mounts", get(mounts))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(JSON_LIMIT))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            note_request,
        ))
        .with_state(state)
}

/// Serves `space` at its socket until interrupted, terminated, or idle for
/// `options.idle_timeout`. Writes the process ID to `serve.pid` beside the socket and
/// removes both on the way out.
///
/// # Errors
///
/// Fails if the socket cannot be bound, for example because its path is too long.
pub async fn serve(space: Space, options: Options) -> io::Result<()> {
    let socket = socket_path(space.path());
    // Holding the space means no other server holds it, so a socket file left behind is
    // stale.
    match std::fs::remove_file(&socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let pid_file = pid_path(space.path());
    std::fs::write(&pid_file, format!("{}\n", std::process::id()))?;
    let state = Arc::new(Shared {
        space: Arc::new(Mutex::new(space)),
        mounts: Mutex::new(BTreeMap::new()),
        host: options.host,
        started: Instant::now(),
        last_request_ms: AtomicU64::new(0),
    });
    let stop = Arc::new(Notify::new());
    if let Some(timeout) = options.idle_timeout {
        tokio::spawn(watch_idle(Arc::clone(&state), timeout, Arc::clone(&stop)));
    }
    let served = axum::serve(listener, router_with(Arc::clone(&state)))
        .with_graceful_shutdown(shutdown(stop))
        .await;
    let mounts = std::mem::take(&mut *lock_mounts(&state));
    for (workspace, mount) in mounts {
        if let Err(e) = mount.unmount() {
            log_internal(&format!("unmounting workspace {workspace}: {e}"));
        }
    }
    let removed = std::fs::remove_file(&socket).and(std::fs::remove_file(&pid_file));
    served?;
    removed
}

async fn watch_idle(state: AppState, timeout: Duration, stop: Arc<Notify>) {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    loop {
        tokio::time::sleep(IDLE_CHECK.min(timeout)).await;
        let now = elapsed_ms(state.started);
        if !lock_mounts(&state).is_empty() {
            // A mount is served by this process, so it is in use however quiet the API is.
            state.last_request_ms.store(now, Ordering::Relaxed);
            continue;
        }
        if now.saturating_sub(state.last_request_ms.load(Ordering::Relaxed)) >= timeout_ms {
            stop.notify_one();
            return;
        }
    }
}

async fn shutdown(stop: Arc<Notify>) {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
        () = stop.notified() => {}
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn note_request(State(state): State<AppState>, request: Request, next: Next) -> Response {
    state
        .last_request_ms
        .store(elapsed_ms(state.started), Ordering::Relaxed);
    let response = next.run(request).await;
    // Count the end of a long request as activity too.
    state
        .last_request_ms
        .store(elapsed_ms(state.started), Ordering::Relaxed);
    response
}

/// An error response.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid",
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    /// Another host failed or sent something unusable.
    fn remote(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "remote",
            message: message.into(),
        }
    }

    fn internal(detail: &dyn std::fmt::Display) -> Self {
        log_internal(detail);
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "internal error".to_owned(),
        }
    }
}

#[expect(
    clippy::print_stderr,
    reason = "the server's log is its standard error"
)]
fn log_internal(detail: &dyn std::fmt::Display) {
    eprintln!("peerfs serve: internal error: {detail}");
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let message = error.to_string();
        let (status, code) = match &error {
            Error::NotFound(_)
            | Error::NoSuchBranch(_)
            | Error::NoSuchWorkspace(_)
            | Error::MissingObject { .. } => (StatusCode::NOT_FOUND, "not_found"),
            Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Error::BranchExists(_)
            | Error::WorkspaceExists(_)
            | Error::BranchMoved { .. }
            | Error::NotFastForward { .. }
            | Error::WorkspaceNotClean(_) => (StatusCode::CONFLICT, "conflict"),
            Error::InvalidName(_)
            | Error::InvalidPath(_)
            | Error::WrongEntry { .. }
            | Error::Unsupported { .. }
            | Error::InvalidHost(_) => (StatusCode::BAD_REQUEST, "invalid"),
            Error::UnknownHostKey { .. } => (StatusCode::FORBIDDEN, "forbidden"),
            Error::Remote(_) | Error::RemoteObject { .. } => (StatusCode::BAD_GATEWAY, "remote"),
            _ => return Self::internal(&error),
        };
        Self {
            status,
            code,
            message,
        }
    }
}

impl From<ClientError> for ApiError {
    fn from(error: ClientError) -> Self {
        Self::remote(error.to_string())
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::invalid(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::invalid(rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            code: self.code.to_owned(),
            error: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Runs `work` on the blocking pool with the space locked.
async fn with_space<T, F>(state: &AppState, work: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Space, &str) -> ApiResult<T> + Send + 'static,
{
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        let mut space = state.space.lock().expect("the space lock is not poisoned");
        work(&mut space, &state.host)
    })
    .await
    .map_err(|e| ApiError::internal(&e))?
}

fn author(host: &str, caller: Caller) -> Author {
    Author {
        host: host.to_owned(),
        agent: caller.agent,
        session: caller.session,
    }
}

/// The commit a branch, or a tracking reference `<remote>/<branch>`, points at.
fn head(space: &Space, revision: &str) -> ApiResult<ObjectId> {
    space
        .resolve(revision)?
        .ok_or_else(|| ApiError::not_found(format!("no branch {revision}")))
}

fn workspace_info(workspace: &Workspace) -> WorkspaceInfo {
    WorkspaceInfo {
        name: workspace.name().to_owned(),
        branch: workspace.branch().to_owned(),
        base: workspace.base().to_string(),
        snapshot: workspace.snapshot().map(|snapshot| snapshot.to_string()),
    }
}

fn commit_info(id: ObjectId, commit: Commit) -> CommitInfo {
    CommitInfo {
        id: id.to_string(),
        tree: commit.tree.to_string(),
        parents: commit.parents.iter().map(ToString::to_string).collect(),
        change: commit.change.to_string(),
        host: commit.author.host,
        agent: commit.author.agent,
        session: commit.author.session,
        time_ms: commit.time.duration_since(UNIX_EPOCH).map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        }),
        message: commit.message,
        predecessor: commit
            .predecessor
            .map(|predecessor| predecessor.to_string()),
        signed_by: commit.signature.map(|signature| signature.key.to_string()),
    }
}

fn absolute(path: &str) -> ApiResult<&Path> {
    let path = Path::new(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ApiError::invalid(format!(
            "{} is not an absolute path on the server's host",
            path.display()
        )))
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such endpoint")
}

async fn method_not_allowed() -> ApiError {
    ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        code: "invalid",
        message: "method not allowed".to_owned(),
    }
}

async fn space_info(State(state): State<AppState>) -> ApiResult<Json<SpaceInfo>> {
    with_space(&state, |space, host| {
        Ok(Json(SpaceInfo {
            path: space.path().display().to_string(),
            host: host.to_owned(),
            objects: u64::try_from(space.object_count()).unwrap_or(u64::MAX),
            key: space.public_key().map(|key| key.to_string()),
        }))
    })
    .await
}

async fn branches(State(state): State<AppState>) -> ApiResult<Json<Vec<BranchInfo>>> {
    with_space(&state, |space, _| {
        Ok(Json(
            space
                .branches()?
                .into_iter()
                .map(|(name, head)| BranchInfo {
                    name,
                    head: head.to_string(),
                })
                .collect(),
        ))
    })
    .await
}

async fn fork(
    State(state): State<AppState>,
    body: Result<Json<ForkRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<BranchInfo>)> {
    let Json(request) = body?;
    with_space(&state, move |space, _| {
        let at = head(space, &request.from)?;
        space.create_branch(&request.name, at)?;
        Ok((
            StatusCode::CREATED,
            Json(BranchInfo {
                name: request.name,
                head: at.to_string(),
            }),
        ))
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    limit: Option<usize>,
}

async fn log(
    State(state): State<AppState>,
    UrlPath(branch): UrlPath<String>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<CommitInfo>>> {
    let Query(query) = query?;
    with_space(&state, move |space, _| {
        let history = space.history(head(space, &branch)?)?;
        let commits = history
            .into_iter()
            .take(query.limit.unwrap_or(usize::MAX))
            .map(|(id, commit)| commit_info(id, commit))
            .collect();
        Ok(Json(commits))
    })
    .await
}

async fn import(
    State(state): State<AppState>,
    body: Result<Json<ImportRequest>, JsonRejection>,
) -> ApiResult<Json<BranchInfo>> {
    let Json(request) = body?;
    outside_mounts(&state, &request.source)?;
    with_space(&state, move |space, host| {
        let tree = space.import(absolute(&request.source)?)?;
        let commit = space.commit_tree(
            &request.branch,
            tree,
            &request.message,
            &author(host, request.caller),
        )?;
        Ok(Json(BranchInfo {
            name: request.branch,
            head: commit.to_string(),
        }))
    })
    .await
}

async fn export(
    State(state): State<AppState>,
    body: Result<Json<ExportRequest>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Json(request) = body?;
    outside_mounts(&state, &request.target)?;
    with_space(&state, move |space, _| {
        let commit = match &request.from {
            Revision::Branch(branch) => head(space, branch)?,
            Revision::Commit(id) => id
                .parse()
                .map_err(|e| ApiError::invalid(format!("commit {id}: {e}")))?,
        };
        let tree = space.commit(commit)?.tree;
        space.export(tree, absolute(&request.target)?)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn workspaces(State(state): State<AppState>) -> ApiResult<Json<Vec<WorkspaceInfo>>> {
    with_space(&state, |space, _| {
        let mut all = Vec::new();
        for name in space.workspaces()? {
            all.push(workspace_info(&space.workspace(&name)?));
        }
        Ok(Json(all))
    })
    .await
}

async fn create_workspace(
    State(state): State<AppState>,
    body: Result<Json<CreateWorkspaceRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<WorkspaceInfo>)> {
    let Json(request) = body?;
    with_space(&state, move |space, _| {
        let workspace = space.create_workspace(&request.name, &request.branch)?;
        Ok((StatusCode::CREATED, Json(workspace_info(&workspace))))
    })
    .await
}

async fn workspace_info_route(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<Json<WorkspaceInfo>> {
    with_space(&state, move |space, _| {
        Ok(Json(workspace_info(&space.workspace(&name)?)))
    })
    .await
}

async fn discard_workspace(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<StatusCode> {
    if lock_mounts(&state).contains_key(&name) {
        return Err(ApiError::conflict(format!(
            "workspace {name} is mounted; unmount it first"
        )));
    }
    with_space(&state, move |space, _| {
        space.discard_workspace(&name)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn read_file(
    State(state): State<AppState>,
    UrlPath((name, path)): UrlPath<(String, String)>,
) -> ApiResult<Vec<u8>> {
    with_space(&state, move |space, _| {
        Ok(space.workspace(&name)?.read(space, &path)?)
    })
    .await
}

async fn write_file(
    State(state): State<AppState>,
    UrlPath((name, path)): UrlPath<(String, String)>,
    bytes: Bytes,
) -> ApiResult<StatusCode> {
    with_space(&state, move |space, _| {
        space.workspace(&name)?.write(space, &path, &bytes)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn remove_file(
    State(state): State<AppState>,
    UrlPath((name, path)): UrlPath<(String, String)>,
) -> ApiResult<StatusCode> {
    with_space(&state, move |space, _| {
        space.workspace(&name)?.remove(space, &path)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    path: String,
}

async fn list(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<DirEntry>>> {
    let Query(query) = query?;
    with_space(&state, move |space, _| {
        let entries = space.workspace(&name)?.list(space, &query.path)?;
        Ok(Json(
            entries
                .into_iter()
                .map(|(name, kind)| DirEntry {
                    name,
                    kind: match kind {
                        peerfs_core::EntryKind::File => EntryKind::File,
                        peerfs_core::EntryKind::Symlink => EntryKind::Symlink,
                        peerfs_core::EntryKind::Directory => EntryKind::Directory,
                    },
                })
                .collect(),
        ))
    })
    .await
}

async fn changes(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<Json<Vec<ChangeInfo>>> {
    with_space(&state, move |space, _| {
        let workspace = space.workspace(&name)?;
        let mut infos = Vec::new();
        for change in workspace.changes(space)? {
            infos.push(match change {
                Change::Added(path) => ChangeInfo::Added {
                    path: path.to_string(),
                },
                Change::Written(path) => ChangeInfo::Written {
                    path: path.to_string(),
                },
                Change::Removed(path) => ChangeInfo::Removed {
                    path: path.to_string(),
                },
                other => return Err(ApiError::internal(&format!("{other:?}"))),
            });
        }
        for (path, class) in workspace.kept_local(space)? {
            infos.push(ChangeInfo::Kept {
                path: path.to_string(),
                class: class.to_string(),
            });
        }
        Ok(Json(infos))
    })
    .await
}

fn conflict_infos(conflicts: Vec<Conflict>) -> Vec<ConflictInfo> {
    conflicts
        .into_iter()
        .map(|conflict| ConflictInfo {
            path: conflict.path.to_string(),
            kind: conflict_kind(conflict.kind).to_owned(),
        })
        .collect()
}

fn conflict_kind(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::Content => "content",
        ConflictKind::AddAdd => "add_add",
        ConflictKind::ModifyDelete => "modify_delete",
        ConflictKind::DeleteModify => "delete_modify",
        ConflictKind::Binary => "binary",
        ConflictKind::Mode => "mode",
        ConflictKind::Type => "type",
        _ => "other",
    }
}

async fn commit(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<CommitRequest>, JsonRejection>,
) -> ApiResult<Json<CommitResponse>> {
    let Json(request) = body?;
    with_space(&state, move |space, host| {
        let mut workspace = space.workspace(&name)?;
        let author = author(host, request.caller);
        let response = match space.commit_workspace(&mut workspace, &request.message, &author)? {
            CommitOutcome::Committed { commit, merged } => CommitResponse::Committed {
                commit: commit.to_string(),
                merged: merged == Merged::Yes,
            },
            CommitOutcome::Conflicted(conflicts) => CommitResponse::Conflicted {
                conflicts: conflict_infos(conflicts),
            },
            CommitOutcome::Unchanged => CommitResponse::Unchanged,
            other => return Err(ApiError::internal(&format!("{other:?}"))),
        };
        Ok(Json(response))
    })
    .await
}

async fn snapshot(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<SnapshotRequest>, JsonRejection>,
) -> ApiResult<Json<SnapshotResponse>> {
    let Json(request) = body?;
    with_space(&state, move |space, host| {
        let mut workspace = space.workspace(&name)?;
        let message = request.message.as_deref().unwrap_or("Snapshot");
        let author = author(host, request.caller);
        Ok(Json(
            match space.snapshot_workspace(&mut workspace, message, &author)? {
                Some(snapshot) => SnapshotResponse::Snapshot {
                    snapshot: snapshot.to_string(),
                },
                None => SnapshotResponse::Unchanged,
            },
        ))
    })
    .await
}

async fn snapshots(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<Json<Vec<CommitInfo>>> {
    with_space(&state, move |space, _| {
        let workspace = space.workspace(&name)?;
        Ok(Json(
            space
                .snapshots(&workspace)?
                .into_iter()
                .map(|(id, commit)| commit_info(id, commit))
                .collect(),
        ))
    })
    .await
}

async fn fetch_objects(
    State(state): State<AppState>,
    body: Result<Json<FetchRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(request) = body?;
    if request.ids.len() > FETCH_BATCH {
        return Err(ApiError::invalid(format!(
            "at most {FETCH_BATCH} objects per fetch, not {}",
            request.ids.len()
        )));
    }
    let ids = request
        .ids
        .iter()
        .map(|id| {
            id.parse::<ObjectId>()
                .map_err(|e| ApiError::invalid(format!("object {id}: {e}")))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let encoded = with_space(&state, move |space, _| {
        let mut objects = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(object) = space.raw_object(id)? {
                objects.push(object);
            }
        }
        Ok(encode_objects(&objects))
    })
    .await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        encoded,
    )
        .into_response())
}

/// Pulls from another server, holding the space only between network waits, so two
/// servers pulling from each other cannot deadlock.
async fn pull(
    State(state): State<AppState>,
    body: Result<Json<PullRequest>, JsonRejection>,
) -> ApiResult<Json<PullResponse>> {
    let Json(request) = body?;
    let from = absolute(&request.from)?.to_owned();
    let remote = Client::new(&from);
    let info = remote.space().await?;
    let own = with_space(&state, |space, _| Ok(space.path().to_owned())).await?;
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(Path::new(&info.path), &own) {
        return Err(ApiError::invalid("a space cannot pull from itself"));
    }
    let head = remote
        .branches()
        .await?
        .into_iter()
        .find(|branch| branch.name == request.branch)
        .ok_or_else(|| {
            ApiError::not_found(format!("no branch {} on {}", request.branch, info.host))
        })?
        .head;
    let head: ObjectId = head
        .parse()
        .map_err(|e| ApiError::remote(format!("{} sent head {head}: {e}", info.host)))?;
    let (host, branch) = (info.host.clone(), request.branch.clone());
    let mut pending = with_space(&state, move |space, _| {
        Ok(space.start_pull(&host, &branch, head)?)
    })
    .await?;
    loop {
        let wanted = pending.wanted();
        if wanted.is_empty() {
            break;
        }
        let ids: Vec<String> = wanted.iter().map(ToString::to_string).collect();
        let bytes = remote.fetch_objects(&ids).await?;
        let objects = decode_objects(&bytes).map_err(|e| {
            ApiError::remote(format!("{} sent undecodable objects: {e}", info.host))
        })?;
        pending = with_space(&state, move |space, _| {
            space.receive(&mut pending, objects)?;
            Ok(pending)
        })
        .await?;
    }
    let pulled = with_space(&state, move |space, _| Ok(space.finish_pull(pending)?)).await?;
    Ok(Json(PullResponse {
        remote: info.host,
        branch: request.branch,
        head: pulled.head.to_string(),
        objects: pulled.objects,
        bytes: pulled.bytes,
    }))
}

async fn hosts(State(state): State<AppState>) -> ApiResult<Json<Vec<HostKey>>> {
    with_space(&state, |space, _| {
        Ok(Json(
            space
                .known_hosts()?
                .into_iter()
                .map(|(host, key)| HostKey {
                    host,
                    key: key.to_string(),
                })
                .collect(),
        ))
    })
    .await
}

/// Adds a host key: `201 Created` when new, `200 OK` when it was already trusted.
async fn add_host(
    State(state): State<AppState>,
    body: Result<Json<HostKey>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<HostKey>)> {
    let Json(request) = body?;
    let key: PublicKey = request
        .key
        .parse()
        .map_err(|e: peerfs_core::ParseKeyError| ApiError::invalid(e.to_string()))?;
    with_space(&state, move |space, _| {
        let status = if space.add_known_host(&request.host, &key)? {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((status, Json(request)))
    })
    .await
}

fn lock_mounts(state: &Shared) -> std::sync::MutexGuard<'_, BTreeMap<String, Mount>> {
    state
        .mounts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Refuses a path inside a mount: the server would wait on itself serving it.
fn outside_mounts(state: &Shared, path: &str) -> ApiResult<()> {
    let path = absolute(path)?;
    let inside = lock_mounts(state)
        .values()
        .any(|mount| path.starts_with(mount.path()));
    if inside {
        return Err(ApiError::invalid(format!(
            "{} is inside a mounted workspace; use the workspace instead",
            path.display()
        )));
    }
    Ok(())
}

async fn mount_workspace(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<MountInfo>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<MountInfo>)> {
    let Json(request) = body?;
    let path = absolute(&request.path)?.to_owned();
    with_space(&state, {
        let name = name.clone();
        move |space, _| Ok(space.workspace(&name).map(drop)?)
    })
    .await?;
    let shared = Arc::clone(&state);
    tokio::task::spawn_blocking(move || {
        // Not under the space's lock: checking the directory could reach another mount.
        let mut mounts = lock_mounts(&shared);
        if let Some(mounted) = mounts.get(&name) {
            return Err(ApiError::conflict(format!(
                "workspace {name} is mounted at {} already",
                mounted.path().display()
            )));
        }
        let mount =
            Mount::new(Arc::clone(&shared.space), &name, &path).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound
                | io::ErrorKind::DirectoryNotEmpty
                | io::ErrorKind::NotADirectory => {
                    ApiError::invalid(format!("{}: {e}", path.display()))
                }
                _ => ApiError::internal(&format!("mounting at {}: {e}", path.display())),
            })?;
        let info = MountInfo {
            workspace: name.clone(),
            path: mount.path().display().to_string(),
        };
        mounts.insert(name, mount);
        Ok((StatusCode::CREATED, Json(info)))
    })
    .await
    .map_err(|e| ApiError::internal(&e))?
}

async fn unmount_workspace(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult<StatusCode> {
    let Some(mount) = lock_mounts(&state).remove(&name) else {
        return Err(ApiError::not_found(format!(
            "workspace {name} is not mounted"
        )));
    };
    tokio::task::spawn_blocking(move || mount.unmount())
        .await
        .map_err(|e| ApiError::internal(&e))?
        .map_err(|e| ApiError::internal(&format!("unmounting {name}: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn mounts(State(state): State<AppState>) -> ApiResult<Json<Vec<MountInfo>>> {
    Ok(Json(
        lock_mounts(&state)
            .iter()
            .map(|(workspace, mount)| MountInfo {
                workspace: workspace.clone(),
                path: mount.path().display().to_string(),
            })
            .collect(),
    ))
}

async fn remotes(State(state): State<AppState>) -> ApiResult<Json<Vec<TrackingInfo>>> {
    with_space(&state, |space, _| {
        Ok(Json(
            space
                .tracking()?
                .into_iter()
                .map(|tracking| TrackingInfo {
                    remote: tracking.remote,
                    branch: tracking.branch,
                    head: tracking.head.to_string(),
                    pulled_ms: tracking
                        .pulled
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |since| {
                            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                        }),
                })
                .collect(),
        ))
    })
    .await
}

fn parse_ids(ids: &[String]) -> ApiResult<Vec<ObjectId>> {
    if ids.len() > FETCH_BATCH {
        return Err(ApiError::invalid(format!(
            "at most {FETCH_BATCH} objects per request, not {}",
            ids.len()
        )));
    }
    ids.iter()
        .map(|id| {
            id.parse::<ObjectId>()
                .map_err(|e| ApiError::invalid(format!("object {id}: {e}")))
        })
        .collect()
}

async fn have_objects(
    State(state): State<AppState>,
    body: Result<Json<HaveRequest>, JsonRejection>,
) -> ApiResult<Json<HaveResponse>> {
    let Json(request) = body?;
    let ids = parse_ids(&request.ids)?;
    with_space(&state, move |space, _| {
        Ok(Json(HaveResponse {
            have: space.have(&ids),
        }))
    })
    .await
}

async fn receive_objects(
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<ReceiveResponse>> {
    let objects = decode_objects(&body)
        .map_err(|e| ApiError::invalid(format!("undecodable objects: {e}")))?;
    with_space(&state, move |space, _| {
        Ok(Json(ReceiveResponse {
            stored: space.accept_objects(&objects)?,
        }))
    })
    .await
}

async fn advance(
    State(state): State<AppState>,
    UrlPath(branch): UrlPath<String>,
    body: Result<Json<AdvanceRequest>, JsonRejection>,
) -> ApiResult<Json<BranchInfo>> {
    let Json(request) = body?;
    let parse = |id: &str| {
        id.parse::<ObjectId>()
            .map_err(|e| ApiError::invalid(format!("commit {id}: {e}")))
    };
    let expected = request.expected.as_deref().map(parse).transpose()?;
    let new = parse(&request.new)?;
    with_space(&state, move |space, _| {
        space.advance_branch(&branch, expected, new)?;
        Ok(Json(BranchInfo {
            name: branch,
            head: new.to_string(),
        }))
    })
    .await
}

/// Sends a local revision to another server and moves its branch, holding the space only
/// between network waits.
async fn push(
    State(state): State<AppState>,
    body: Result<Json<PushRequest>, JsonRejection>,
) -> ApiResult<Json<PushResponse>> {
    let Json(request) = body?;
    let to = absolute(&request.to)?.to_owned();
    let remote = Client::new(&to);
    let info = remote.space().await?;
    let own = with_space(&state, |space, _| Ok(space.path().to_owned())).await?;
    if std::fs::canonicalize(&info.path).ok() == std::fs::canonicalize(&own).ok() {
        return Err(ApiError::invalid("a space cannot push to itself"));
    }
    let from = request.from.clone();
    let mut pending = with_space(&state, move |space, _| {
        let head = head(space, &from)?;
        Ok(space.start_push(head)?)
    })
    .await?;
    let head = pending.head();
    loop {
        let ask = pending.to_ask();
        if ask.is_empty() {
            break;
        }
        let ids: Vec<String> = ask.iter().map(ToString::to_string).collect();
        let answers = remote.have(&ids).await?;
        pending = with_space(&state, move |space, _| {
            space.answer_push(&mut pending, &answers)?;
            Ok(pending)
        })
        .await?;
    }
    let order = with_space(&state, move |space, _| Ok(space.push_order(&pending)?)).await?;
    let objects = u64::try_from(order.len()).unwrap_or(u64::MAX);
    let mut bytes = 0;
    for batch in order.chunks(FETCH_BATCH) {
        let batch = batch.to_vec();
        let (encoded, size) = with_space(&state, move |space, _| {
            let mut raw = Vec::with_capacity(batch.len());
            for id in batch {
                raw.push(space.raw_object(id)?.ok_or_else(|| {
                    ApiError::internal(&format!("{id} is referenced but missing here"))
                })?);
            }
            let size: usize = raw.iter().map(|object| object.bytes.len()).sum();
            Ok((encode_objects(&raw), size))
        })
        .await?;
        bytes += u64::try_from(size).unwrap_or(u64::MAX);
        remote.receive_objects(encoded.into()).await?;
    }
    let (host, branch) = (info.host.clone(), request.branch.clone());
    let tracked = with_space(&state, move |space, _| {
        Ok(space.resolve(&format!("{host}/{branch}"))?)
    })
    .await?;
    let expected = match tracked {
        Some(seen) => Some(seen.to_string()),
        None => remote
            .branches()
            .await?
            .into_iter()
            .find(|found| found.name == request.branch)
            .map(|found| found.head),
    };
    let new = head.to_string();
    if let Err(error) = remote
        .advance(&request.branch, expected.as_deref(), &new)
        .await
    {
        return Err(if error.code() == Some("conflict") {
            ApiError {
                status: StatusCode::CONFLICT,
                code: "conflict",
                message: format!(
                    "{error}; pull {host}/{branch}, merge it, and push again",
                    host = info.host,
                    branch = request.branch
                ),
            }
        } else {
            error.into()
        });
    }
    let (host, branch) = (info.host.clone(), request.branch.clone());
    with_space(&state, move |space, _| {
        Ok(space.record_tracking(&host, &branch, head)?)
    })
    .await?;
    Ok(Json(PushResponse {
        remote: info.host,
        branch: request.branch,
        head: new,
        objects,
        bytes,
    }))
}

async fn merge(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<MergeRequest>, JsonRejection>,
) -> ApiResult<Json<MergeResponse>> {
    let Json(request) = body?;
    with_space(&state, move |space, host| {
        let mut workspace = space.workspace(&name)?;
        let author = author(host, request.caller);
        let response = match space.merge_into_workspace(&mut workspace, &request.from, &author)? {
            MergeOutcome::UpToDate => MergeResponse::UpToDate,
            MergeOutcome::FastForward { commit } => MergeResponse::FastForward {
                commit: commit.to_string(),
            },
            MergeOutcome::Merged { commit } => MergeResponse::Merged {
                commit: commit.to_string(),
            },
            MergeOutcome::Conflicted(conflicts) => MergeResponse::Conflicted {
                conflicts: conflict_infos(conflicts),
            },
            other => return Err(ApiError::internal(&format!("{other:?}"))),
        };
        Ok(Json(response))
    })
    .await
}
