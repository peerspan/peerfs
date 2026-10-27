//! A space: one project's objects, branches and workspaces in a directory.

use std::fs::{self, File};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::codec::len_u64;
use crate::edit::TreeEdit;
use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::merge::{Conflict, merge_trees};
use crate::object::{
    Author, ChangeId, ChunkList, Chunking, Commit, Content, Entry, FileMode, Kind, Name, Tree,
};
use crate::path::TreePath;
use crate::refs::Refs;
use crate::sign::{HostKeys, KeyFile, KnownHosts, PublicKey, Signer, sign_commit};
use crate::store::{Store, sync_dir};
use crate::sync::{Pull, Pulled, Push, RawObject, Remote, Tracking, TrackingRefs, accept_objects};
use crate::workspace::Workspace;
use std::collections::{HashSet, VecDeque};

/// The first line of a space's `FORMAT` file.
const FORMAT: &str = "peerfs space 2";

/// The space's signing key, until the host daemon signs for it.
const KEY_FILE: &str = "signing.key";

/// The keys this space trusts for other hosts, until the host daemon supplies them.
const HOSTS_FILE: &str = "hosts";

/// Files smaller than this are stored whole.
const WHOLE_FILE_LIMIT: usize = 64 * 1024;
const CHUNK_MIN: usize = 16 * 1024;
const CHUNK_AVERAGE: usize = 64 * 1024;
const CHUNK_MAX: usize = 256 * 1024;

/// The branch a new space starts with.
pub const DEFAULT_BRANCH: &str = "main";

/// One project's objects, branches and workspaces.
///
/// A process holds a space exclusively while it is open. Commits it stores are signed by
/// its [`Signer`], and commits other hosts send must be signed by a key its [`HostKeys`]
/// know for their author's host.
#[derive(Debug)]
pub struct Space {
    dir: PathBuf,
    store: Store,
    refs: Refs,
    tracking: TrackingRefs,
    signer: Option<Box<dyn Signer>>,
    host_keys: Box<dyn HostKeys>,
    _lock: File,
}

/// What committing a workspace did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitOutcome {
    /// The branch now points at `commit`. When `merged` holds, the branch had moved and the
    /// workspace's changes were merged onto its new head.
    Committed {
        /// The new head of the branch.
        commit: ObjectId,
        /// How the commit was made.
        merged: Merged,
    },
    /// The branch had moved and the changes conflict with it. Nothing was committed. The
    /// workspace now follows the new head and holds the merge, with markers in conflicted
    /// text files; resolve them and commit again.
    Conflicted(Vec<Conflict>),
    /// The workspace has no changes.
    Unchanged,
}

/// What merging a revision into a workspace did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MergeOutcome {
    /// The branch already contains the revision.
    UpToDate,
    /// The revision contains the branch, which now points at it.
    FastForward {
        /// The new head.
        commit: ObjectId,
    },
    /// The histories diverged and merged cleanly into a commit with both as parents.
    Merged {
        /// The merge commit, the branch's new head.
        commit: ObjectId,
    },
    /// The merge conflicts. Nothing was committed. The workspace holds the merge with
    /// markers; resolving and committing it makes a commit with both parents.
    Conflicted(Vec<Conflict>),
}

/// Whether a commit needed a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merged {
    /// The branch had not moved.
    No,
    /// The branch had moved and the changes merged without conflict.
    Yes,
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("space.lock")
}

impl Space {
    /// Creates a space in `dir`, which must not exist or be empty, with a `main` branch at
    /// an empty first commit, and a new signing key.
    ///
    /// # Errors
    ///
    /// Fails if `dir` holds anything, or on a file system error.
    pub fn init(dir: impl AsRef<Path>, author: &Author) -> Result<Self> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir).map_err(|e| Error::io("creating", dir, e))?;
        let mut contents = fs::read_dir(dir).map_err(|e| Error::io("listing", dir, e))?;
        if contents.next().is_some() {
            return Err(Error::NotASpace {
                path: dir.to_owned(),
                reason: "the directory is not empty".to_owned(),
            });
        }
        for sub in ["objects", "refs/branches", "workspaces"] {
            let path = dir.join(sub);
            fs::create_dir_all(&path).map_err(|e| Error::io("creating", path, e))?;
        }
        let format = dir.join("FORMAT");
        fs::write(&format, format!("{FORMAT}\n")).map_err(|e| Error::io("writing", format, e))?;
        KeyFile::create(&dir.join(KEY_FILE))?;
        sync_dir(dir)?;

        let mut space = Self::open(dir)?;
        let tree = space.put_tree(&Tree::new())?;
        let commit = space.put_commit(&Commit {
            tree,
            parents: Vec::new(),
            change: ChangeId::random(),
            author: author.clone(),
            time: SystemTime::now(),
            message: "Create the space".to_owned(),
            predecessor: None,
            signature: None,
        })?;
        space.create_branch(DEFAULT_BRANCH, commit)?;
        Ok(space)
    }

    /// Opens the space in `dir`. It signs with the key in its `signing.key` and trusts the
    /// host keys in its `hosts` file until told otherwise; without a key, it stores
    /// commits unsigned, and other hosts refuse them.
    ///
    /// # Errors
    ///
    /// Fails if `dir` is not a space of this format, if another process has it open, or on
    /// a file system error.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let format = dir.join("FORMAT");
        let found = fs::read_to_string(&format).map_err(|e| Error::NotASpace {
            path: dir.to_owned(),
            reason: format!("cannot read FORMAT: {e}"),
        })?;
        if found.lines().next() != Some(FORMAT) {
            return Err(Error::NotASpace {
                path: dir.to_owned(),
                reason: format!("unknown format {:?}", found.trim()),
            });
        }
        let lock_path = lock_path(dir);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::io("opening", &lock_path, e))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::SpaceInUse {
                    path: dir.to_owned(),
                });
            }
            Err(fs::TryLockError::Error(e)) => return Err(Error::io("locking", lock_path, e)),
        }
        let key_path = dir.join(KEY_FILE);
        let signer: Option<Box<dyn Signer>> = if key_path.exists() {
            Some(Box::new(KeyFile::load(&key_path)?))
        } else {
            None
        };
        Ok(Self {
            dir: dir.to_owned(),
            store: Store::open(&dir.join("objects"))?,
            refs: Refs::new(dir.join("refs/branches")),
            tracking: TrackingRefs::new(dir.join("refs/remotes")),
            signer,
            host_keys: Box::new(KnownHosts::new(dir.join(HOSTS_FILE))),
            _lock: lock,
        })
    }

    /// Signs commits stored from now on with `signer`, or leaves them unsigned.
    pub fn set_signer(&mut self, signer: Option<Box<dyn Signer>>) {
        self.signer = signer;
    }

    /// Checks commits other hosts send against `host_keys` from now on.
    pub fn set_host_keys(&mut self, host_keys: Box<dyn HostKeys>) {
        self.host_keys = host_keys;
    }

    /// The key this space's commits are signed with, if it signs them.
    pub fn public_key(&self) -> Option<PublicKey> {
        self.signer.as_ref().map(|signer| signer.public_key())
    }

    /// The host keys listed in the space's `hosts` file.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is malformed.
    pub fn known_hosts(&self) -> Result<Vec<(String, PublicKey)>> {
        KnownHosts::new(self.dir.join(HOSTS_FILE)).list()
    }

    /// Lists `key` in the space's `hosts` file as a key of `host`. Returns whether it was
    /// new.
    ///
    /// # Errors
    ///
    /// Fails if `host` is empty or holds whitespace, or on a file system error.
    pub fn add_known_host(&mut self, host: &str, key: &PublicKey) -> Result<bool> {
        KnownHosts::new(self.dir.join(HOSTS_FILE)).add(host, key)
    }

    /// The space's directory.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The number of objects stored.
    pub fn object_count(&self) -> usize {
        self.store.len()
    }

    /// Whether the space holds `id` and, the store being closed, everything it reaches.
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.store.contains(id)
    }

    /// An object as stored, checked against its identity, for sending to another host.
    ///
    /// # Errors
    ///
    /// Fails if the object is corrupt or cannot be read.
    pub fn raw_object(&self, id: ObjectId) -> Result<Option<RawObject>> {
        Ok(self
            .store
            .get(&id)?
            .map(|(kind, bytes)| RawObject { id, kind, bytes }))
    }

    /// Fetches `branch` and all its history from the host `from` through `remote`, and
    /// records its head as the tracking reference `<from>/<branch>`.
    ///
    /// Only objects this space lacks are fetched. Every object is checked against its
    /// identity and expected kind before it is stored. A pull that fails part way changes
    /// no reference, and the next pull fetches what is left.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached, lacks the branch or an object of its
    /// history, or sends an object that does not match.
    pub fn pull(&mut self, remote: &mut dyn Remote, from: &str, branch: &str) -> Result<Pulled> {
        let head = remote
            .branch(branch)?
            .ok_or_else(|| Error::NoSuchBranch(format!("{from}/{branch}")))?;
        let mut pull = self.start_pull(from, branch, head)?;
        loop {
            let wanted = pull.wanted();
            if wanted.is_empty() {
                return self.finish_pull(pull);
            }
            let objects = remote.fetch(&wanted)?;
            self.receive(&mut pull, objects)?;
        }
    }

    /// Starts pulling `head`, the head of `branch` on the host `from`, step by step; see
    /// [`Pull`]. [`Space::pull`] does every step for a [`Remote`].
    ///
    /// # Errors
    ///
    /// Fails if a name is invalid.
    pub fn start_pull(&self, from: &str, branch: &str, head: ObjectId) -> Result<Pull> {
        Ok(Pull::new(
            &self.store,
            Name::try_from(from)?,
            branch_name(branch)?,
            head,
        ))
    }

    /// Takes in the objects fetched for [`Pull::wanted`], checking each against its identity
    /// and expected kind.
    ///
    /// # Errors
    ///
    /// Fails if a wanted object is missing or does not match, or on a store error.
    pub fn receive(&mut self, pull: &mut Pull, objects: Vec<RawObject>) -> Result<()> {
        pull.receive(&mut self.store, &*self.host_keys, objects)
    }

    /// Completes a pull once nothing is wanted: stores what was held back, makes it
    /// durable, and records the tracking reference.
    ///
    /// # Errors
    ///
    /// Fails if objects are still wanted, or on a store error.
    pub fn finish_pull(&mut self, pull: Pull) -> Result<Pulled> {
        pull.finish(&mut self.store, &self.tracking)
    }

    /// Which of `ids` this space holds; the store being closed, holding one means holding
    /// everything it reaches.
    pub fn have(&self, ids: &[ObjectId]) -> Vec<bool> {
        ids.iter().map(|id| self.store.contains(id)).collect()
    }

    /// Starts pushing the commit `head` to a host that may lack some of its history; see
    /// [`Push`].
    ///
    /// # Errors
    ///
    /// Fails if `head` is not a commit this space holds.
    pub fn start_push(&self, head: ObjectId) -> Result<Push> {
        self.commit(head)?;
        Ok(Push::new(head))
    }

    /// Takes the receiver's answers for [`Push::to_ask`], `held[i]` for the `i`th object.
    ///
    /// # Errors
    ///
    /// Fails if the answers do not match the question, or on a store error.
    pub fn answer_push(&self, push: &mut Push, held: &[bool]) -> Result<()> {
        push.answer(&self.store, held)
    }

    /// The objects the receiver lacks, each after everything it references: the order to
    /// send them in.
    ///
    /// # Errors
    ///
    /// Fails if objects are still to be asked about.
    pub fn push_order(&self, push: &Push) -> Result<Vec<ObjectId>> {
        push.order()
    }

    /// Stores objects another host sent, in order, and makes them durable. An object that
    /// does not match its identity, or references one this space does not hold, is refused,
    /// so the store stays closed. Returns how many objects were new.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::RemoteObject`] at the first object refused; those before it are
    /// kept.
    pub fn accept_objects(&mut self, objects: &[RawObject]) -> Result<u64> {
        accept_objects(&mut self.store, &*self.host_keys, objects)
    }

    /// Moves `branch` from `expected` to `new` (`None`: creates it), as the branch's home
    /// host does for a push. `new` must be a commit held here whose history contains
    /// `expected`, so a push never drops commits it did not see.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotFastForward`] when `new` does not contain `expected`, and
    /// [`Error::BranchMoved`] when the branch no longer points at `expected`.
    pub fn advance_branch(
        &mut self,
        branch: &str,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<()> {
        let name = branch_name(branch)?;
        if self.store.kind_of(&new) != Some(Kind::Commit) {
            return Err(Error::MissingObject {
                kind: Kind::Commit,
                id: new,
            });
        }
        if let Some(expected) = expected
            && !self.is_ancestor(expected, new)?
        {
            return Err(Error::NotFastForward {
                branch: branch.to_owned(),
                new,
            });
        }
        self.store.sync()?;
        self.refs.compare_and_swap(&name, expected, new)
    }

    /// Records `head` as the latest known head of `branch` on the host `remote`, as after a
    /// push this space made there.
    ///
    /// # Errors
    ///
    /// Fails if a name is invalid, `head` is not held here, or on a file system error.
    pub fn record_tracking(&mut self, remote: &str, branch: &str, head: ObjectId) -> Result<()> {
        self.commit(head)?;
        self.tracking
            .set(&Name::try_from(remote)?, &branch_name(branch)?, head)
    }

    /// Whether `ancestor` is `descendant` or in its history.
    ///
    /// # Errors
    ///
    /// Fails if a commit in the history is missing or corrupt.
    pub fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        let mut queue = VecDeque::from([descendant]);
        let mut seen = HashSet::from([descendant]);
        while let Some(id) = queue.pop_front() {
            if id == ancestor {
                return Ok(true);
            }
            for parent in self.commit(id)?.parents {
                if seen.insert(parent) {
                    queue.push_back(parent);
                }
            }
        }
        Ok(false)
    }

    /// A common ancestor of `a` and `b` nearest to `b`, if their histories meet.
    ///
    /// # Errors
    ///
    /// Fails if a commit in either history is missing or corrupt.
    pub fn merge_base(&self, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
        let mut ancestors = HashSet::from([a]);
        let mut queue = VecDeque::from([a]);
        while let Some(id) = queue.pop_front() {
            for parent in self.commit(id)?.parents {
                if ancestors.insert(parent) {
                    queue.push_back(parent);
                }
            }
        }
        let mut queue = VecDeque::from([b]);
        let mut seen = HashSet::from([b]);
        while let Some(id) = queue.pop_front() {
            if ancestors.contains(&id) {
                return Ok(Some(id));
            }
            for parent in self.commit(id)?.parents {
                if seen.insert(parent) {
                    queue.push_back(parent);
                }
            }
        }
        Ok(None)
    }

    /// Merges `revision` (a branch, or `<remote>/<branch>`) into the branch `workspace`
    /// follows. The workspace must have no uncommitted changes.
    ///
    /// # Errors
    ///
    /// Fails if the revision does not exist, the workspace is not clean, or the branch
    /// moves meanwhile.
    pub fn merge_into_workspace(
        &mut self,
        workspace: &mut Workspace,
        revision: &str,
        author: &Author,
    ) -> Result<MergeOutcome> {
        let theirs = self
            .resolve(revision)?
            .ok_or_else(|| Error::NoSuchBranch(revision.to_owned()))?;
        let theirs_tree = self.commit(theirs)?.tree;
        if !workspace.changes()?.is_empty() || workspace.merging().is_some() {
            return Err(Error::WorkspaceNotClean(workspace.name().to_owned()));
        }
        let branch = workspace.branch().to_owned();
        let head = self
            .branch(&branch)?
            .ok_or_else(|| Error::NoSuchBranch(branch.clone()))?;
        if workspace.base() != head {
            workspace.reset(head)?;
        }
        if self.is_ancestor(theirs, head)? {
            return Ok(MergeOutcome::UpToDate);
        }
        if self.is_ancestor(head, theirs)? {
            self.update_branch(&branch, Some(head), theirs)?;
            workspace.reset(theirs)?;
            return Ok(MergeOutcome::FastForward { commit: theirs });
        }
        let head_tree = self.commit(head)?.tree;
        let base_tree = match self.merge_base(head, theirs)? {
            Some(base) => Some(self.commit(base)?.tree),
            None => None,
        };
        let (tree, conflicts) = merge_trees(self, base_tree, head_tree, theirs_tree)?;
        if !conflicts.is_empty() {
            workspace.rebase(self, head, head_tree, tree)?;
            workspace.set_merging(Some(theirs))?;
            return Ok(MergeOutcome::Conflicted(conflicts));
        }
        let commit = self.put_commit(&Commit {
            tree,
            parents: vec![head, theirs],
            change: workspace.change(),
            author: author.clone(),
            time: SystemTime::now(),
            message: format!("Merge {revision}"),
            predecessor: None,
            signature: None,
        })?;
        self.update_branch(&branch, Some(head), commit)?;
        workspace.finish_change(commit)?;
        Ok(MergeOutcome::Merged { commit })
    }

    /// Every tracking reference: other hosts' branch heads as of their last pull.
    ///
    /// # Errors
    ///
    /// Fails if a reference cannot be read.
    pub fn tracking(&self) -> Result<Vec<Tracking>> {
        self.tracking.list()
    }

    /// The commit `revision` names: a local branch, or `<remote>/<branch>` for the head
    /// last pulled from another host.
    ///
    /// # Errors
    ///
    /// Fails if a name is invalid or a reference cannot be read.
    pub fn resolve(&self, revision: &str) -> Result<Option<ObjectId>> {
        match revision.split_once('/') {
            Some((remote, branch)) => Ok(self
                .tracking
                .get(&Name::try_from(remote)?, &branch_name(branch)?)?
                .map(|tracking| tracking.head)),
            None => self.branch(revision),
        }
    }

    /// Makes every object written so far durable on this host.
    ///
    /// # Errors
    ///
    /// Fails on a file system error.
    pub fn sync(&mut self) -> Result<()> {
        self.store.sync()
    }

    fn object(&self, id: ObjectId, expected: Kind) -> Result<Vec<u8>> {
        let (found, bytes) = self
            .store
            .get(&id)?
            .ok_or(Error::MissingObject { kind: expected, id })?;
        if found != expected {
            return Err(Error::WrongKind {
                id,
                expected,
                found,
            });
        }
        Ok(bytes)
    }

    /// Stores a directory listing.
    ///
    /// # Errors
    ///
    /// Fails on a file system error.
    pub fn put_tree(&mut self, tree: &Tree) -> Result<ObjectId> {
        self.store.put(Kind::Tree, &tree.encode())
    }

    /// Reads a directory listing.
    ///
    /// # Errors
    ///
    /// Fails if the tree is missing, corrupt or not a tree.
    pub fn tree(&self, id: ObjectId) -> Result<Tree> {
        Tree::decode(&self.object(id, Kind::Tree)?).map_err(|source| Error::Decode {
            kind: Kind::Tree,
            id,
            source,
        })
    }

    /// Stores a commit, signed by the space's signer if it has one. Its tree and parents
    /// should be stored first.
    ///
    /// # Errors
    ///
    /// Fails if the signer fails, or on a file system error.
    pub fn put_commit(&mut self, commit: &Commit) -> Result<ObjectId> {
        match &self.signer {
            Some(signer) => {
                let commit = sign_commit(commit, signer.as_ref())?;
                self.store.put(Kind::Commit, &commit.encode())
            }
            None => self.store.put(Kind::Commit, &commit.encode()),
        }
    }

    /// Reads a commit.
    ///
    /// # Errors
    ///
    /// Fails if the commit is missing, corrupt or not a commit.
    pub fn commit(&self, id: ObjectId) -> Result<Commit> {
        Commit::decode(&self.object(id, Kind::Commit)?).map_err(|source| Error::Decode {
            kind: Kind::Commit,
            id,
            source,
        })
    }

    /// Stores file bytes: whole below 64 KiB, as content-defined chunks above.
    ///
    /// # Errors
    ///
    /// Fails on a file system error.
    pub fn write_content(&mut self, bytes: &[u8]) -> Result<Content> {
        if bytes.is_empty() {
            return Ok(Content::Empty);
        }
        if bytes.len() < WHOLE_FILE_LIMIT {
            return Ok(Content::Blob(self.store.put(Kind::Blob, bytes)?));
        }
        let chunker = fastcdc::v2020::FastCDC::new(bytes, CHUNK_MIN, CHUNK_AVERAGE, CHUNK_MAX);
        let mut chunks = Vec::new();
        for chunk in chunker {
            let piece = &bytes[chunk.offset..chunk.offset + chunk.length];
            chunks.push(self.store.put(Kind::Blob, piece)?);
        }
        let list = ChunkList {
            chunking: Chunking::FastCdcV1,
            size: len_u64(bytes.len()),
            chunks,
        };
        Ok(Content::Chunked(
            self.store.put(Kind::File, &list.encode())?,
        ))
    }

    /// Reads file bytes back.
    ///
    /// # Errors
    ///
    /// Fails if an object is missing or corrupt, or the chunks do not add up to the size.
    pub fn read_content(&self, content: &Content) -> Result<Vec<u8>> {
        match *content {
            Content::Empty => Ok(Vec::new()),
            Content::Blob(id) => self.object(id, Kind::Blob),
            Content::Chunked(id) => {
                let list = ChunkList::decode(&self.object(id, Kind::File)?).map_err(|source| {
                    Error::Decode {
                        kind: Kind::File,
                        id,
                        source,
                    }
                })?;
                let mut bytes = Vec::new();
                for chunk in &list.chunks {
                    bytes.extend_from_slice(&self.object(*chunk, Kind::Blob)?);
                }
                if len_u64(bytes.len()) != list.size {
                    return Err(Error::Corrupt(format!(
                        "file {id} is {} bytes, not {}",
                        bytes.len(),
                        list.size
                    )));
                }
                Ok(bytes)
            }
        }
    }

    /// The entry at `path` in the tree `root`, if any.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way is missing or corrupt.
    pub fn entry(&self, root: ObjectId, path: &TreePath) -> Result<Option<Entry>> {
        let mut entry = Entry::Tree(root);
        for name in path.components() {
            let Entry::Tree(id) = entry else {
                return Ok(None);
            };
            let Some(next) = self.tree(id)?.get(name).cloned() else {
                return Ok(None);
            };
            entry = next;
        }
        Ok(Some(entry))
    }

    /// The head of `branch`, if it exists.
    ///
    /// # Errors
    ///
    /// Fails if the name is invalid or the reference cannot be read.
    pub fn branch(&self, branch: &str) -> Result<Option<ObjectId>> {
        self.refs.get(&branch_name(branch)?)
    }

    /// Every branch and its head, by name.
    ///
    /// # Errors
    ///
    /// Fails if a reference cannot be read.
    pub fn branches(&self) -> Result<Vec<(String, ObjectId)>> {
        Ok(self
            .refs
            .list()?
            .into_iter()
            .map(|(name, head)| (name.to_string(), head))
            .collect())
    }

    /// Creates `branch` at `commit`. Forking copies no data.
    ///
    /// # Errors
    ///
    /// Fails if the branch exists, the commit is missing, or on a file system error.
    pub fn create_branch(&mut self, branch: &str, commit: ObjectId) -> Result<()> {
        let name = branch_name(branch)?;
        self.commit(commit)?;
        self.store.sync()?;
        match self.refs.compare_and_swap(&name, None, commit) {
            Err(Error::BranchMoved { .. }) => Err(Error::BranchExists(branch.to_owned())),
            other => other,
        }
    }

    /// Points `branch` at `new` if it still points at `expected`, after making every
    /// object written so far durable.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::BranchMoved`] if the branch no longer points at `expected`.
    pub fn update_branch(
        &mut self,
        branch: &str,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<()> {
        let name = branch_name(branch)?;
        self.store.sync()?;
        self.refs.compare_and_swap(&name, expected, new)
    }

    /// Commits `tree` onto `branch` as a child of its head.
    ///
    /// # Errors
    ///
    /// Fails if the branch does not exist or moves meanwhile.
    pub fn commit_tree(
        &mut self,
        branch: &str,
        tree: ObjectId,
        message: &str,
        author: &Author,
    ) -> Result<ObjectId> {
        let head = self
            .branch(branch)?
            .ok_or_else(|| Error::NoSuchBranch(branch.to_owned()))?;
        let commit = self.put_commit(&Commit {
            tree,
            parents: vec![head],
            change: ChangeId::random(),
            author: author.clone(),
            time: SystemTime::now(),
            message: message.to_owned(),
            predecessor: None,
            signature: None,
        })?;
        self.update_branch(branch, Some(head), commit)?;
        Ok(commit)
    }

    /// The commits from `start` back along first parents.
    ///
    /// # Errors
    ///
    /// Fails if a commit is missing or corrupt.
    pub fn history(&self, start: ObjectId) -> Result<Vec<(ObjectId, Commit)>> {
        let mut history = Vec::new();
        let mut next = Some(start);
        while let Some(id) = next {
            let commit = self.commit(id)?;
            next = commit.parents.first().copied();
            history.push((id, commit));
        }
        Ok(history)
    }

    /// Stores the directory `source` and everything under it, and returns its tree.
    ///
    /// # Errors
    ///
    /// Fails on a name that is not UTF-8, a special file (a socket, FIFO or device), or a
    /// file system error.
    pub fn import(&mut self, source: impl AsRef<Path>) -> Result<ObjectId> {
        let source = source.as_ref();
        let mut tree = Tree::new();
        let entries = fs::read_dir(source).map_err(|e| Error::io("listing", source, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io("listing", source, e))?;
            let path = entry.path();
            let name =
                entry
                    .file_name()
                    .to_str()
                    .map(Name::try_from)
                    .ok_or(Error::Unsupported {
                        path: path.clone(),
                        reason: "the name is not UTF-8",
                    })??;
            let meta = fs::symlink_metadata(&path).map_err(|e| Error::io("reading", &path, e))?;
            let kind = meta.file_type();
            let entry = if kind.is_dir() {
                Entry::Tree(self.import(&path)?)
            } else if kind.is_symlink() {
                let target = fs::read_link(&path).map_err(|e| Error::io("reading", &path, e))?;
                let target = target.to_str().ok_or(Error::Unsupported {
                    path: path.clone(),
                    reason: "the link target is not UTF-8",
                })?;
                Entry::Symlink(target.to_owned())
            } else if kind.is_file() {
                let bytes = fs::read(&path).map_err(|e| Error::io("reading", &path, e))?;
                Entry::File {
                    content: self.write_content(&bytes)?,
                    mode: mode_of(meta.permissions().mode()),
                }
            } else {
                return Err(Error::Unsupported {
                    path,
                    reason: "it is not a file, directory or symbolic link",
                });
            };
            tree.insert(name, entry);
        }
        self.put_tree(&tree)
    }

    /// Writes the tree `root` out to `target`, which must not exist or be empty.
    ///
    /// # Errors
    ///
    /// Fails if `target` holds anything, an object is missing, or on a file system error.
    pub fn export(&self, root: ObjectId, target: impl AsRef<Path>) -> Result<()> {
        let target = target.as_ref();
        fs::create_dir_all(target).map_err(|e| Error::io("creating", target, e))?;
        let mut contents = fs::read_dir(target).map_err(|e| Error::io("listing", target, e))?;
        if contents.next().is_some() {
            return Err(Error::Unsupported {
                path: target.to_owned(),
                reason: "the directory is not empty",
            });
        }
        self.export_into(root, target)
    }

    fn export_into(&self, root: ObjectId, target: &Path) -> Result<()> {
        for (name, entry) in self.tree(root)?.iter() {
            let path = target.join(name.as_str());
            match entry {
                Entry::Tree(id) => {
                    fs::create_dir(&path).map_err(|e| Error::io("creating", &path, e))?;
                    self.export_into(*id, &path)?;
                }
                Entry::Symlink(link) => {
                    symlink(link, &path).map_err(|e| Error::io("linking", &path, e))?;
                }
                Entry::File { content, mode } => {
                    fs::write(&path, self.read_content(content)?)
                        .map_err(|e| Error::io("writing", &path, e))?;
                    fs::set_permissions(&path, fs::Permissions::from_mode(permissions(*mode)))
                        .map_err(|e| Error::io("setting the mode of", &path, e))?;
                }
            }
        }
        Ok(())
    }

    fn workspace_dir(&self, name: &Name) -> PathBuf {
        self.dir.join("workspaces").join(name.as_str())
    }

    /// Creates a workspace that follows `branch`, starting at its head.
    ///
    /// # Errors
    ///
    /// Fails if the workspace exists, the branch does not, or on a file system error.
    pub fn create_workspace(&mut self, workspace: &str, branch: &str) -> Result<Workspace> {
        let name = Name::try_from(workspace)?;
        let head = self
            .branch(branch)?
            .ok_or_else(|| Error::NoSuchBranch(branch.to_owned()))?;
        let dir = self.workspace_dir(&name);
        if dir.exists() {
            return Err(Error::WorkspaceExists(workspace.to_owned()));
        }
        Workspace::create(dir, name, branch_name(branch)?, head)
    }

    /// Opens an existing workspace.
    ///
    /// # Errors
    ///
    /// Fails if there is no such workspace or its state cannot be read.
    pub fn workspace(&self, workspace: &str) -> Result<Workspace> {
        let name = Name::try_from(workspace)?;
        let dir = self.workspace_dir(&name);
        if !dir.exists() {
            return Err(Error::NoSuchWorkspace(workspace.to_owned()));
        }
        Workspace::open(dir, name)
    }

    /// The names of every workspace.
    ///
    /// # Errors
    ///
    /// Fails on a file system error.
    pub fn workspaces(&self) -> Result<Vec<String>> {
        let dir = self.dir.join("workspaces");
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| Error::io("listing", &dir, e))? {
            let entry = entry.map_err(|e| Error::io("listing", &dir, e))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Deletes a workspace and its uncommitted changes. History is untouched.
    ///
    /// # Errors
    ///
    /// Fails if there is no such workspace or on a file system error.
    pub fn discard_workspace(&mut self, workspace: &str) -> Result<()> {
        let dir = self.workspace(workspace)?.dir().to_owned();
        fs::remove_dir_all(&dir).map_err(|e| Error::io("removing", dir, e))
    }

    /// Takes an unsigned snapshot of the workspace, for the current change: a commit of its
    /// tree whose parent is the change's previous snapshot, or the workspace's base. The
    /// branch and the overlay are untouched, and the snapshot stays on this host. Returns
    /// `None` when nothing changed since the previous snapshot, or since the base.
    ///
    /// Meant to be called once per agent tool call; committing the workspace then replaces
    /// the change's snapshots with one signed commit.
    ///
    /// # Errors
    ///
    /// Fails on a store or file system error.
    pub fn snapshot_workspace(
        &mut self,
        workspace: &mut Workspace,
        message: &str,
        author: &Author,
    ) -> Result<Option<ObjectId>> {
        let base_tree = self.commit(workspace.base())?.tree;
        let tree = workspace.apply(self, base_tree)?;
        let previous = workspace.snapshot();
        let previous_tree = match previous {
            Some(snapshot) => self.commit(snapshot)?.tree,
            None => base_tree,
        };
        if tree == previous_tree {
            return Ok(None);
        }
        let snapshot = Commit {
            tree,
            parents: vec![previous.unwrap_or(workspace.base())],
            change: workspace.change(),
            author: author.clone(),
            time: SystemTime::now(),
            message: message.to_owned(),
            predecessor: None,
            signature: None,
        };
        let id = self.store.put(Kind::Commit, &snapshot.encode())?;
        // The workspace's state names the snapshot only once the snapshot is durable.
        self.store.sync()?;
        workspace.set_snapshot(id)?;
        Ok(Some(id))
    }

    /// The current change's snapshots, newest first.
    ///
    /// # Errors
    ///
    /// Fails if a snapshot is missing or corrupt.
    pub fn snapshots(&self, workspace: &Workspace) -> Result<Vec<(ObjectId, Commit)>> {
        let mut snapshots = Vec::new();
        let mut next = workspace.snapshot();
        while let Some(id) = next {
            let commit = self.commit(id)?;
            if commit.signature.is_some() || commit.change != workspace.change() {
                break;
            }
            next = commit.parents.first().copied();
            snapshots.push((id, commit));
        }
        Ok(snapshots)
    }

    /// Commits the workspace's changes to its branch as one signed commit, ending the
    /// change. When the change has snapshots, a last one is taken and the commit names it
    /// as its predecessor. When the branch has moved, the changes are merged onto its head
    /// first; on a conflict nothing is committed, the change and its snapshots continue,
    /// and the workspace holds the merge to resolve.
    ///
    /// # Errors
    ///
    /// Fails if the branch is gone, or on a store or file system error.
    pub fn commit_workspace(
        &mut self,
        workspace: &mut Workspace,
        message: &str,
        author: &Author,
    ) -> Result<CommitOutcome> {
        let branch = workspace.branch().to_string();
        let head = self
            .branch(&branch)?
            .ok_or_else(|| Error::NoSuchBranch(branch.clone()))?;
        let base_commit = workspace.base();
        let base_tree = self.commit(base_commit)?.tree;
        let ours = workspace.apply(self, base_tree)?;
        let merging = workspace.merging();
        if ours == base_tree && merging.is_none() {
            if head != base_commit {
                workspace.reset(head)?;
            }
            return Ok(CommitOutcome::Unchanged);
        }
        if workspace.snapshot().is_some() {
            self.snapshot_workspace(workspace, message, author)?;
        }
        let (tree, merged) = if head == base_commit {
            (ours, Merged::No)
        } else {
            let head_tree = self.commit(head)?.tree;
            let (tree, conflicts) = merge_trees(self, Some(base_tree), ours, head_tree)?;
            if !conflicts.is_empty() {
                workspace.rebase(self, head, head_tree, tree)?;
                workspace.set_merging(merging)?;
                return Ok(CommitOutcome::Conflicted(conflicts));
            }
            (tree, Merged::Yes)
        };
        let mut parents = vec![head];
        parents.extend(merging.filter(|other| *other != head));
        let commit = self.put_commit(&Commit {
            tree,
            parents,
            change: workspace.change(),
            author: author.clone(),
            time: SystemTime::now(),
            message: message.to_owned(),
            predecessor: workspace.snapshot(),
            signature: None,
        })?;
        self.update_branch(&branch, Some(head), commit)?;
        workspace.finish_change(commit)?;
        Ok(CommitOutcome::Committed { commit, merged })
    }

    pub(crate) fn edit(&self, root: ObjectId) -> Result<TreeEdit> {
        TreeEdit::new(self, root)
    }
}

pub(crate) fn mode_of(permissions: u32) -> FileMode {
    if permissions & 0o111 == 0 {
        FileMode::Regular
    } else {
        FileMode::Executable
    }
}

pub(crate) fn permissions(mode: FileMode) -> u32 {
    match mode {
        FileMode::Regular => 0o644,
        FileMode::Executable => 0o755,
    }
}

/// A branch name: a valid [`Name`] that does not start with `.`, which marks the
/// temporary files of reference updates.
fn branch_name(branch: &str) -> Result<Name> {
    if branch.starts_with('.') {
        return Err(Error::InvalidPath(branch.to_owned()));
    }
    Ok(Name::try_from(branch)?)
}
