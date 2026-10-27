//! The peerfs local API server: one space, served over a Unix socket.
//!
//! Requests run one at a time against the space, so agents working concurrently serialize
//! here instead of contending for the space's lock. The routes and bodies are those of
//! [`peerfs_api`].

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
    BranchInfo, Caller, ChangeInfo, Client, ClientError, CommitInfo, CommitRequest, CommitResponse,
    ConflictInfo, CreateWorkspaceRequest, DirEntry, EntryKind, ErrorBody, ExportRequest,
    FetchRequest, ForkRequest, ImportRequest, PullRequest, PullResponse, Revision, SOCKET_NAME,
    SpaceInfo, TrackingInfo, WorkspaceInfo,
};
use peerfs_core::{
    Author, Change, CommitOutcome, ConflictKind, Error, FETCH_BATCH, Merged, ObjectId, Space,
    Workspace, decode_objects, encode_objects,
};
use serde::Deserialize;
use tokio::net::UnixListener;
use tokio::sync::Notify;

/// The largest JSON request body, as on the host daemon's `admin.sock`.
const JSON_LIMIT: usize = 1024 * 1024;

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
    space: Mutex<Space>,
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
        space: Mutex::new(space),
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
        .route("/v1/objects/fetch", post(fetch_objects))
        .route("/v1/pull", post(pull))
        .route("/v1/remotes", get(remotes))
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
        space: Mutex::new(space),
        host: options.host,
        started: Instant::now(),
        last_request_ms: AtomicU64::new(0),
    });
    let stop = Arc::new(Notify::new());
    if let Some(timeout) = options.idle_timeout {
        tokio::spawn(watch_idle(Arc::clone(&state), timeout, Arc::clone(&stop)));
    }
    let served = axum::serve(listener, router_with(state))
        .with_graceful_shutdown(shutdown(stop))
        .await;
    let removed = std::fs::remove_file(&socket).and(std::fs::remove_file(&pid_file));
    served?;
    removed
}

async fn watch_idle(state: AppState, timeout: Duration, stop: Arc<Notify>) {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    loop {
        tokio::time::sleep(IDLE_CHECK.min(timeout)).await;
        let now = elapsed_ms(state.started);
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
            Error::BranchExists(_) | Error::WorkspaceExists(_) | Error::BranchMoved { .. } => {
                (StatusCode::CONFLICT, "conflict")
            }
            Error::InvalidName(_)
            | Error::InvalidPath(_)
            | Error::WrongEntry { .. }
            | Error::Unsupported { .. } => (StatusCode::BAD_REQUEST, "invalid"),
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
            .map(|(id, commit)| CommitInfo {
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
            })
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
        let changes = space.workspace(&name)?.changes()?;
        Ok(Json(
            changes
                .into_iter()
                .map(|change| match change {
                    Change::Written(path) => ChangeInfo::Written {
                        path: path.to_string(),
                    },
                    Change::Removed(path) => ChangeInfo::Removed {
                        path: path.to_string(),
                    },
                })
                .collect(),
        ))
    })
    .await
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
                conflicts: conflicts
                    .into_iter()
                    .map(|conflict| ConflictInfo {
                        path: conflict.path.to_string(),
                        kind: conflict_kind(conflict.kind).to_owned(),
                    })
                    .collect(),
            },
            CommitOutcome::Unchanged => CommitResponse::Unchanged,
            other => return Err(ApiError::internal(&format!("{other:?}"))),
        };
        Ok(Json(response))
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
