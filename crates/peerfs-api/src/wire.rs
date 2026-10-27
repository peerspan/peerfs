//! Request and response bodies of the local API, as JSON.
//!
//! Requests reject unknown fields. Absent optional fields are omitted, never `null`.
//! Object and commit identities are lowercase hexadecimal strings.

use serde::{Deserialize, Serialize};

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    /// `invalid`, `not_found`, `conflict`, `forbidden`, `remote` or `internal`.
    pub code: String,
    /// What went wrong. Internal errors say only `internal error`.
    pub error: String,
}

/// `GET /v1/space`: what the server serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceInfo {
    /// The space's directory on the server's host.
    pub path: String,
    /// The host recorded on commits made through this server.
    pub host: String,
    /// The number of objects stored.
    pub objects: u64,
    /// The key the space signs its commits with, in standard Base64, if it signs them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A key trusted to sign another host's commits: an entry of `GET /v1/hosts`, and the body
/// of `POST /v1/hosts`, which adds one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostKey {
    /// The host, as commits name their author's host.
    pub host: String,
    /// Its Ed25519 public key, in standard Base64.
    pub key: String,
}

/// A branch and the commit it points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchInfo {
    /// The branch.
    pub name: String,
    /// Its head commit.
    pub head: String,
}

/// `POST /v1/branches`: create a branch at another's head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkRequest {
    /// The new branch.
    pub name: String,
    /// The branch whose head it starts at.
    pub from: String,
}

/// Who is asking, for the commits a request makes. The host is the server's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caller {
    /// The registered agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The agent's session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// One commit of a branch's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitInfo {
    /// The commit.
    pub id: String,
    /// Its root tree.
    pub tree: String,
    /// The commits it follows.
    pub parents: Vec<String>,
    /// The change it belongs to.
    pub change: String,
    /// The host that made it.
    pub host: String,
    /// The agent that made it, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The session that made it, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// Its message.
    pub message: String,
    /// The local snapshot it replaces, if its change had snapshots. Only the host that
    /// took it holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<String>,
    /// The key that signed it, in standard Base64; absent for an unsigned commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<String>,
}

/// `POST /v1/import`: commit a directory on the server's host onto a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// The directory, on the server's host.
    pub source: String,
    /// The branch to commit to.
    pub branch: String,
    /// The commit message.
    pub message: String,
    /// Who is importing.
    #[serde(default)]
    pub caller: Caller,
}

/// `POST /v1/export`: write a tree to an empty directory on the server's host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// The directory, on the server's host.
    pub target: String,
    /// What to export.
    pub from: Revision,
}

/// A branch's head, or a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Revision {
    /// The head of this branch.
    Branch(String),
    /// This commit.
    Commit(String),
}

/// A workspace and where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceInfo {
    /// The workspace.
    pub name: String,
    /// The branch it follows.
    pub branch: String,
    /// The commit its changes are relative to.
    pub base: String,
    /// The latest snapshot of its current change, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// `POST /v1/workspaces`: create a workspace following a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspaceRequest {
    /// The workspace.
    pub name: String,
    /// The branch it follows.
    pub branch: String,
}

/// What kind of entry a directory listing names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// A directory.
    Directory,
}

/// One entry of `GET /v1/workspaces/{name}/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirEntry {
    /// The entry's name.
    pub name: String,
    /// What it is.
    pub kind: EntryKind,
}

/// One uncommitted change of `GET /v1/workspaces/{name}/changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ChangeInfo {
    /// The path was written.
    Written {
        /// The path.
        path: String,
    },
    /// The path was removed.
    Removed {
        /// The path.
        path: String,
    },
}

/// `POST /v1/workspaces/{name}/commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitRequest {
    /// The commit message.
    pub message: String,
    /// Who is committing.
    #[serde(default)]
    pub caller: Caller,
}

/// `POST /v1/workspaces/{name}/snapshot`: an unsigned snapshot of the workspace for its
/// current change, kept on this host until a commit replaces it. One per tool call.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    /// What the snapshot follows, such as the tool call; `Snapshot` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Who is snapshotting.
    #[serde(default)]
    pub caller: Caller,
}

/// The result of `POST /v1/workspaces/{name}/snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase", deny_unknown_fields)]
pub enum SnapshotResponse {
    /// A snapshot was taken.
    Snapshot {
        /// The snapshot.
        snapshot: String,
    },
    /// Nothing changed since the last snapshot, or since the workspace's base.
    Unchanged,
}

/// A path both sides changed in ways the merge could not combine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictInfo {
    /// The path.
    pub path: String,
    /// `content`, `add_add`, `modify_delete`, `delete_modify`, `binary`, `mode` or `type`.
    pub kind: String,
}

/// The result of `POST /v1/workspaces/{name}/commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase", deny_unknown_fields)]
pub enum CommitResponse {
    /// The branch now points at `commit`.
    Committed {
        /// The new head.
        commit: String,
        /// Whether the branch had moved and the changes were merged onto it.
        merged: bool,
    },
    /// Nothing was committed; the workspace holds the merge with markers to resolve.
    Conflicted {
        /// Every conflict.
        conflicts: Vec<ConflictInfo>,
    },
    /// The workspace had no changes.
    Unchanged,
}

/// `POST /v1/objects/fetch`: objects by id, at most 256. The response is the objects this
/// space holds, encoded as `application/octet-stream` by `peerfs_core::encode_objects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    /// The objects wanted.
    pub ids: Vec<String>,
}

/// `POST /v1/pull`: fetch a branch and its history from another space's server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequest {
    /// The other server's socket, on this host.
    pub from: String,
    /// The branch there.
    pub branch: String,
}

/// What a pull did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullResponse {
    /// The other server's host, which names the tracking reference.
    pub remote: String,
    /// The branch pulled.
    pub branch: String,
    /// Its head, now held here as `<remote>/<branch>`.
    pub head: String,
    /// Objects fetched.
    pub objects: u64,
    /// Their uncompressed bytes.
    pub bytes: u64,
}

/// One tracking reference of `GET /v1/remotes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackingInfo {
    /// The host it came from.
    pub remote: String,
    /// The branch there.
    pub branch: String,
    /// Its head when pulled.
    pub head: String,
    /// When it was pulled, in milliseconds since the Unix epoch.
    pub pulled_ms: u64,
}

/// `POST /v1/objects/have`: which of these objects the space holds, and so everything
/// they reach. At most 256 ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaveRequest {
    /// The objects asked about.
    pub ids: Vec<String>,
}

/// The answer to [`HaveRequest`], one entry per id, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaveResponse {
    /// Whether each object is held.
    pub have: Vec<bool>,
}

/// The answer to `POST /v1/objects/receive`, whose body is objects encoded by
/// `peerfs_core::encode_objects`, each after everything it references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveResponse {
    /// How many of the objects were new.
    pub stored: u64,
}

/// `POST /v1/branches/{name}/advance`: move a branch held here, as its home host does for
/// a push. `new` must already be held, and its history must contain `expected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvanceRequest {
    /// The head the branch must still have; absent to create the branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The commit to move it to.
    pub new: String,
}

/// `POST /v1/push`: send a local revision to another server and move its branch there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushRequest {
    /// The other server's socket, on this host.
    pub to: String,
    /// The branch there.
    pub branch: String,
    /// The local branch, or `<host>/<branch>`, to send.
    pub from: String,
}

/// What a push did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushResponse {
    /// The other server's host.
    pub remote: String,
    /// The branch there.
    pub branch: String,
    /// Its new head.
    pub head: String,
    /// Objects sent.
    pub objects: u64,
    /// Their uncompressed bytes.
    pub bytes: u64,
}

/// `POST /v1/workspaces/{name}/merge`: merge a revision into the branch a workspace follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeRequest {
    /// The branch, or `<host>/<branch>`, to merge.
    pub from: String,
    /// Who is merging.
    #[serde(default)]
    pub caller: Caller,
}

/// What a merge did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum MergeResponse {
    /// The branch already contains the revision.
    UpToDate,
    /// The branch now points at the revision, which contained it.
    FastForward {
        /// The new head.
        commit: String,
    },
    /// A merge commit with both histories as parents.
    Merged {
        /// The new head.
        commit: String,
    },
    /// Nothing was committed; the workspace holds the merge with markers to resolve, and
    /// committing it records both parents.
    Conflicted {
        /// Every conflict.
        conflicts: Vec<ConflictInfo>,
    },
}
