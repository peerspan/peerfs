//! Workspaces: a writable overlay on the local host over a commit of a branch.
//!
//! A workspace keeps the files written in it under `upper/`, whole, and the paths removed
//! from its base in `whiteouts`. Reading looks at the overlay first, then the base commit.
//! Every write is synced before it returns.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::classes::{Class, Classifier};
use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{ChangeId, Content, Entry, FileMode, Name, Tree};
use crate::path::TreePath;
use crate::space::{Space, mode_of, permissions};
use crate::store::sync_dir;

/// What kind of entry a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// A directory.
    Directory,
}

impl EntryKind {
    fn of(entry: &Entry) -> Self {
        match entry {
            Entry::File { .. } => Self::File,
            Entry::Symlink(_) => Self::Symlink,
            Entry::Tree(_) => Self::Directory,
        }
    }
}

/// Where a path's entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// In the overlay: a real file, link or directory at this local path. A directory here
    /// may also have entries from the base.
    Local(PathBuf),
    /// Only in the base commit.
    Base(Entry),
}

/// An entry to create in a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewEntry<'target> {
    /// An empty regular file.
    File {
        /// Whether it is executable.
        executable: bool,
    },
    /// An empty directory.
    Directory,
    /// A symbolic link to this target.
    Symlink(&'target str),
}

/// An uncommitted change in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Change {
    /// A file or link the base does not have was written.
    Added(TreePath),
    /// A file or link the base has was written.
    Written(TreePath),
    /// The path, and anything under it, was removed from the base.
    Removed(TreePath),
}

/// A writable view of a branch on this host.
#[derive(Debug)]
pub struct Workspace {
    dir: PathBuf,
    name: Name,
    branch: Name,
    base: ObjectId,
    change: ChangeId,
    snapshot: Option<ObjectId>,
    merging: Option<ObjectId>,
    whiteouts: BTreeSet<TreePath>,
    /// The base commit's tree, once looked up.
    base_tree: Cell<Option<ObjectId>>,
    /// The state and whiteout files as this value last read or wrote them.
    freshness: Freshness,
    /// When each base entry was first seen at its path, loaded on first use.
    stamps: Option<HashMap<TreePath, (u64, SystemTime)>>,
}

/// Identifies a version of a file: its inode, length and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct FileVersion {
    inode: u64,
    len: u64,
    modified: (i64, i64),
}

impl FileVersion {
    fn of(path: &Path) -> Self {
        fs::metadata(path).map_or_else(
            |_| Self::default(),
            |meta| Self {
                inode: meta.ino(),
                len: meta.len(),
                modified: (meta.mtime(), meta.mtime_nsec()),
            },
        )
    }
}

/// The versions of a workspace's state and whiteout files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Freshness {
    state: FileVersion,
    whiteouts: FileVersion,
}

impl Freshness {
    fn of(dir: &Path) -> Self {
        Self {
            state: FileVersion::of(&dir.join(STATE)),
            whiteouts: FileVersion::of(&dir.join(WHITEOUTS)),
        }
    }
}

const STATE: &str = "state";
const WHITEOUTS: &str = "whiteouts";
const STAMPS: &str = "stamps";

/// A stable digest of a base entry, for telling whether a path's content changed.
fn entry_key(entry: &Entry) -> u64 {
    let mut hasher = blake3::Hasher::new_derive_key("peerfs 2026-10-27 stamp key");
    match entry {
        Entry::File { content, mode } => {
            hasher.update(b"f");
            hasher.update(&[match mode {
                FileMode::Regular => 0,
                FileMode::Executable => 1,
            }]);
            match content {
                Content::Empty => {}
                Content::Blob(id) | Content::Chunked(id) => {
                    hasher.update(&id.to_bytes());
                }
            }
        }
        Entry::Symlink(target) => {
            hasher.update(b"l");
            hasher.update(target.as_bytes());
        }
        Entry::Tree(id) => {
            hasher.update(b"d");
            hasher.update(&id.to_bytes());
        }
    }
    let mut key = [0; 8];
    key.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    u64::from_le_bytes(key)
}

/// One line of the stamp log.
fn stamp_line(path: &TreePath, key: u64, at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!(
        "{}.{:09} {key:016x} {path}\n",
        since.as_secs(),
        since.subsec_nanos()
    )
}

fn parse_time(text: &str) -> Option<SystemTime> {
    let (secs, nanos) = text.split_once('.')?;
    Some(UNIX_EPOCH + Duration::new(secs.parse().ok()?, nanos.parse().ok()?))
}

fn upper(dir: &Path) -> PathBuf {
    dir.join("upper")
}

impl Workspace {
    pub(crate) fn create(dir: PathBuf, name: Name, branch: Name, base: ObjectId) -> Result<Self> {
        fs::create_dir_all(upper(&dir)).map_err(|e| Error::io("creating", &dir, e))?;
        let workspace = Self {
            dir,
            name,
            branch,
            base,
            change: ChangeId::random(),
            snapshot: None,
            merging: None,
            whiteouts: BTreeSet::new(),
            base_tree: Cell::new(None),
            freshness: Freshness::default(),
            stamps: None,
        };
        let mut workspace = workspace;
        workspace.save()?;
        workspace.save_whiteouts()?;
        if let Some(parent) = workspace.dir.parent() {
            sync_dir(parent)?;
        }
        Ok(workspace)
    }

    pub(crate) fn open(dir: PathBuf, name: Name) -> Result<Self> {
        let freshness = Freshness::of(&dir);
        let state_path = dir.join(STATE);
        let state =
            fs::read_to_string(&state_path).map_err(|e| Error::io("reading", &state_path, e))?;
        let corrupt = |what: &str| Error::Corrupt(format!("workspace {name}: bad {what}"));
        let mut fields = BTreeMap::new();
        for line in state.lines() {
            if let Some((key, value)) = line.split_once(' ') {
                fields.insert(key, value);
            }
        }
        let branch = fields
            .get("branch")
            .and_then(|text| Name::try_from(*text).ok())
            .ok_or_else(|| corrupt("branch"))?;
        let base = fields
            .get("base")
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| corrupt("base"))?;
        let change = fields
            .get("change")
            .and_then(|text| ChangeId::from_hex(text))
            .ok_or_else(|| corrupt("change"))?;
        let snapshot = match fields.get("snapshot") {
            Some(text) => Some(text.parse().map_err(|_| corrupt("snapshot"))?),
            None => None,
        };
        let merging = match fields.get("merge") {
            Some(text) => Some(text.parse().map_err(|_| corrupt("merge"))?),
            None => None,
        };
        let whiteouts_path = dir.join(WHITEOUTS);
        let mut whiteouts = BTreeSet::new();
        match fs::read_to_string(&whiteouts_path) {
            // A log: `+path` adds a whiteout, `-path` drops one, a bare path adds one.
            Ok(text) => {
                for line in text.lines() {
                    if let Some(dropped) = line.strip_prefix('-') {
                        whiteouts.remove(&TreePath::try_from(dropped)?);
                    } else {
                        let added = line.strip_prefix('+').unwrap_or(line);
                        whiteouts.insert(TreePath::try_from(added)?);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io("reading", whiteouts_path, e)),
        }
        Ok(Self {
            dir,
            name,
            branch,
            base,
            change,
            snapshot,
            merging,
            whiteouts,
            base_tree: Cell::new(None),
            freshness,
            stamps: None,
        })
    }

    /// Whether this value still matches the workspace on disk: no other value of it has
    /// changed its state or removals since this one last read or wrote them. Files in the
    /// overlay are always read from disk, so they never make a value stale.
    pub fn is_current(&self) -> bool {
        Freshness::of(&self.dir) == self.freshness
    }

    /// When the base entry `entry` at `path` is dated: when this workspace first saw that
    /// content there, or when a commit took it from the overlay with that mtime. Kept in
    /// the workspace, so the date survives commits and remounts and changes only with the
    /// content.
    ///
    /// # Errors
    ///
    /// Fails if the workspace's stamps cannot be read or written.
    pub fn stamp(&mut self, path: &TreePath, entry: &Entry) -> Result<SystemTime> {
        let key = entry_key(entry);
        if let Some((known, at)) = self.stamps()?.get(path)
            && *known == key
        {
            return Ok(*at);
        }
        let now = SystemTime::now();
        self.record_stamp(path, key, now)?;
        Ok(now)
    }

    /// Dates the base entry `entry` at `path` to `at`.
    pub(crate) fn set_stamp(
        &mut self,
        path: &TreePath,
        entry: &Entry,
        at: SystemTime,
    ) -> Result<()> {
        self.stamps()?;
        self.record_stamp(path, entry_key(entry), at)
    }

    fn stamps(&mut self) -> Result<&mut HashMap<TreePath, (u64, SystemTime)>> {
        if self.stamps.is_none() {
            self.stamps = Some(self.load_stamps()?);
        }
        Ok(self.stamps.get_or_insert_with(HashMap::new))
    }

    /// Reads the stamp log, `seconds.nanoseconds key path` per line, the last line for a
    /// path winning; rewrites it when it has grown well past the stamps it holds.
    fn load_stamps(&self) -> Result<HashMap<TreePath, (u64, SystemTime)>> {
        let path = self.dir.join(STAMPS);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => return Err(Error::io("reading", &path, e)),
        };
        let mut stamps = HashMap::new();
        let mut lines = 0_usize;
        for line in text.lines() {
            lines += 1;
            let mut fields = line.splitn(3, ' ');
            let (Some(at), Some(key), Some(at_path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Some(at), Ok(key), Ok(at_path)) = (
                parse_time(at),
                u64::from_str_radix(key, 16),
                TreePath::try_from(at_path),
            ) else {
                continue;
            };
            stamps.insert(at_path, (key, at));
        }
        if lines > 2 * stamps.len() + 1024 {
            let mut compact = String::new();
            for (at_path, (key, at)) in &stamps {
                compact.push_str(&stamp_line(at_path, *key, *at));
            }
            write_synced(&self.dir, STAMPS, compact.as_bytes())?;
        }
        Ok(stamps)
    }

    fn record_stamp(&mut self, path: &TreePath, key: u64, at: SystemTime) -> Result<()> {
        let text = path.to_string();
        // A name holding a line break cannot be logged; it is dated afresh each time.
        if !text.contains(['\n', '\r']) {
            let log = self.dir.join(STAMPS);
            // Not synced: a lost stamp only dates a file afresh, which costs a rebuild.
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .and_then(|mut file| file.write_all(stamp_line(path, key, at).as_bytes()))
                .map_err(|e| Error::io("writing", &log, e))?;
        }
        if let Some(stamps) = &mut self.stamps {
            stamps.insert(path.clone(), (key, at));
        }
        Ok(())
    }

    /// The workspace's name.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// The branch it commits to.
    pub fn branch(&self) -> &str {
        self.branch.as_str()
    }

    /// The commit its changes are relative to.
    pub fn base(&self) -> ObjectId {
        self.base
    }

    /// The change its commits belong to.
    pub fn change(&self) -> ChangeId {
        self.change
    }

    /// The latest snapshot of the current change, if one was taken since the last commit.
    pub fn snapshot(&self) -> Option<ObjectId> {
        self.snapshot
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: ObjectId) -> Result<()> {
        self.snapshot = Some(snapshot);
        self.save()
    }

    /// Ends the current change: follows `commit` with an empty overlay and starts a new
    /// change with no snapshots.
    pub(crate) fn finish_change(&mut self, space: &Space, commit: ObjectId) -> Result<()> {
        self.change = ChangeId::random();
        self.snapshot = None;
        self.reset(space, commit)
    }

    /// The commit being merged in, when a merge stopped on conflicts. Committing the
    /// workspace makes a commit with it as a second parent.
    pub fn merging(&self) -> Option<ObjectId> {
        self.merging
    }

    pub(crate) fn set_merging(&mut self, merging: Option<ObjectId>) -> Result<()> {
        self.merging = merging;
        self.save()
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn upper_path(&self, path: &TreePath) -> PathBuf {
        let mut full = upper(&self.dir);
        for name in path.components() {
            full.push(name.as_str());
        }
        full
    }

    /// Writes the state file.
    fn save(&mut self) -> Result<()> {
        let snapshot = self
            .snapshot
            .map(|snapshot| format!("snapshot {snapshot}\n"))
            .unwrap_or_default();
        let merge = self
            .merging
            .map(|merging| format!("merge {merging}\n"))
            .unwrap_or_default();
        let state = format!(
            "branch {}\nbase {}\nchange {}\n{snapshot}{merge}",
            self.branch, self.base, self.change
        );
        write_synced(&self.dir, STATE, state.as_bytes())?;
        self.freshness = Freshness::of(&self.dir);
        Ok(())
    }

    /// Rewrites the whiteout log as the current set.
    fn save_whiteouts(&mut self) -> Result<()> {
        let mut whiteouts = String::new();
        for path in &self.whiteouts {
            whiteouts.push_str(&path.to_string());
            whiteouts.push('\n');
        }
        write_synced(&self.dir, WHITEOUTS, whiteouts.as_bytes())?;
        self.freshness = Freshness::of(&self.dir);
        Ok(())
    }

    /// Records the removal of the base's entry at `path`, durably.
    fn add_whiteout(&mut self, path: &TreePath) -> Result<()> {
        if self.whiteouts.insert(path.clone()) {
            self.log_whiteout('+', path)?;
        }
        Ok(())
    }

    /// Forgets a removal at `path`, durably.
    fn drop_whiteout(&mut self, path: &TreePath) -> Result<()> {
        if self.whiteouts.remove(path) {
            self.log_whiteout('-', path)?;
        }
        Ok(())
    }

    fn log_whiteout(&mut self, op: char, path: &TreePath) -> Result<()> {
        let log = self.dir.join(WHITEOUTS);
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut file| {
                file.write_all(format!("{op}{path}\n").as_bytes())?;
                file.sync_data()
            })
            .map_err(|e| Error::io("writing", &log, e))?;
        self.freshness = Freshness::of(&self.dir);
        Ok(())
    }

    fn base_tree(&self, space: &Space) -> Result<ObjectId> {
        if let Some(tree) = self.base_tree.get() {
            return Ok(tree);
        }
        let tree = space.commit(self.base)?.tree;
        self.base_tree.set(Some(tree));
        Ok(tree)
    }

    /// Whether the base's entry at `path` is hidden by a removal or by a file written over
    /// one of its directories.
    fn hidden(&self, path: &TreePath) -> bool {
        if !self.whiteouts.is_empty() {
            let mut prefix = TreePath::root();
            for name in path.components() {
                prefix = prefix.join(name);
                if self.whiteouts.contains(&prefix) {
                    return true;
                }
            }
        }
        let mut ancestor = path.clone();
        while let Some((parent, _)) = ancestor.split_last() {
            if !parent.is_root()
                && fs::symlink_metadata(self.upper_path(&parent)).is_ok_and(|meta| !meta.is_dir())
            {
                return true;
            }
            ancestor = parent;
        }
        false
    }

    fn base_entry(&self, space: &Space, path: &TreePath) -> Result<Option<Entry>> {
        if self.hidden(path) {
            return Ok(None);
        }
        space.entry(self.base_tree(space)?, path)
    }

    /// Reads the file at `path`.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there, it is not a regular file, or on a store error.
    pub fn read(&self, space: &Space, path: &str) -> Result<Vec<u8>> {
        let tree_path = TreePath::try_from(path)?;
        let local = self.upper_path(&tree_path);
        if let Ok(meta) = fs::symlink_metadata(&local) {
            if meta.is_file() {
                return fs::read(&local).map_err(|e| Error::io("reading", local, e));
            }
            if !meta.is_dir() {
                return Err(Error::WrongEntry {
                    path: tree_path.to_string(),
                    found: "symlink",
                });
            }
        }
        match self.base_entry(space, &tree_path)? {
            Some(Entry::File { content, .. }) => space.read_content(&content),
            Some(other) => Err(Error::WrongEntry {
                path: tree_path.to_string(),
                found: other.describe(),
            }),
            None => Err(Error::NotFound(tree_path.to_string())),
        }
    }

    /// Writes `bytes` to the file at `path`, creating its directories. A file that was
    /// executable stays executable.
    ///
    /// # Errors
    ///
    /// Fails if the path is the root or on a file system error.
    pub fn write(&mut self, space: &Space, path: &str, bytes: &[u8]) -> Result<()> {
        let tree_path = TreePath::try_from(path)?;
        let Some((parent_path, _)) = tree_path.split_last() else {
            return Err(Error::WrongEntry {
                path: tree_path.to_string(),
                found: "directory",
            });
        };
        let parent = self.upper_path(&parent_path);
        let local = self.upper_path(&tree_path);
        let mode = match fs::symlink_metadata(&local) {
            Ok(meta) if meta.is_file() => mode_of(meta.permissions().mode()),
            _ => match self.base_entry(space, &tree_path)? {
                Some(Entry::File { mode, .. }) => mode,
                _ => FileMode::Regular,
            },
        };
        if let Ok(meta) = fs::symlink_metadata(&local) {
            remove_local(&local, &meta)?;
        }
        make_dirs(&parent)?;
        let mut file = File::create(&local).map_err(|e| Error::io("creating", &local, e))?;
        file.write_all(bytes)
            .and_then(|()| file.set_permissions(fs::Permissions::from_mode(permissions(mode))))
            .and_then(|()| file.sync_data())
            .map_err(|e| Error::io("writing", &local, e))?;
        sync_dir(&parent)?;
        self.drop_whiteout(&tree_path)?;
        Ok(())
    }

    /// Removes the file, link or directory at `path`.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there or on a file system error.
    pub fn remove(&mut self, space: &Space, path: &str) -> Result<()> {
        self.remove_at(space, &TreePath::try_from(path)?)
    }

    /// Where the entry at `path` is, if anything is there.
    ///
    /// # Errors
    ///
    /// Fails on a store error.
    pub fn locate(&self, space: &Space, path: &TreePath) -> Result<Option<Located>> {
        let local = self.upper_path(path);
        if fs::symlink_metadata(&local).is_ok() {
            return Ok(Some(Located::Local(local)));
        }
        Ok(self.base_entry(space, path)?.map(Located::Base))
    }

    /// Brings the entry at `path` into the overlay, if it is only in the base, and returns
    /// its local path. A directory is created empty; its base entries stay visible.
    ///
    /// The copy is not synced: like any write through a mount, it is durable once synced.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there, or on a store or file system error.
    pub fn copy_up(&mut self, space: &Space, path: &TreePath) -> Result<PathBuf> {
        let local = self.upper_path(path);
        if fs::symlink_metadata(&local).is_ok() {
            return Ok(local);
        }
        let entry = self
            .base_entry(space, path)?
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        if let Some((parent, _)) = path.split_last() {
            make_dirs(&self.upper_path(&parent))?;
        }
        match entry {
            Entry::File { content, mode } => {
                fs::write(&local, space.read_content(&content)?)
                    .and_then(|()| {
                        fs::set_permissions(&local, fs::Permissions::from_mode(permissions(mode)))
                    })
                    .map_err(|e| Error::io("copying up", &local, e))?;
            }
            Entry::Symlink(target) => {
                symlink(target, &local).map_err(|e| Error::io("copying up", &local, e))?;
            }
            Entry::Tree(_) => make_dirs(&local)?,
        }
        Ok(local)
    }

    /// Creates an empty file, an empty directory or a link at `path`, in a directory that
    /// exists, and returns its local path.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Exists`] if something is there, [`Error::NotFound`] if its
    /// directory is missing, or on a file system error.
    pub fn create_entry(
        &mut self,
        space: &Space,
        path: &TreePath,
        new: NewEntry<'_>,
    ) -> Result<PathBuf> {
        let Some((parent, _)) = path.split_last() else {
            return Err(Error::Exists(path.to_string()));
        };
        if self.locate(space, path)?.is_some() {
            return Err(Error::Exists(path.to_string()));
        }
        match self.locate(space, &parent)? {
            Some(Located::Base(Entry::Tree(_))) => make_dirs(&self.upper_path(&parent))?,
            Some(Located::Local(local)) if local.is_dir() => {}
            Some(_) => {
                return Err(Error::WrongEntry {
                    path: parent.to_string(),
                    found: "file",
                });
            }
            None => return Err(Error::NotFound(parent.to_string())),
        }
        let local = self.upper_path(path);
        let created = match new {
            NewEntry::File { executable } => File::create_new(&local).and_then(|file| {
                let mode = if executable {
                    FileMode::Executable
                } else {
                    FileMode::Regular
                };
                file.set_permissions(fs::Permissions::from_mode(permissions(mode)))
            }),
            NewEntry::Directory => fs::create_dir(&local),
            NewEntry::Symlink(target) => symlink(target, &local),
        };
        created.map_err(|e| Error::io("creating", &local, e))?;
        Ok(local)
    }

    /// Removes the empty directory at `path`.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotEmpty`] if it has entries, or if it is missing or not a
    /// directory.
    pub fn remove_dir(&mut self, space: &Space, path: &TreePath) -> Result<()> {
        if !self.list(space, &path.to_string())?.is_empty() {
            return Err(Error::NotEmpty(path.to_string()));
        }
        self.remove_at(space, path)
    }

    /// Moves the entry at `from` to `to`, replacing a file or empty directory there.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::DirectoryRename`] for a directory that holds base content,
    /// [`Error::NotEmpty`] when `to` is a directory with entries, [`Error::WrongEntry`]
    /// when a file would replace a directory or the reverse, or [`Error::NotFound`].
    pub fn rename(&mut self, space: &Space, from: &TreePath, to: &TreePath) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let source = self
            .locate(space, from)?
            .ok_or_else(|| Error::NotFound(from.to_string()))?;
        let from_in_base = self.base_entry(space, from)?.is_some();
        let source_is_dir = match &source {
            Located::Local(local) => local.is_dir() && !local.is_symlink(),
            Located::Base(entry) => matches!(entry, Entry::Tree(_)),
        };
        if source_is_dir && from_in_base {
            return Err(Error::DirectoryRename(from.to_string()));
        }
        let Some((to_parent, _)) = to.split_last() else {
            return Err(Error::Exists(to.to_string()));
        };
        if let Some(existing) = self.locate(space, to)? {
            let existing_is_dir = match &existing {
                Located::Local(local) => local.is_dir() && !local.is_symlink(),
                Located::Base(entry) => matches!(entry, Entry::Tree(_)),
            };
            match (source_is_dir, existing_is_dir) {
                (false, true) => {
                    return Err(Error::WrongEntry {
                        path: to.to_string(),
                        found: "directory",
                    });
                }
                (true, false) => {
                    return Err(Error::WrongEntry {
                        path: to.to_string(),
                        found: "file",
                    });
                }
                (true, true) => self.remove_dir(space, to)?,
                (false, false) => self.remove_at(space, to)?,
            }
        }
        let local_from = self.copy_up(space, from)?;
        match self.locate(space, &to_parent)? {
            Some(Located::Base(Entry::Tree(_))) => make_dirs(&self.upper_path(&to_parent))?,
            Some(Located::Local(local)) if local.is_dir() => {}
            _ => return Err(Error::NotFound(to_parent.to_string())),
        }
        let local_to = self.upper_path(to);
        fs::rename(&local_from, &local_to).map_err(|e| Error::io("renaming", &local_from, e))?;
        if from_in_base {
            self.add_whiteout(from)?;
        }
        Ok(())
    }

    fn remove_at(&mut self, space: &Space, tree_path: &TreePath) -> Result<()> {
        let tree_path = tree_path.clone();
        let local = self.upper_path(&tree_path);
        let in_upper = match fs::symlink_metadata(&local) {
            Ok(meta) => {
                remove_local(&local, &meta)?;
                true
            }
            Err(_) => false,
        };
        let in_base = self.base_entry(space, &tree_path)?.is_some();
        if in_base {
            self.add_whiteout(&tree_path)?;
        }
        if !in_upper && !in_base {
            return Err(Error::NotFound(tree_path.to_string()));
        }
        Ok(())
    }

    /// The entries of the directory at `path`, by name.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there, it is not a directory, or on a store error.
    pub fn list(&self, space: &Space, path: &str) -> Result<Vec<(String, EntryKind)>> {
        let tree_path = TreePath::try_from(path)?;
        let mut entries = BTreeMap::new();
        let mut found = tree_path.is_root();
        match self.base_entry(space, &tree_path)? {
            Some(Entry::Tree(id)) => {
                found = true;
                for (name, entry) in space.shared_tree(id)?.iter() {
                    if !self.whiteouts.contains(&tree_path.join(name)) {
                        entries.insert(name.to_string(), EntryKind::of(entry));
                    }
                }
            }
            Some(other) => {
                return Err(Error::WrongEntry {
                    path: tree_path.to_string(),
                    found: other.describe(),
                });
            }
            None => {}
        }
        let local = self.upper_path(&tree_path);
        if fs::symlink_metadata(&local).is_ok_and(|meta| meta.is_dir()) {
            found = true;
            for entry in fs::read_dir(&local).map_err(|e| Error::io("listing", &local, e))? {
                let entry = entry.map_err(|e| Error::io("listing", &local, e))?;
                let kind = entry
                    .file_type()
                    .map_err(|e| Error::io("reading", entry.path(), e))?;
                let kind = if kind.is_dir() {
                    EntryKind::Directory
                } else if kind.is_symlink() {
                    EntryKind::Symlink
                } else {
                    EntryKind::File
                };
                if let Some(name) = entry.file_name().to_str() {
                    entries.insert(name.to_owned(), kind);
                }
            }
        }
        if !found {
            return Err(Error::NotFound(tree_path.to_string()));
        }
        Ok(entries.into_iter().collect())
    }

    /// The uncommitted changes a commit would take: source paths added or written, then
    /// paths removed. Derived and secret paths are left out; see [`Workspace::kept_local`].
    ///
    /// # Errors
    ///
    /// Fails on a file system or store error.
    pub fn changes(&self, space: &Space) -> Result<Vec<Change>> {
        let mut changes = Vec::new();
        for (path, local) in self.classified(space)?.source {
            if local.is_dir() {
                continue;
            }
            changes.push(if self.base_entry(space, &path)?.is_some() {
                Change::Written(path)
            } else {
                Change::Added(path)
            });
        }
        changes.extend(self.whiteouts.iter().cloned().map(Change::Removed));
        Ok(changes)
    }

    /// The overlay's derived and secret paths, outermost only: they stay in the workspace
    /// across commits, and no commit or snapshot takes them.
    ///
    /// # Errors
    ///
    /// Fails on a file system or store error.
    pub fn kept_local(&self, space: &Space) -> Result<Vec<(TreePath, Class)>> {
        Ok(self.classified(space)?.local)
    }

    /// The classifier for this workspace's view: the space owner's rules, and the
    /// `.gitignore` files the workspace shows.
    fn classifier<'view>(&'view self, space: &'view Space) -> Result<Classifier<'view>> {
        let owner = space.owner_rules()?;
        Ok(Classifier::new(&owner, move |dir: &TreePath| {
            let file = Name::try_from(".gitignore").ok()?;
            self.read(space, &dir.join(&file).to_string()).ok()
        }))
    }

    /// The overlay's entries by class: source files, links and empty directories to
    /// commit, and the outermost derived and secret paths to keep.
    fn classified(&self, space: &Space) -> Result<Classified> {
        let mut rules = self.classifier(space)?;
        let mut sorted = Classified::default();
        walk_classified(
            &upper(&self.dir),
            &TreePath::root(),
            &mut rules,
            &mut sorted,
        )?;
        Ok(sorted)
    }

    /// Applies the overlay's source entries and removals to `base_tree` and returns the
    /// resulting tree.
    pub(crate) fn apply(&self, space: &mut Space, base_tree: ObjectId) -> Result<Applied> {
        let written = self.classified(space)?.source;
        let mut files = Vec::new();
        let mut edit = space.edit(base_tree)?;
        for removed in &self.whiteouts {
            edit.remove(space, removed)?;
        }
        for (path, local) in written {
            let meta = fs::symlink_metadata(&local).map_err(|e| Error::io("reading", &local, e))?;
            let entry = if meta.is_dir() {
                // An empty directory of the overlay. One the base shows here is only the
                // parent of a removed or copied-up entry, so it changes nothing.
                if self.base_entry(space, &path)?.is_some() {
                    continue;
                }
                Entry::Tree(space.put_tree(&Tree::new())?)
            } else if meta.is_symlink() {
                let target = fs::read_link(&local).map_err(|e| Error::io("reading", &local, e))?;
                Entry::Symlink(target.to_string_lossy().into_owned())
            } else {
                let bytes = fs::read(&local).map_err(|e| Error::io("reading", &local, e))?;
                Entry::File {
                    content: space.write_content(&bytes)?,
                    mode: mode_of(meta.permissions().mode()),
                }
            };
            if let Ok(modified) = meta.modified() {
                files.push((path.clone(), entry.clone(), modified));
            }
            edit.insert(space, &path, entry)?;
        }
        Ok(Applied {
            tree: edit.finish(space)?,
            files,
        })
    }

    /// Dates each entry committed from the overlay that `tree` holds unchanged to the
    /// overlay file's mtime, so committing it does not make it look modified.
    pub(crate) fn keep_mtimes(
        &mut self,
        space: &Space,
        tree: ObjectId,
        files: &[(TreePath, Entry, SystemTime)],
    ) -> Result<()> {
        for (path, entry, modified) in files {
            if space.entry(tree, path)?.as_ref() == Some(entry) {
                self.set_stamp(path, entry, *modified)?;
            }
        }
        Ok(())
    }

    /// Drops the overlay's source entries and removals, keeps its derived and secret
    /// paths, and follows `commit`.
    pub(crate) fn reset(&mut self, space: &Space, commit: ObjectId) -> Result<()> {
        let kept = self.kept_local(space)?;
        prune_upper(&upper(&self.dir), &TreePath::root(), &kept)?;
        self.whiteouts.clear();
        self.save_whiteouts()?;
        self.base_tree.set(None);
        self.merging = None;
        self.base = commit;
        self.save()
    }

    /// Follows `head` and holds `target` as its overlay.
    pub(crate) fn rebase(
        &mut self,
        space: &Space,
        head: ObjectId,
        head_tree: ObjectId,
        target: ObjectId,
    ) -> Result<()> {
        self.reset(space, head)?;
        self.materialize(
            space,
            &TreePath::root(),
            Some(&Entry::Tree(head_tree)),
            Some(&Entry::Tree(target)),
        )?;
        self.save()?;
        self.save_whiteouts()
    }

    fn materialize(
        &mut self,
        space: &Space,
        path: &TreePath,
        from: Option<&Entry>,
        to: Option<&Entry>,
    ) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let replaces_directory = matches!(from, Some(Entry::Tree(_)));
        match to {
            None => {
                self.whiteouts.insert(path.clone());
            }
            Some(Entry::Tree(to_id)) => {
                let from_tree = match from {
                    Some(Entry::Tree(id)) => Some(space.tree(*id)?),
                    Some(_) => {
                        self.whiteouts.insert(path.clone());
                        None
                    }
                    None => None,
                };
                let to_tree = space.tree(*to_id)?;
                let names: BTreeSet<&Name> = to_tree
                    .iter()
                    .chain(from_tree.iter().flat_map(Tree::iter))
                    .map(|(name, _)| name)
                    .collect();
                for name in names {
                    self.materialize(
                        space,
                        &path.join(name),
                        from_tree.as_ref().and_then(|tree| tree.get(name)),
                        to_tree.get(name),
                    )?;
                }
            }
            Some(Entry::File { content, mode }) => {
                if replaces_directory {
                    self.whiteouts.insert(path.clone());
                }
                let local = self.upper_path(path);
                make_dirs(local.parent().expect("an upper path has a parent"))?;
                fs::write(&local, space.read_content(content)?)
                    .and_then(|()| {
                        fs::set_permissions(&local, fs::Permissions::from_mode(permissions(*mode)))
                    })
                    .map_err(|e| Error::io("writing", &local, e))?;
            }
            Some(Entry::Symlink(target)) => {
                if replaces_directory {
                    self.whiteouts.insert(path.clone());
                }
                let local = self.upper_path(path);
                make_dirs(local.parent().expect("an upper path has a parent"))?;
                symlink(target, &local).map_err(|e| Error::io("linking", &local, e))?;
            }
        }
        Ok(())
    }
}

fn make_dirs(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| Error::io("creating", dir, e))
}

fn remove_local(path: &Path, meta: &fs::Metadata) -> Result<()> {
    let removed = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    removed.map_err(|e| Error::io("removing", path, e))
}

fn write_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let temporary = dir.join(format!(".{name}.new"));
    let mut file = File::create(&temporary).map_err(|e| Error::io("creating", &temporary, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| Error::io("writing", &temporary, e))?;
    let target = dir.join(name);
    fs::rename(&temporary, &target).map_err(|e| Error::io("renaming", &temporary, e))?;
    sync_dir(dir)
}

/// A tree made from a workspace's overlay, with what each overlay entry became and the
/// overlay file's mtime.
#[derive(Debug)]
pub(crate) struct Applied {
    pub(crate) tree: ObjectId,
    pub(crate) files: Vec<(TreePath, Entry, SystemTime)>,
}

/// An overlay's entries by class.
#[derive(Debug, Default)]
struct Classified {
    /// Source files, links and empty directories, with their local paths.
    source: Vec<(TreePath, PathBuf)>,
    /// The outermost derived and secret paths.
    local: Vec<(TreePath, Class)>,
}

/// Sorts the entries under `dir`, whose tree path is `path` and which is source, by
/// class. A derived or secret directory is not entered.
fn walk_classified(
    dir: &Path,
    path: &TreePath,
    rules: &mut Classifier<'_>,
    sorted: &mut Classified,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .map_err(|e| Error::io("listing", dir, e))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| Error::io("listing", dir, e))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let local = entry.path();
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| Name::try_from(name).ok())
        else {
            return Err(Error::Unsupported {
                path: local,
                reason: "the name is not a valid UTF-8 path component",
            });
        };
        let child = path.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|e| Error::io("reading", &local, e))?;
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        let class = rules.classify_child(&child, kind);
        if class != Class::Source {
            sorted.local.push((child, class));
            continue;
        }
        if kind != EntryKind::Directory {
            sorted.source.push((child, local));
            continue;
        }
        let empty = fs::read_dir(&local)
            .map_err(|e| Error::io("listing", &local, e))?
            .next()
            .is_none();
        if empty {
            sorted.source.push((child, local));
        } else {
            walk_classified(&local, &child, rules, sorted)?;
        }
    }
    Ok(())
}

/// Removes everything under `dir` except the `kept` paths and the directories leading to
/// them. Returns whether anything under `dir` was kept.
fn prune_upper(dir: &Path, path: &TreePath, kept: &[(TreePath, Class)]) -> Result<bool> {
    let mut kept_any = false;
    for entry in fs::read_dir(dir).map_err(|e| Error::io("listing", dir, e))? {
        let entry = entry.map_err(|e| Error::io("listing", dir, e))?;
        let local = entry.path();
        let meta = fs::symlink_metadata(&local).map_err(|e| Error::io("reading", &local, e))?;
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| Name::try_from(name).ok())
        else {
            // No commit could have taken it, so nothing keeps it.
            remove_local(&local, &meta)?;
            continue;
        };
        let child = path.join(&name);
        if kept.iter().any(|(kept, _)| *kept == child) {
            kept_any = true;
            continue;
        }
        let leads_to_kept = meta.is_dir() && kept.iter().any(|(kept, _)| kept.starts_with(&child));
        if leads_to_kept && prune_upper(&local, &child, kept)? {
            kept_any = true;
        } else {
            remove_local(&local, &meta)?;
        }
    }
    Ok(kept_any)
}
