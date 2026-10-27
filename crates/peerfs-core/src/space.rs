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
use crate::store::{Store, sync_dir};
use crate::workspace::Workspace;

/// The first line of a space's `FORMAT` file.
const FORMAT: &str = "peerfs space 1";

/// Files smaller than this are stored whole.
const WHOLE_FILE_LIMIT: usize = 64 * 1024;
const CHUNK_MIN: usize = 16 * 1024;
const CHUNK_AVERAGE: usize = 64 * 1024;
const CHUNK_MAX: usize = 256 * 1024;

/// The branch a new space starts with.
pub const DEFAULT_BRANCH: &str = "main";

/// One project's objects, branches and workspaces.
///
/// A process holds a space exclusively while it is open.
#[derive(Debug)]
pub struct Space {
    dir: PathBuf,
    store: Store,
    refs: Refs,
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
    /// an empty first commit.
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
        })?;
        space.create_branch(DEFAULT_BRANCH, commit)?;
        Ok(space)
    }

    /// Opens the space in `dir`.
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
        Ok(Self {
            dir: dir.to_owned(),
            store: Store::open(&dir.join("objects"))?,
            refs: Refs::new(dir.join("refs/branches")),
            _lock: lock,
        })
    }

    /// The space's directory.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The number of objects stored.
    pub fn object_count(&self) -> usize {
        self.store.len()
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

    /// Stores a commit. Its tree and parents should be stored first.
    ///
    /// # Errors
    ///
    /// Fails on a file system error.
    pub fn put_commit(&mut self, commit: &Commit) -> Result<ObjectId> {
        self.store.put(Kind::Commit, &commit.encode())
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

    /// Commits a workspace's changes to its branch.
    ///
    /// If the branch moved since the workspace's base, the changes are merged onto the new
    /// head. A conflict commits nothing: the workspace moves to the new head with the
    /// merge in place, markers included, for the caller to resolve and commit again.
    ///
    /// # Errors
    ///
    /// Fails if the branch is gone, an object is missing, or on a file system error.
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
        if ours == base_tree {
            if head != base_commit {
                workspace.reset(head)?;
            }
            return Ok(CommitOutcome::Unchanged);
        }
        let (tree, merged) = if head == base_commit {
            (ours, Merged::No)
        } else {
            let head_tree = self.commit(head)?.tree;
            let (tree, conflicts) = merge_trees(self, Some(base_tree), ours, head_tree)?;
            if !conflicts.is_empty() {
                workspace.rebase(self, head, head_tree, tree)?;
                return Ok(CommitOutcome::Conflicted(conflicts));
            }
            (tree, Merged::Yes)
        };
        let commit = self.put_commit(&Commit {
            tree,
            parents: vec![head],
            change: workspace.change(),
            author: author.clone(),
            time: SystemTime::now(),
            message: message.to_owned(),
        })?;
        self.update_branch(&branch, Some(head), commit)?;
        workspace.reset(commit)?;
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
