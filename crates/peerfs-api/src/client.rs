//! A client of the local API over its Unix socket.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

use crate::wire::{
    AdvanceRequest, BranchInfo, Caller, ChangeInfo, CommitInfo, CommitRequest, CommitResponse,
    CreateWorkspaceRequest, DirEntry, ErrorBody, ExportRequest, FetchRequest, ForkRequest,
    HaveRequest, HaveResponse, ImportRequest, MergeRequest, MergeResponse, PullRequest,
    PullResponse, PushRequest, PushResponse, ReceiveResponse, Revision, SpaceInfo, TrackingInfo,
    WorkspaceInfo,
};

/// A connection-per-request client of a `peerfs` server.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

/// A request that failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// No server answered at the socket.
    #[error("cannot connect to {}: {source}", socket.display())]
    Connect {
        /// The socket.
        socket: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The HTTP exchange failed.
    #[error("request failed: {0}")]
    Http(#[from] hyper::Error),
    /// The server answered with an error.
    #[error("{error} ({code})")]
    Api {
        /// The HTTP status.
        status: u16,
        /// `invalid`, `not_found`, `conflict` or `internal`.
        code: String,
        /// The server's message.
        error: String,
    },
    /// A response could not be decoded.
    #[error("unexpected response: {0}")]
    Decode(String),
}

impl ClientError {
    /// The server's error code, when the server answered with an error.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// A raw response: status and body.
#[derive(Debug, Clone)]
pub struct RawResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Bytes,
}

/// Percent-encodes `text`, keeping `/` when `keep_slash` says so.
fn encode(text: &str, keep_slash: Slash) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || b"-._~".contains(&byte);
        if unreserved || (byte == b'/' && keep_slash == Slash::Keep) {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slash {
    Keep,
    Encode,
}

fn segment(text: &str) -> String {
    encode(text, Slash::Encode)
}

impl Client {
    /// A client of the server listening on `socket`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The socket this client connects to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Sends one request and returns the raw response, whatever its status.
    ///
    /// # Errors
    ///
    /// Fails if the server cannot be reached or the exchange breaks off.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Bytes,
    ) -> Result<RawResponse, ClientError> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| ClientError::Connect {
                    socket: self.socket.clone(),
                    source,
                })?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "peerfs");
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        let request = request
            .body(Full::new(body))
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        let response = sender.send_request(request).await?;
        let status = response.status().as_u16();
        let body = response.into_body().collect().await?.to_bytes();
        Ok(RawResponse { status, body })
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Bytes,
    ) -> Result<Bytes, ClientError> {
        let response = self.send(method, path, content_type, body).await?;
        if StatusCode::from_u16(response.status).is_ok_and(|status| status.is_success()) {
            return Ok(response.body);
        }
        let error: ErrorBody = serde_json::from_slice(&response.body).map_err(|_| {
            ClientError::Decode(format!(
                "status {} with body {:?}",
                response.status,
                String::from_utf8_lossy(&response.body)
            ))
        })?;
        Err(ClientError::Api {
            status: response.status,
            code: error.code,
            error: error.error,
        })
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, ClientError> {
        let bytes = self.call_json(method, path, body).await?;
        serde_json::from_slice(&bytes).map_err(|e| ClientError::Decode(e.to_string()))
    }

    async fn call_json(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<Bytes, ClientError> {
        let (content_type, body) = match body {
            Some(body) => (
                Some("application/json"),
                Bytes::from(
                    serde_json::to_vec(body).map_err(|e| ClientError::Decode(e.to_string()))?,
                ),
            ),
            None => (None, Bytes::new()),
        };
        self.call(method, path, content_type, body).await
    }

    /// What the server serves.
    ///
    /// # Errors
    ///
    /// Fails if the server cannot be reached or answers with an error.
    pub async fn space(&self) -> Result<SpaceInfo, ClientError> {
        self.json(Method::GET, "/v1/space", None::<&()>).await
    }

    /// Every branch and its head.
    ///
    /// # Errors
    ///
    /// Fails if the server cannot be reached or answers with an error.
    pub async fn branches(&self) -> Result<Vec<BranchInfo>, ClientError> {
        self.json(Method::GET, "/v1/branches", None::<&()>).await
    }

    /// Creates branch `name` at the head of `from`.
    ///
    /// # Errors
    ///
    /// Fails with `conflict` if the branch exists, `not_found` if `from` does not.
    pub async fn fork(&self, name: &str, from: &str) -> Result<BranchInfo, ClientError> {
        let request = ForkRequest {
            name: name.to_owned(),
            from: from.to_owned(),
        };
        self.json(Method::POST, "/v1/branches", Some(&request))
            .await
    }

    /// A branch's history, newest first, at most `limit` commits.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such branch.
    pub async fn log(
        &self,
        branch: &str,
        limit: Option<usize>,
    ) -> Result<Vec<CommitInfo>, ClientError> {
        let mut path = format!("/v1/branches/{}/log", segment(branch));
        if let Some(limit) = limit {
            write!(path, "?limit={limit}").expect("writing to a String cannot fail");
        }
        self.json(Method::GET, &path, None::<&()>).await
    }

    /// Commits the directory `source`, on the server's host, onto `branch`.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be imported or the branch does not exist.
    pub async fn import(
        &self,
        source: &str,
        branch: &str,
        message: &str,
        caller: &Caller,
    ) -> Result<BranchInfo, ClientError> {
        let request = ImportRequest {
            source: source.to_owned(),
            branch: branch.to_owned(),
            message: message.to_owned(),
            caller: caller.clone(),
        };
        self.json(Method::POST, "/v1/import", Some(&request)).await
    }

    /// Writes a revision's tree to the empty directory `target` on the server's host.
    ///
    /// # Errors
    ///
    /// Fails if the target is not empty or the revision does not exist.
    pub async fn export(&self, target: &str, from: Revision) -> Result<(), ClientError> {
        let request = ExportRequest {
            target: target.to_owned(),
            from,
        };
        self.call_json(Method::POST, "/v1/export", Some(&request))
            .await
            .map(drop)
    }

    /// The objects among `ids` this space holds, encoded by `peerfs_core::encode_objects`.
    ///
    /// # Errors
    ///
    /// Fails with `invalid` for more than 256 ids or a malformed id.
    pub async fn fetch_objects(&self, ids: &[String]) -> Result<Bytes, ClientError> {
        let request = FetchRequest { ids: ids.to_vec() };
        self.call_json(Method::POST, "/v1/objects/fetch", Some(&request))
            .await
    }

    /// Has this server pull `branch` and its history from the server at `from`.
    ///
    /// # Errors
    ///
    /// Fails if the other server cannot be reached, lacks the branch, or sends objects
    /// that do not match.
    pub async fn pull(&self, from: &str, branch: &str) -> Result<PullResponse, ClientError> {
        let request = PullRequest {
            from: from.to_owned(),
            branch: branch.to_owned(),
        };
        self.json(Method::POST, "/v1/pull", Some(&request)).await
    }

    /// Every tracking reference: other hosts' branch heads as of their last pull.
    ///
    /// # Errors
    ///
    /// Fails if the server cannot be reached or answers with an error.
    pub async fn remotes(&self) -> Result<Vec<TrackingInfo>, ClientError> {
        self.json(Method::GET, "/v1/remotes", None::<&()>).await
    }

    /// Which of `ids` this space holds, in order.
    ///
    /// # Errors
    ///
    /// Fails with `invalid` for more than 256 ids or a malformed id.
    pub async fn have(&self, ids: &[String]) -> Result<Vec<bool>, ClientError> {
        let request = HaveRequest { ids: ids.to_vec() };
        let response: HaveResponse = self
            .json(Method::POST, "/v1/objects/have", Some(&request))
            .await?;
        Ok(response.have)
    }

    /// Sends objects encoded by `peerfs_core::encode_objects`, each after everything it
    /// references. Returns how many were new.
    ///
    /// # Errors
    ///
    /// Fails with `remote` when an object does not match its identity or references one
    /// the space does not hold; the objects before it are kept.
    pub async fn receive_objects(&self, encoded: Bytes) -> Result<u64, ClientError> {
        let body = self
            .call(
                Method::POST,
                "/v1/objects/receive",
                Some("application/octet-stream"),
                encoded,
            )
            .await?;
        let response: ReceiveResponse =
            serde_json::from_slice(&body).map_err(|e| ClientError::Decode(e.to_string()))?;
        Ok(response.stored)
    }

    /// Moves `branch` from `expected` (`None`: creates it) to the held commit `new`, which
    /// must contain `expected` in its history.
    ///
    /// # Errors
    ///
    /// Fails with `conflict` if the branch moved or `new` does not contain `expected`.
    pub async fn advance(
        &self,
        branch: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<BranchInfo, ClientError> {
        let path = format!("/v1/branches/{}/advance", segment(branch));
        let request = AdvanceRequest {
            expected: expected.map(ToOwned::to_owned),
            new: new.to_owned(),
        };
        self.json(Method::POST, &path, Some(&request)).await
    }

    /// Has this server send the local revision `from` to the server at `to` and move
    /// `branch` there to it.
    ///
    /// # Errors
    ///
    /// Fails with `conflict` if the branch there moved since this server last saw it or
    /// the revision does not contain it: pull, merge, and push again.
    pub async fn push(
        &self,
        to: &str,
        branch: &str,
        from: &str,
    ) -> Result<PushResponse, ClientError> {
        let request = PushRequest {
            to: to.to_owned(),
            branch: branch.to_owned(),
            from: from.to_owned(),
        };
        self.json(Method::POST, "/v1/push", Some(&request)).await
    }

    /// Merges the revision `from` into the branch a workspace follows.
    ///
    /// # Errors
    ///
    /// Fails with `conflict` if the workspace has uncommitted changes, `not_found` if the
    /// revision does not exist. A merge conflict is an outcome, not an error.
    pub async fn merge(
        &self,
        workspace: &str,
        from: &str,
        caller: &Caller,
    ) -> Result<MergeResponse, ClientError> {
        let path = format!("/v1/workspaces/{}/merge", segment(workspace));
        let request = MergeRequest {
            from: from.to_owned(),
            caller: caller.clone(),
        };
        self.json(Method::POST, &path, Some(&request)).await
    }

    /// Every workspace.
    ///
    /// # Errors
    ///
    /// Fails if the server cannot be reached or answers with an error.
    pub async fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, ClientError> {
        self.json(Method::GET, "/v1/workspaces", None::<&()>).await
    }

    /// Creates a workspace following `branch`.
    ///
    /// # Errors
    ///
    /// Fails with `conflict` if the workspace exists, `not_found` if the branch does not.
    pub async fn create_workspace(
        &self,
        name: &str,
        branch: &str,
    ) -> Result<WorkspaceInfo, ClientError> {
        let request = CreateWorkspaceRequest {
            name: name.to_owned(),
            branch: branch.to_owned(),
        };
        self.json(Method::POST, "/v1/workspaces", Some(&request))
            .await
    }

    /// One workspace.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such workspace.
    pub async fn workspace(&self, name: &str) -> Result<WorkspaceInfo, ClientError> {
        let path = format!("/v1/workspaces/{}", segment(name));
        self.json(Method::GET, &path, None::<&()>).await
    }

    /// Deletes a workspace and its uncommitted changes.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such workspace.
    pub async fn discard(&self, name: &str) -> Result<(), ClientError> {
        let path = format!("/v1/workspaces/{}", segment(name));
        self.call(Method::DELETE, &path, None, Bytes::new())
            .await
            .map(drop)
    }

    fn file_path(workspace: &str, path: &str) -> String {
        format!(
            "/v1/workspaces/{}/files/{}",
            segment(workspace),
            encode(path.trim_start_matches('/'), Slash::Keep)
        )
    }

    /// Reads a file in a workspace.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such file, `invalid` if it is not a file.
    pub async fn read(&self, workspace: &str, path: &str) -> Result<Bytes, ClientError> {
        self.call(
            Method::GET,
            &Self::file_path(workspace, path),
            None,
            Bytes::new(),
        )
        .await
    }

    /// Writes a file in a workspace, creating its directories.
    ///
    /// # Errors
    ///
    /// Fails if the workspace does not exist or the path is invalid.
    pub async fn write(
        &self,
        workspace: &str,
        path: &str,
        bytes: impl Into<Bytes>,
    ) -> Result<(), ClientError> {
        self.call(
            Method::PUT,
            &Self::file_path(workspace, path),
            Some("application/octet-stream"),
            bytes.into(),
        )
        .await
        .map(drop)
    }

    /// Removes a file or directory in a workspace.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if nothing is there.
    pub async fn remove(&self, workspace: &str, path: &str) -> Result<(), ClientError> {
        self.call(
            Method::DELETE,
            &Self::file_path(workspace, path),
            None,
            Bytes::new(),
        )
        .await
        .map(drop)
    }

    /// Lists a directory in a workspace.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such directory.
    pub async fn list(&self, workspace: &str, path: &str) -> Result<Vec<DirEntry>, ClientError> {
        let path = format!(
            "/v1/workspaces/{}/list?path={}",
            segment(workspace),
            segment(path)
        );
        self.json(Method::GET, &path, None::<&()>).await
    }

    /// A workspace's uncommitted changes.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such workspace.
    pub async fn changes(&self, workspace: &str) -> Result<Vec<ChangeInfo>, ClientError> {
        let path = format!("/v1/workspaces/{}/changes", segment(workspace));
        self.json(Method::GET, &path, None::<&()>).await
    }

    /// Commits a workspace's changes to its branch, merging if the branch moved.
    ///
    /// # Errors
    ///
    /// Fails with `not_found` if there is no such workspace. A conflict is not an error:
    /// it is [`CommitResponse::Conflicted`].
    pub async fn commit(
        &self,
        workspace: &str,
        message: &str,
        caller: &Caller,
    ) -> Result<CommitResponse, ClientError> {
        let path = format!("/v1/workspaces/{}/commit", segment(workspace));
        let request = CommitRequest {
            message: message.to_owned(),
            caller: caller.clone(),
        };
        self.json(Method::POST, &path, Some(&request)).await
    }
}
