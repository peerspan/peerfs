//! The core of peerfs: content-addressed history, branches and workspaces on one host.
//!
//! A [`Space`] holds one project. Its files are immutable objects named by hash: blobs,
//! chunked files, trees and commits. Branches name commits and move only by
//! compare-and-swap. A [`Workspace`] is a writable overlay over a commit; committing it
//! merges onto the branch's head when the branch has moved.
//!
//! # Examples
//!
//! ```
//! use peerfs_core::{Author, CommitOutcome, Space};
//!
//! # fn main() -> Result<(), peerfs_core::Error> {
//! let dir = tempfile::tempdir().expect("temporary directory");
//! let author = Author { host: "laptop".to_owned(), ..Author::default() };
//! let mut space = Space::init(dir.path().join("space"), &author)?;
//! let mut workspace = space.create_workspace("agent-1", "main")?;
//! workspace.write(&space, "src/lib.rs", b"pub fn answer() -> u32 { 42 }\n")?;
//! let outcome = space.commit_workspace(&mut workspace, "Add the answer", &author)?;
//! assert!(matches!(outcome, CommitOutcome::Committed { .. }));
//! assert_eq!(workspace.read(&space, "src/lib.rs")?, b"pub fn answer() -> u32 { 42 }\n");
//! # Ok(())
//! # }
//! ```

mod codec;
mod edit;
mod error;
mod id;
mod merge;
mod object;
mod path;
mod refs;
mod sign;
mod space;
mod store;
mod sync;
mod workspace;

pub use codec::DecodeError;
pub use error::{Error, Result};
pub use id::{Algorithm, DIGEST_LEN, ENCODED_LEN, ObjectId, ParseIdError};
pub use merge::{Conflict, ConflictKind};
pub use object::{
    Author, ChangeId, Commit, Content, Entry, FileMode, InvalidNameError, Kind, Name, Tree,
};
pub use path::TreePath;
pub use sign::{
    HostKeys, KeyFile, KnownHosts, PUBLIC_KEY_LEN, ParseKeyError, PublicKey, SIGNATURE_LEN,
    SIGNING_CONTEXT, Signature, Signer,
};
pub use space::{CommitOutcome, DEFAULT_BRANCH, MergeOutcome, Merged, Space};
pub use sync::{
    FETCH_BATCH, Pull, Pulled, Push, RawObject, Remote, Tracking, decode_objects, encode_objects,
};
pub use workspace::{Change, EntryKind, Workspace};
