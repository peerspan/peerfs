//! The peerfs local API: its JSON bodies and a client over the server's Unix socket.
//!
//! A `peerfs` server owns one space and serves it at `<space>/peerfs.sock` with HTTP/1.1,
//! following the Peerspan host daemon's `admin.sock` conventions: JSON bodies under `/v1`,
//! errors as [`ErrorBody`] with codes `invalid` (400), `not_found` (404), `conflict` (409)
//! and `internal` (500), unknown fields rejected. File contents are raw request and
//! response bodies.

mod client;
mod wire;

pub use client::{Client, ClientError, RawResponse};
pub use hyper::Method;
pub use wire::{
    BranchInfo, Caller, ChangeInfo, CommitInfo, CommitRequest, CommitResponse, ConflictInfo,
    CreateWorkspaceRequest, DirEntry, EntryKind, ErrorBody, ExportRequest, FetchRequest,
    ForkRequest, ImportRequest, PullRequest, PullResponse, Revision, SpaceInfo, TrackingInfo,
    WorkspaceInfo,
};

/// The socket's file name inside the space directory.
pub const SOCKET_NAME: &str = "peerfs.sock";
