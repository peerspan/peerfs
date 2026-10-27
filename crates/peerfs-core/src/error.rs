//! The error type of space operations.

use std::io;
use std::path::PathBuf;

use crate::codec::DecodeError;
use crate::id::ObjectId;
use crate::object::{InvalidNameError, Kind};
use crate::sign::PublicKey;

/// A space operation that failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file system operation failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The directory is not a space, or one of a format this version does not read.
    #[error("{} is not a peerfs space: {reason}", path.display())]
    NotASpace {
        /// The directory.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },
    /// Another process holds the space.
    #[error("{} is in use by another process", path.display())]
    SpaceInUse {
        /// The space directory.
        path: PathBuf,
    },
    /// Stored data does not match its identity or cannot be read.
    #[error("corrupt store: {0}")]
    Corrupt(String),
    /// An object's bytes could not be decoded.
    #[error("cannot decode {kind} {id}: {source}")]
    Decode {
        /// The object's kind.
        kind: Kind,
        /// The object.
        id: ObjectId,
        /// What was wrong with it.
        source: DecodeError,
    },
    /// An object is not in the store.
    #[error("{kind} {id} is not in the store")]
    MissingObject {
        /// The kind expected.
        kind: Kind,
        /// The object.
        id: ObjectId,
    },
    /// An object has a different kind than expected.
    #[error("object {id} is a {found}, not a {expected}")]
    WrongKind {
        /// The object.
        id: ObjectId,
        /// The kind expected.
        expected: Kind,
        /// The kind stored.
        found: Kind,
    },
    /// A name of a branch or workspace, or a path component, is not allowed.
    #[error(transparent)]
    InvalidName(#[from] InvalidNameError),
    /// A path is not a relative path of valid names.
    #[error("invalid path {0:?}")]
    InvalidPath(String),
    /// No branch has this name.
    #[error("no branch {0}")]
    NoSuchBranch(String),
    /// A branch with this name exists already.
    #[error("branch {0} exists already")]
    BranchExists(String),
    /// No workspace has this name.
    #[error("no workspace {0}")]
    NoSuchWorkspace(String),
    /// A workspace with this name exists already.
    #[error("workspace {0} exists already")]
    WorkspaceExists(String),
    /// A branch moved between reading it and updating it.
    #[error(
        "branch {branch} moved: expected {}, found {}",
        show_head(expected.as_ref()),
        show_head(found.as_ref())
    )]
    BranchMoved {
        /// The branch.
        branch: String,
        /// The commit the update was based on.
        expected: Option<ObjectId>,
        /// The commit the branch points at.
        found: Option<ObjectId>,
    },
    /// Nothing exists at a path.
    #[error("no such file or directory: {0}")]
    NotFound(String),
    /// A path names a directory or link where a file was expected, or the reverse.
    #[error("{path} is a {found}")]
    WrongEntry {
        /// The path.
        path: String,
        /// What it is.
        found: &'static str,
    },
    /// The source tree holds something peerfs cannot store.
    #[error("cannot store {}: {reason}", path.display())]
    Unsupported {
        /// The path.
        path: PathBuf,
        /// Why.
        reason: &'static str,
    },
    /// A workspace has uncommitted changes or an unfinished merge, so it cannot merge.
    #[error("workspace {0} has uncommitted changes or an unfinished merge")]
    WorkspaceNotClean(String),
    /// A branch would move to a commit whose history does not hold its current head.
    #[error("branch {branch} cannot move to {new}: it does not contain the branch's head")]
    NotFastForward {
        /// The branch.
        branch: String,
        /// The commit it would move to.
        new: ObjectId,
    },
    /// Another host could not be reached or answered something unusable.
    #[error("remote: {0}")]
    Remote(String),
    /// An object another host sent, or failed to send, cannot be accepted.
    #[error("remote object {id} {problem}")]
    RemoteObject {
        /// The object.
        id: ObjectId,
        /// What was wrong.
        problem: &'static str,
    },
    /// A commit another host sent is signed by a key not known to belong to its author's
    /// host.
    #[error("commit {commit} is signed by {key}, which is not a known key of host {host}")]
    UnknownHostKey {
        /// The commit.
        commit: ObjectId,
        /// The host it names as its author.
        host: String,
        /// The key that signed it.
        key: PublicKey,
    },
    /// A commit could not be signed.
    #[error("signing: {0}")]
    Signing(String),
    /// Something exists at a path already.
    #[error("{0} exists already")]
    Exists(String),
    /// A directory still has entries.
    #[error("directory {0} is not empty")]
    NotEmpty(String),
    /// A directory holding committed content cannot be renamed in place; copy it and
    /// remove the original instead, as overlay file systems require.
    #[error("cannot rename directory {0}: it holds committed content")]
    DirectoryRename(String),
    /// A host name is empty or holds whitespace.
    #[error("invalid host {0:?}")]
    InvalidHost(String),
    /// No snapshot of a workspace's current change starts with what was given.
    #[error("workspace {workspace} has no snapshot {snapshot} in its current change")]
    NoSuchSnapshot {
        /// The workspace.
        workspace: String,
        /// What was given: a snapshot or the start of one.
        snapshot: String,
    },
    /// More than one snapshot starts with what was given.
    #[error("{0} is the start of more than one snapshot; give more of it")]
    AmbiguousSnapshot(String),
    /// A workspace moved onto a new base during its change, by a conflicted commit or a
    /// merge, or holds a merge to resolve. Its earlier snapshots were taken over the old
    /// base, so restoring one would undo what the new base brought.
    #[error(
        "workspace {0} moved onto a new base during this change, by a conflicted commit or a \
         merge; restoring an earlier snapshot would undo that, so resolve and commit first"
    )]
    BaseMoved(String),
}

impl Error {
    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// A branch head for a message: the id, or `none` for a branch that does not exist.
fn show_head(head: Option<&ObjectId>) -> String {
    head.map_or_else(|| "none".to_owned(), ToString::to_string)
}

/// The result of a space operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;
