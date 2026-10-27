//! Workspaces: a writable overlay on the local host over a commit of a branch.
//!
//! A workspace keeps the files written in it under `upper/`, whole, and the paths removed
//! from its base in `whiteouts`. Reading looks at the overlay first, then the base commit.
//! Every write is synced before it returns.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{ChangeId, Entry, FileMode, Name, Tree};
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

/// An uncommitted change in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// The path was written.
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
        };
        workspace.save()?;
        if let Some(parent) = workspace.dir.parent() {
            sync_dir(parent)?;
        }
        Ok(workspace)
    }

    pub(crate) fn open(dir: PathBuf, name: Name) -> Result<Self> {
        let state_path = dir.join("state");
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
        let whiteouts_path = dir.join("whiteouts");
        let whiteouts = match fs::read_to_string(&whiteouts_path) {
            Ok(text) => text
                .lines()
                .map(TreePath::try_from)
                .collect::<Result<BTreeSet<_>>>()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(Error::io("reading", whiteouts_path, e)),
        };
        Ok(Self {
            dir,
            name,
            branch,
            base,
            change,
            snapshot,
            merging,
            whiteouts,
        })
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
    pub(crate) fn finish_change(&mut self, commit: ObjectId) -> Result<()> {
        self.change = ChangeId::random();
        self.snapshot = None;
        self.reset(commit)
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

    fn save(&self) -> Result<()> {
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
        write_synced(&self.dir, "state", state.as_bytes())?;
        let mut whiteouts = String::new();
        for path in &self.whiteouts {
            whiteouts.push_str(&path.to_string());
            whiteouts.push('\n');
        }
        write_synced(&self.dir, "whiteouts", whiteouts.as_bytes())
    }

    fn base_tree(&self, space: &Space) -> Result<ObjectId> {
        Ok(space.commit(self.base)?.tree)
    }

    /// Whether the base's entry at `path` is hidden by a removal or by a file written over
    /// one of its directories.
    fn hidden(&self, path: &TreePath) -> bool {
        if self
            .whiteouts
            .iter()
            .any(|removed| path.starts_with(removed))
        {
            return true;
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
        if self.whiteouts.remove(&tree_path) {
            self.save()?;
        }
        Ok(())
    }

    /// Removes the file, link or directory at `path`.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there or on a file system error.
    pub fn remove(&mut self, space: &Space, path: &str) -> Result<()> {
        let tree_path = TreePath::try_from(path)?;
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
            self.whiteouts.insert(tree_path.clone());
            self.save()?;
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
                for (name, entry) in space.tree(id)?.iter() {
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

    /// The uncommitted changes: paths written, then paths removed.
    ///
    /// # Errors
    ///
    /// Fails on a file system error.
    pub fn changes(&self) -> Result<Vec<Change>> {
        let mut changes = Vec::new();
        walk_upper(&upper(&self.dir), &TreePath::root(), &mut |path, _| {
            changes.push(Change::Written(path));
            Ok(())
        })?;
        changes.extend(self.whiteouts.iter().cloned().map(Change::Removed));
        Ok(changes)
    }

    /// Applies the overlay to `base_tree` and returns the resulting tree.
    pub(crate) fn apply(&self, space: &mut Space, base_tree: ObjectId) -> Result<ObjectId> {
        let mut edit = space.edit(base_tree)?;
        for removed in &self.whiteouts {
            edit.remove(space, removed)?;
        }
        let mut written = Vec::new();
        walk_upper(&upper(&self.dir), &TreePath::root(), &mut |path, local| {
            written.push((path, local.to_owned()));
            Ok(())
        })?;
        for (path, local) in written {
            let meta = fs::symlink_metadata(&local).map_err(|e| Error::io("reading", &local, e))?;
            let entry = if meta.is_symlink() {
                let target = fs::read_link(&local).map_err(|e| Error::io("reading", &local, e))?;
                Entry::Symlink(target.to_string_lossy().into_owned())
            } else {
                let bytes = fs::read(&local).map_err(|e| Error::io("reading", &local, e))?;
                Entry::File {
                    content: space.write_content(&bytes)?,
                    mode: mode_of(meta.permissions().mode()),
                }
            };
            edit.insert(space, &path, entry)?;
        }
        edit.finish(space)
    }

    /// Drops the overlay and follows `commit`.
    pub(crate) fn reset(&mut self, commit: ObjectId) -> Result<()> {
        let local = upper(&self.dir);
        fs::remove_dir_all(&local).map_err(|e| Error::io("clearing", &local, e))?;
        fs::create_dir(&local).map_err(|e| Error::io("creating", &local, e))?;
        self.whiteouts.clear();
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
        self.reset(head)?;
        self.materialize(
            space,
            &TreePath::root(),
            Some(&Entry::Tree(head_tree)),
            Some(&Entry::Tree(target)),
        )?;
        self.save()
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

/// Calls `visit` for every file and link under `dir`, with its tree path.
fn walk_upper(
    dir: &Path,
    path: &TreePath,
    visit: &mut impl FnMut(TreePath, &Path) -> Result<()>,
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
        let kind = entry
            .file_type()
            .map_err(|e| Error::io("reading", &local, e))?;
        if kind.is_dir() {
            walk_upper(&local, &child, visit)?;
        } else {
            visit(child, &local)?;
        }
    }
    Ok(())
}
