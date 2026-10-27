//! Request and response bodies of the local API, as JSON.
//!
//! Requests reject unknown fields. Absent optional fields are omitted, never `null`.
//! Object and commit identities are lowercase hexadecimal strings.

use serde::{Deserialize, Serialize};

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    /// `invalid`, `not_found`, `conflict` or `internal`.
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
