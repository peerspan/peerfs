//! Workspaces: a writable overlay on the local host over a commit of a branch.
//!
//! A workspace keeps the files written in it under `upper/`, whole, and the paths removed
//! from its base in `whiteouts`. Reading looks at the overlay first, then the base commit,
//! then the build cache version the workspace holds, if any: that layer shows only at
//! derived paths, is read-only, and keeps its own removals in `cache-whiteouts`. Every
//! write is synced before it returns.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, FileTimes};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::classes::{Class, Classifier};
use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{ChangeId, Content, Entry, FileMode, Name, Tree};
use crate::path::TreePath;
use crate::space::cache::{DATES, FILES, copy_tree, version_dir};
use crate::space::{RulesStamp, Space, mode_of, permissions};
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
    /// Only in the build cache version the workspace holds: a real file, link or directory
    /// at this local path, which must never be written. A directory here may also have
    /// entries from the overlay and the base.
    Cached(PathBuf),
}

/// The build cache version a workspace holds, which it shows read-only under its derived
/// paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldCache {
    /// The cache.
    pub name: String,
    /// The version, which never changes once saved.
    pub version: String,
    /// The commit whose tree the version's files were built from.
    pub commit: ObjectId,
}

/// Which layers a read looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layers {
    /// The overlay, the base and the held cache version.
    All,
    /// The overlay and the base only.
    AboveCache,
}

/// Whether the base's entry at a path, and with it the cache's, is hidden by a removal or by
/// a file written over one of its directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseView {
    Shown,
    Hidden,
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
    /// Counts this workspace's changes by any of its values.
    generation: Generation,
    /// The state count as of this value's last read or write.
    seen: u64,
    /// The overlay count as of this value's last change or check.
    seen_overlay: u64,
    /// When each base entry was first seen at its path, loaded on first use.
    stamps: Option<Stamps>,
    /// When the workspace was created, and at which base; absent for workspaces created
    /// before it was recorded.
    created: Option<(SystemTime, ObjectId)>,
    /// The build cache version it holds.
    cache: Option<HeldCache>,
    /// The held version's paths removed, which stay hidden.
    cache_whiteouts: BTreeSet<TreePath>,
    /// The held version's `dates`, loaded on first use.
    dates: Option<Stamps>,
    /// Classes of paths the cache layer was asked about, by path and whether it is a
    /// directory. Forgotten when a `.gitignore` or the owner's rules change.
    classes: RefCell<HashMap<(TreePath, bool), Class>>,
    /// The owner's rules file as it was when `classes` were worked out.
    classes_rules: Cell<Option<RulesStamp>>,
}

/// Counts a workspace's changes, shared by every value of it that a space opened. The
/// space's lock keeps other processes out, so they see every change.
#[derive(Debug, Default)]
pub(crate) struct Counts {
    /// Writes to the state and whiteouts.
    state: AtomicU64,
    /// Overlay files created, replaced, moved or removed.
    overlay: AtomicU64,
}

impl Counts {
    pub(crate) fn discarded(&self) {
        self.state.fetch_add(1, Ordering::AcqRel);
        self.overlay.fetch_add(1, Ordering::AcqRel);
    }
}

pub(crate) type Generation = Arc<Counts>;

const STATE: &str = "state";
const WHITEOUTS: &str = "whiteouts";
const CACHE_WHITEOUTS: &str = "cache-whiteouts";
const STAMPS: &str = "stamps";

/// The file whose changes change which paths are derived.
const GITIGNORE: &str = ".gitignore";

/// The buffer a cached file is copied up through.
const COPY_BUFFER: usize = 1024 * 1024;

/// A stable digest of a base entry, for telling whether a path's content changed.
pub(crate) fn entry_key(entry: &Entry) -> u64 {
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

/// One line of the stamp log, and of a cache version's `dates`.
pub(crate) fn stamp_line(path: &TreePath, key: u64, at: SystemTime) -> String {
    format!("{} {key:016x} {path}\n", time_text(at))
}

/// Dates of base entries by path, each with its entry's key.
type Stamps = HashMap<TreePath, (u64, SystemTime)>;

/// A line of the stamp log as path, key and date, or `None` if it is malformed.
fn parse_stamp_line(line: &str) -> Option<(TreePath, u64, SystemTime)> {
    let mut fields = line.splitn(3, ' ');
    let (at, key, path) = (fields.next()?, fields.next()?, fields.next()?);
    Some((
        TreePath::try_from(path).ok()?,
        u64::from_str_radix(key, 16).ok()?,
        parse_time(at)?,
    ))
}

/// Reads stamp lines, the last line for a path winning, and how many lines there were. A
/// missing file has none.
fn read_stamps(path: &Path) -> Result<(Stamps, usize)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((HashMap::new(), 0)),
        Err(e) => return Err(Error::io("reading", path, e)),
    };
    let mut stamps = HashMap::new();
    let mut lines = 0_usize;
    for line in text.lines() {
        lines += 1;
        if let Some((at_path, key, at)) = parse_stamp_line(line) {
            stamps.insert(at_path, (key, at));
        }
    }
    Ok((stamps, lines))
}

/// A time as `seconds.nanoseconds` since the Unix epoch.
pub(crate) fn time_text(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:09}", since.as_secs(), since.subsec_nanos())
}

pub(crate) fn parse_time(text: &str) -> Option<SystemTime> {
    let (secs, nanos) = text.split_once('.')?;
    Some(UNIX_EPOCH + Duration::new(secs.parse().ok()?, nanos.parse().ok()?))
}

/// The `cache NAME VERSION COMMIT` line of a workspace's state.
fn parse_held(text: &str) -> Option<HeldCache> {
    let mut fields = text.split(' ');
    let (name, version, commit) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    Some(HeldCache {
        name: name.to_owned(),
        version: version.to_owned(),
        commit: commit.parse().ok()?,
    })
}

/// The `created SECONDS.NANOS BASE` line of a workspace's state.
fn parse_created(text: &str) -> Option<(SystemTime, ObjectId)> {
    let (at, base) = text.split_once(' ')?;
    Some((parse_time(at)?, base.parse().ok()?))
}

/// The cache version the workspace `workspace` of the space in `space_dir` holds, read from
/// its state without opening the space, as a confined process needs it: its directory in
/// the space, relative to the space's directory.
///
/// # Errors
///
/// Fails if the workspace's state cannot be read or names a malformed version.
pub fn held_cache_version(space_dir: &Path, workspace: &str) -> Result<Option<PathBuf>> {
    let name = Name::try_from(workspace)?;
    let state_path = space_dir.join("workspaces").join(name.as_str()).join(STATE);
    let state =
        fs::read_to_string(&state_path).map_err(|e| Error::io("reading", &state_path, e))?;
    for line in state.lines() {
        if let Some(text) = line.strip_prefix("cache ") {
            let held = parse_held(text)
                .ok_or_else(|| Error::Corrupt(format!("workspace {workspace}: bad cache")))?;
            return Ok(Some(version_dir(Path::new(""), &held.version)?));
        }
    }
    Ok(None)
}

/// Whether `path` names a `.gitignore`, whose changes can change which paths are derived.
fn is_gitignore(path: &TreePath) -> bool {
    path.split_last()
        .is_some_and(|(_, name)| name.as_str() == GITIGNORE)
}

/// What kind of entry a file type is.
fn kind_of(file_type: fs::FileType) -> EntryKind {
    if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    }
}

/// Whether `path` is a directory itself, not a link to one.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Copies the cached entry at `from` into the overlay at `to`: a file with its mode and
/// dates, a link, or an empty directory with its mode. Read and written in plain calls, so
/// a confined process can copy too.
fn copy_cached(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).map_err(|e| Error::io("reading", from, e))?;
    let mode = fs::Permissions::from_mode(meta.permissions().mode() & 0o7777);
    if meta.is_dir() {
        make_dirs(to)?;
        return fs::set_permissions(to, mode).map_err(|e| Error::io("copying up", to, e));
    }
    if meta.is_symlink() {
        let target = fs::read_link(from).map_err(|e| Error::io("reading", from, e))?;
        return symlink(target, to).map_err(|e| Error::io("copying up", to, e));
    }
    let mut source = File::open(from).map_err(|e| Error::io("reading", from, e))?;
    let mut target = File::create_new(to).map_err(|e| Error::io("copying up", to, e))?;
    let mut buffer = vec![0; COPY_BUFFER];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|e| Error::io("reading", from, e))?;
        if read == 0 {
            break;
        }
        target
            .write_all(&buffer[..read])
            .map_err(|e| Error::io("copying up", to, e))?;
    }
    let mut times = FileTimes::new();
    if let Ok(modified) = meta.modified() {
        times = times.set_modified(modified);
    }
    if let Ok(accessed) = meta.accessed() {
        times = times.set_accessed(accessed);
    }
    target
        .set_permissions(mode)
        .and_then(|()| target.set_times(times))
        .map_err(|e| Error::io("copying up", to, e))
}

fn upper(dir: &Path) -> PathBuf {
    dir.join("upper")
}

impl Workspace {
    pub(crate) fn create(
        dir: PathBuf,
        name: Name,
        branch: Name,
        base: ObjectId,
        generation: Generation,
        cache: Option<HeldCache>,
    ) -> Result<Self> {
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
            seen: generation.state.load(Ordering::Acquire),
            seen_overlay: generation.overlay.load(Ordering::Acquire),
            generation,
            stamps: None,
            created: Some((SystemTime::now(), base)),
            cache,
            cache_whiteouts: BTreeSet::new(),
            dates: None,
            classes: RefCell::new(HashMap::new()),
            classes_rules: Cell::new(None),
        };
        let mut workspace = workspace;
        workspace.save()?;
        workspace.save_whiteouts()?;
        if let Some(parent) = workspace.dir.parent() {
            sync_dir(parent)?;
        }
        Ok(workspace)
    }

    pub(crate) fn open(dir: PathBuf, name: Name, generation: Generation) -> Result<Self> {
        let seen = generation.state.load(Ordering::Acquire);
        let seen_overlay = generation.overlay.load(Ordering::Acquire);
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
        let created = match fields.get("created") {
            Some(text) => Some(parse_created(text).ok_or_else(|| corrupt("creation"))?),
            None => None,
        };
        let cache = match fields.get("cache") {
            Some(text) => Some(parse_held(text).ok_or_else(|| corrupt("cache"))?),
            None => None,
        };
        let whiteouts = read_whiteouts(&dir.join(WHITEOUTS))?;
        let cache_whiteouts = read_whiteouts(&dir.join(CACHE_WHITEOUTS))?;
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
            generation,
            seen,
            seen_overlay,
            stamps: None,
            created,
            cache,
            cache_whiteouts,
            dates: None,
            classes: RefCell::new(HashMap::new()),
            classes_rules: Cell::new(None),
        })
    }

    /// The build cache version this workspace holds, if any.
    pub fn cache(&self) -> Option<&HeldCache> {
        self.cache.as_ref()
    }

    /// The held version's directory, if the workspace holds one.
    fn version_dir(&self) -> Option<PathBuf> {
        let held = self.cache.as_ref()?;
        let space_dir = self.dir.parent()?.parent()?;
        version_dir(space_dir, &held.version).ok()
    }

    /// Forgets the classes the cache layer worked out, as when a `.gitignore` changed. The
    /// owner's `classes` file is read again too.
    pub fn forget_classes(&self) {
        self.classes.borrow_mut().clear();
    }

    /// Whether this value still matches the workspace on disk: no other value of it from
    /// this space has changed its state or removals since this one last read or wrote them.
    /// The space's lock keeps other processes from changing them. Files in the overlay are
    /// always read from disk, so they never make a value stale.
    pub fn is_current(&self) -> bool {
        self.generation.state.load(Ordering::Acquire) == self.seen
    }

    /// Whether another value of this workspace has created, replaced, moved or removed
    /// overlay files since this one last did or asked, which makes overlay files held open
    /// from before then suspect. Such a change may have rewritten a `.gitignore`, so the
    /// classes this value worked out are forgotten too.
    pub fn overlay_changed_elsewhere(&mut self) -> bool {
        let now = self.generation.overlay.load(Ordering::Acquire);
        let changed = now != self.seen_overlay;
        self.seen_overlay = now;
        if changed {
            self.forget_classes();
        }
        changed
    }

    /// When the base entry `entry` at `path` is dated. With a held cache version whose
    /// commit has the same entry at `path`, as that version's `dates` have it, so an
    /// unchanged source is older than the output built from it. Otherwise when this
    /// workspace first saw that content there (when it was created, while its base is still
    /// the one it was created at), or when a commit took it from the overlay with that
    /// mtime. Kept in the workspace, so the date survives commits and remounts and changes
    /// only with the content.
    ///
    /// # Errors
    ///
    /// Fails if the workspace's stamps or its cache version's dates cannot be read, or the
    /// stamps cannot be written.
    pub fn stamp(&mut self, path: &TreePath, entry: &Entry) -> Result<SystemTime> {
        let key = entry_key(entry);
        if let Some(at) = self.cache_date(path, key)? {
            return Ok(at);
        }
        if let Some((known, at)) = self.stamps()?.get(path)
            && *known == key
        {
            return Ok(*at);
        }
        let at = match self.created {
            Some((created, base)) if base == self.base => created,
            _ => SystemTime::now(),
        };
        self.record_stamp(path, key, at)?;
        Ok(at)
    }

    /// The date the held version's `dates` give the entry with `key` at `path`, if its
    /// commit had that entry there.
    fn cache_date(&mut self, path: &TreePath, key: u64) -> Result<Option<SystemTime>> {
        if self.dates.is_none() {
            let Some(dir) = self.version_dir() else {
                return Ok(None);
            };
            self.dates = Some(read_stamps(&dir.join(DATES))?.0);
        }
        Ok(self
            .dates
            .as_ref()
            .and_then(|dates| dates.get(path))
            .filter(|(known, _)| *known == key)
            .map(|(_, at)| *at))
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

    fn stamps(&mut self) -> Result<&mut Stamps> {
        if self.stamps.is_none() {
            self.stamps = Some(self.load_stamps()?);
        }
        Ok(self.stamps.get_or_insert_with(HashMap::new))
    }

    /// Reads the stamp log, `seconds.nanoseconds key path` per line, the last line for a
    /// path winning; rewrites it when it has grown well past the stamps it holds.
    fn load_stamps(&self) -> Result<Stamps> {
        let (stamps, lines) = read_stamps(&self.dir.join(STAMPS))?;
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
        self.forget_classes();
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

    /// Counts a write to the state or whiteouts, which makes every other value stale.
    fn wrote(&mut self) {
        let now = self.generation.state.fetch_add(1, Ordering::AcqRel) + 1;
        // Another value's write since this one last read still makes it stale.
        if self.seen + 1 == now {
            self.seen = now;
        }
    }

    /// Counts a change to the overlay's files, which other values may hold open.
    fn changed_overlay(&mut self) {
        let now = self.generation.overlay.fetch_add(1, Ordering::AcqRel) + 1;
        // Another value's change since this one last looked still counts against it.
        if self.seen_overlay + 1 == now {
            self.seen_overlay = now;
        }
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
        let created = self
            .created
            .map(|(at, base)| format!("created {} {base}\n", time_text(at)))
            .unwrap_or_default();
        let cache = self
            .cache
            .as_ref()
            .map(|held| format!("cache {} {} {}\n", held.name, held.version, held.commit))
            .unwrap_or_default();
        let state = format!(
            "branch {}\nbase {}\nchange {}\n{snapshot}{merge}{created}{cache}",
            self.branch, self.base, self.change
        );
        write_synced(&self.dir, STATE, state.as_bytes())?;
        self.wrote();
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
        self.wrote();
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

    /// Forgets the removal at `path` now that a file or link is there again, when what it
    /// removed was a file or link too: the path was changed, not removed and added. A
    /// removed directory stays removed, so its entries stay hidden.
    fn replaced(&mut self, space: &Space, path: &TreePath) -> Result<()> {
        if !self.whiteouts.remove(path) {
            return Ok(());
        }
        let base = self.base_entry(space, path);
        self.whiteouts.insert(path.clone());
        if matches!(base?, Some(Entry::File { .. } | Entry::Symlink(_))) {
            self.drop_whiteout(path)?;
        }
        Ok(())
    }

    fn log_whiteout(&mut self, op: char, path: &TreePath) -> Result<()> {
        self.append_log(WHITEOUTS, &format!("{op}{path}\n"))
    }

    /// Records the removal of the held version's entry at `path`, durably. It is never
    /// forgotten: what the workspace writes there later is in its overlay, and the version
    /// never changes.
    fn add_cache_whiteout(&mut self, path: &TreePath) -> Result<()> {
        if self.cache_whiteouts.insert(path.clone()) {
            self.append_log(CACHE_WHITEOUTS, &format!("+{path}\n"))?;
        }
        Ok(())
    }

    fn append_log(&mut self, name: &str, line: &str) -> Result<()> {
        let log = self.dir.join(name);
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut file| {
                file.write_all(line.as_bytes())?;
                file.sync_data()
            })
            .map_err(|e| Error::io("writing", &log, e))?;
        self.wrote();
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

    fn base_view(&self, path: &TreePath) -> BaseView {
        if self.hidden(path) {
            BaseView::Hidden
        } else {
            BaseView::Shown
        }
    }

    fn base_entry(&self, space: &Space, path: &TreePath) -> Result<Option<Entry>> {
        self.base_entry_in(space, path, self.base_view(path))
    }

    fn base_entry_in(
        &self,
        space: &Space,
        path: &TreePath,
        view: BaseView,
    ) -> Result<Option<Entry>> {
        if view == BaseView::Hidden {
            return Ok(None);
        }
        space.entry(self.base_tree(space)?, path)
    }

    /// Whether the held version's entry at `path`, or one of its directories, was removed.
    /// Whether the held version has an entry at `path` that no cache whiteout hides,
    /// whether or not the layer shows it there.
    fn cache_holds(&self, path: &TreePath) -> bool {
        !self.cache_hidden(path)
            && self
                .cache_local(path)
                .is_some_and(|local| fs::symlink_metadata(local).is_ok())
    }

    fn cache_hidden(&self, path: &TreePath) -> bool {
        if self.cache_whiteouts.is_empty() {
            return false;
        }
        let mut prefix = TreePath::root();
        path.components().iter().any(|name| {
            prefix = prefix.join(name);
            self.cache_whiteouts.contains(&prefix)
        })
    }

    /// Where the held version keeps the entry at `path`, if the workspace holds one.
    fn cache_local(&self, path: &TreePath) -> Option<PathBuf> {
        let mut full = self.version_dir()?.join(FILES);
        for name in path.components() {
            full.push(name.as_str());
        }
        Some(full)
    }

    /// The held version's entry at `path`, when the workspace would show it there with no
    /// overlay entry at `path` itself: the base has nothing there, nothing removed it or its
    /// directories, and it is derived. `view` says whether the base is hidden at `path`.
    ///
    /// Callers check the overlay at `path` first; one that removes or moves an overlay entry
    /// uses this to learn whether a cached entry would show once it is gone.
    fn cached(&self, space: &Space, path: &TreePath, view: BaseView) -> Result<Option<PathBuf>> {
        if view == BaseView::Hidden || path.is_root() || self.cache_hidden(path) {
            return Ok(None);
        }
        let Some(local) = self.cache_local(path) else {
            return Ok(None);
        };
        let Ok(meta) = fs::symlink_metadata(&local) else {
            return Ok(None);
        };
        if self.cache_shows(space, path, kind_of(meta.file_type()))? {
            Ok(Some(local))
        } else {
            Ok(None)
        }
    }

    /// Whether the cache layer shows its entry of `kind` at `path`, which it holds: every
    /// directory on the way is a real directory in the version (never a link out of it) and
    /// a directory, or nothing, in the base; the base has nothing at `path`; and `path` is
    /// derived, or a source directory, which shows only that it leads to derived files. So
    /// a source or secret path never shows a cache file, and the cache never hides the base.
    fn cache_shows(&self, space: &Space, path: &TreePath, kind: EntryKind) -> Result<bool> {
        let Some(mut version) = self.cache_local(&TreePath::root()) else {
            return Ok(false);
        };
        let names = path.components();
        let mut base = Some(Entry::Tree(self.base_tree(space)?));
        for (index, name) in names.iter().enumerate() {
            base = match base {
                Some(Entry::Tree(id)) => space.shared_tree(id)?.get(name).cloned(),
                _ => None,
            };
            if index + 1 == names.len() {
                if base.is_some() {
                    return Ok(false);
                }
                break;
            }
            version.push(name.as_str());
            if !is_real_dir(&version)
                || matches!(base, Some(Entry::File { .. } | Entry::Symlink(_)))
            {
                return Ok(false);
            }
        }
        Ok(match self.class_of(space, path, kind)? {
            Class::Derived => true,
            Class::Source => kind == EntryKind::Directory,
            Class::Secret => false,
        })
    }

    /// The class of `path`, an entry of `kind`, given its directories' classes, which are
    /// worked out too. Remembered until [`Workspace::forget_classes`] or the owner's rules
    /// file changes.
    fn class_of(&self, space: &Space, path: &TreePath, kind: EntryKind) -> Result<Class> {
        // The owner may have edited their rules since, which no generation counts.
        let rules_now = space.owner_rules_stamp();
        if self.classes_rules.get() != rules_now {
            self.forget_classes();
            self.classes_rules.set(rules_now);
        }
        let names = path.components();
        let mut rules = None;
        let mut prefix = TreePath::root();
        for (index, name) in names.iter().enumerate() {
            prefix = prefix.join(name);
            let kind = if index + 1 == names.len() {
                kind
            } else {
                EntryKind::Directory
            };
            let key = (prefix.clone(), kind == EntryKind::Directory);
            let known = self.classes.borrow().get(&key).copied();
            let class = if let Some(class) = known {
                class
            } else {
                let rules = match &mut rules {
                    Some(rules) => rules,
                    None => rules.insert(self.classifier(space)?),
                };
                let class = rules.classify_child(&prefix, kind);
                self.classes.borrow_mut().insert(key, class);
                class
            };
            if class != Class::Source {
                return Ok(class);
            }
        }
        Ok(Class::Source)
    }

    /// Reads the file at `path`.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there, it is not a regular file, or on a store error.
    pub fn read(&self, space: &Space, path: &str) -> Result<Vec<u8>> {
        self.read_in(space, &TreePath::try_from(path)?, Layers::All)
    }

    fn read_in(&self, space: &Space, tree_path: &TreePath, layers: Layers) -> Result<Vec<u8>> {
        let local = self.upper_path(tree_path);
        let in_overlay = match fs::symlink_metadata(&local) {
            Ok(meta) if meta.is_file() => {
                return fs::read(&local).map_err(|e| Error::io("reading", local, e));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(Error::WrongEntry {
                    path: tree_path.to_string(),
                    found: "symlink",
                });
            }
            Ok(_) => true,
            Err(_) => false,
        };
        let view = self.base_view(tree_path);
        match self.base_entry_in(space, tree_path, view)? {
            Some(Entry::File { content, .. }) => return space.read_content(&content),
            Some(other) => {
                return Err(Error::WrongEntry {
                    path: tree_path.to_string(),
                    found: other.describe(),
                });
            }
            None => {}
        }
        if layers == Layers::All
            && !in_overlay
            && let Some(cached) = self.cached(space, tree_path, view)?
        {
            let meta =
                fs::symlink_metadata(&cached).map_err(|e| Error::io("reading", &cached, e))?;
            if meta.is_file() {
                return fs::read(&cached).map_err(|e| Error::io("reading", cached, e));
            }
            return Err(Error::WrongEntry {
                path: tree_path.to_string(),
                found: if meta.is_dir() {
                    "directory"
                } else {
                    "symlink"
                },
            });
        }
        Err(Error::NotFound(tree_path.to_string()))
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
        self.changed_overlay();
        let parent = self.upper_path(&parent_path);
        let local = self.upper_path(&tree_path);
        let file_mode = |path: &Path| match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => Some(mode_of(meta.permissions().mode())),
            _ => None,
        };
        let mode = if let Some(mode) = file_mode(&local) {
            mode
        } else {
            let view = self.base_view(&tree_path);
            match self.base_entry_in(space, &tree_path, view)? {
                Some(Entry::File { mode, .. }) => mode,
                Some(_) => FileMode::Regular,
                None => self
                    .cached(space, &tree_path, view)?
                    .and_then(|cached| file_mode(&cached))
                    .unwrap_or(FileMode::Regular),
            }
        };
        if is_gitignore(&tree_path) {
            self.forget_classes();
        }
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
        let view = self.base_view(path);
        if let Some(entry) = self.base_entry_in(space, path, view)? {
            return Ok(Some(Located::Base(entry)));
        }
        Ok(self.cached(space, path, view)?.map(Located::Cached))
    }

    /// Brings the entry at `path` into the overlay, if it is only in the base or the held
    /// cache version, and returns its local path. A directory is created empty; its base and
    /// cached entries stay visible. A cached file keeps its mode and dates.
    ///
    /// The copy is not synced: like any write through a mount, it is durable once synced.
    ///
    /// # Errors
    ///
    /// Fails if nothing is there, or on a store or file system error.
    pub fn copy_up(&mut self, space: &Space, path: &TreePath) -> Result<PathBuf> {
        let local = self.upper_path(path);
        let entry = match self.locate(space, path)? {
            Some(Located::Local(local)) => return Ok(local),
            Some(Located::Base(entry)) => entry,
            Some(Located::Cached(cached)) => {
                self.changed_overlay();
                if let Some((parent, _)) = path.split_last() {
                    make_dirs(&self.upper_path(&parent))?;
                }
                copy_cached(&cached, &local)?;
                if is_gitignore(path) {
                    self.forget_classes();
                }
                return Ok(local);
            }
            None => return Err(Error::NotFound(path.to_string())),
        };
        self.changed_overlay();
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
            Some(Located::Cached(cached)) if is_real_dir(&cached) => {
                make_dirs(&self.upper_path(&parent))?;
            }
            Some(Located::Local(local)) if local.is_dir() => {}
            Some(_) => {
                return Err(Error::WrongEntry {
                    path: parent.to_string(),
                    found: "file",
                });
            }
            None => return Err(Error::NotFound(parent.to_string())),
        }
        self.changed_overlay();
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
        if !matches!(new, NewEntry::Directory) {
            self.replaced(space, path)?;
        }
        if is_gitignore(path) {
            self.forget_classes();
        }
        Ok(local)
    }

    /// Makes `to` a hard link to the file or link at `from`, in a directory that exists,
    /// and returns its local path. A base entry is copied up first, so both paths share one
    /// overlay file: a write through either changes both, and a commit takes each path's
    /// content.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::WrongEntry`] if `from` is a directory, [`Error::Exists`] if
    /// something is at `to`, [`Error::NotFound`] if `from` or the directory of `to` is
    /// missing, or on a store or file system error.
    pub fn link(&mut self, space: &Space, from: &TreePath, to: &TreePath) -> Result<PathBuf> {
        let Some((to_parent, _)) = to.split_last() else {
            return Err(Error::Exists(to.to_string()));
        };
        let source_is_dir = match self.locate(space, from)? {
            Some(Located::Local(local) | Located::Cached(local)) => is_real_dir(&local),
            Some(Located::Base(entry)) => matches!(entry, Entry::Tree(_)),
            None => return Err(Error::NotFound(from.to_string())),
        };
        if source_is_dir {
            return Err(Error::WrongEntry {
                path: from.to_string(),
                found: "directory",
            });
        }
        if self.locate(space, to)?.is_some() {
            return Err(Error::Exists(to.to_string()));
        }
        match self.locate(space, &to_parent)? {
            Some(Located::Base(Entry::Tree(_))) => make_dirs(&self.upper_path(&to_parent))?,
            Some(Located::Cached(cached)) if is_real_dir(&cached) => {
                make_dirs(&self.upper_path(&to_parent))?;
            }
            Some(Located::Local(local)) if local.is_dir() => {}
            Some(_) => {
                return Err(Error::WrongEntry {
                    path: to_parent.to_string(),
                    found: "file",
                });
            }
            None => return Err(Error::NotFound(to_parent.to_string())),
        }
        let local_from = self.copy_up(space, from)?;
        let local_to = self.upper_path(to);
        self.changed_overlay();
        fs::hard_link(&local_from, &local_to).map_err(|e| Error::io("linking", &local_to, e))?;
        self.replaced(space, to)?;
        if is_gitignore(to) {
            self.forget_classes();
        }
        Ok(local_to)
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
        let view = self.base_view(from);
        let from_in_base = self.base_entry_in(space, from, view)?.is_some();
        // Under the overlay's own entry too, which would show again once that moved.
        let from_in_cache = !from_in_base && self.cached(space, from, view)?.is_some();
        let source_is_dir = match &source {
            Located::Local(local) | Located::Cached(local) => is_real_dir(local),
            Located::Base(entry) => matches!(entry, Entry::Tree(_)),
        };
        if source_is_dir && (from_in_base || from_in_cache) {
            return Err(Error::DirectoryRename(from.to_string()));
        }
        let Some((to_parent, _)) = to.split_last() else {
            return Err(Error::Exists(to.to_string()));
        };
        if let Some(existing) = self.locate(space, to)? {
            let existing_is_dir = match &existing {
                Located::Local(local) | Located::Cached(local) => is_real_dir(local),
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
            Some(Located::Cached(cached)) if is_real_dir(&cached) => {
                make_dirs(&self.upper_path(&to_parent))?;
            }
            Some(Located::Local(local)) if local.is_dir() => {}
            _ => return Err(Error::NotFound(to_parent.to_string())),
        }
        let local_to = self.upper_path(to);
        self.changed_overlay();
        fs::rename(&local_from, &local_to).map_err(|e| Error::io("renaming", &local_from, e))?;
        if from_in_base {
            self.add_whiteout(from)?;
        }
        if from_in_cache || (from_in_base && self.cache_holds(from)) {
            self.add_cache_whiteout(from)?;
        }
        if !source_is_dir {
            self.replaced(space, to)?;
        }
        if is_gitignore(from) || is_gitignore(to) {
            self.forget_classes();
        }
        Ok(())
    }

    fn remove_at(&mut self, space: &Space, tree_path: &TreePath) -> Result<()> {
        self.changed_overlay();
        let tree_path = tree_path.clone();
        let local = self.upper_path(&tree_path);
        let in_upper = match fs::symlink_metadata(&local) {
            Ok(meta) => {
                remove_local(&local, &meta)?;
                true
            }
            Err(_) => false,
        };
        let view = self.base_view(&tree_path);
        let in_base = self.base_entry_in(space, &tree_path, view)?.is_some();
        if in_base {
            self.add_whiteout(&tree_path)?;
        }
        let in_cache = !in_base && self.cached(space, &tree_path, view)?.is_some();
        // A base whiteout hides the cache's entries there too, but a commit drops it, so a
        // base directory that leads to cached files gets a cache whiteout as well.
        if in_cache || (in_base && self.cache_holds(&tree_path)) {
            self.add_cache_whiteout(&tree_path)?;
        }
        if is_gitignore(&tree_path) {
            self.forget_classes();
        }
        if !in_upper && !in_base && !in_cache {
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
        let view = self.base_view(&tree_path);
        match self.base_entry_in(space, &tree_path, view)? {
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
        let overlay = fs::symlink_metadata(&local).ok();
        if overlay.as_ref().is_some_and(fs::Metadata::is_dir) {
            found = true;
            for entry in fs::read_dir(&local).map_err(|e| Error::io("listing", &local, e))? {
                let entry = entry.map_err(|e| Error::io("listing", &local, e))?;
                let kind = entry
                    .file_type()
                    .map_err(|e| Error::io("reading", entry.path(), e))?;
                if let Some(name) = entry.file_name().to_str() {
                    entries.insert(name.to_owned(), kind_of(kind));
                }
            }
        }
        if let Some(cached) = self.cache_local(&tree_path)
            && is_real_dir(&cached)
        {
            if !found && overlay.is_none() && self.cached(space, &tree_path, view)?.is_some() {
                found = true;
            }
            if found {
                self.list_cached(space, &tree_path, &cached, &mut entries)?;
            }
        }
        if !found {
            return Err(Error::NotFound(tree_path.to_string()));
        }
        Ok(entries.into_iter().collect())
    }

    /// Adds the held version's entries in the directory `path`, kept at `cached`, that the
    /// workspace shows and the layers above do not have.
    fn list_cached(
        &self,
        space: &Space,
        path: &TreePath,
        cached: &Path,
        entries: &mut BTreeMap<String, EntryKind>,
    ) -> Result<()> {
        for entry in fs::read_dir(cached).map_err(|e| Error::io("listing", cached, e))? {
            let entry = entry.map_err(|e| Error::io("listing", cached, e))?;
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let Ok(child) = Name::try_from(name.as_str()).map(|name| path.join(&name)) else {
                continue;
            };
            if entries.contains_key(&name) {
                continue;
            }
            if self
                .cached(space, &child, self.base_view(&child))?
                .is_some()
            {
                let kind = entry
                    .file_type()
                    .map_err(|e| Error::io("reading", entry.path(), e))?;
                entries.insert(name, kind_of(kind));
            }
        }
        Ok(())
    }

    /// The date the workspace shows each file and link of its base, by path, with the key of
    /// its content: what a build cache version saves as its `dates`. Paths the overlay
    /// replaced or removed are left out.
    pub(crate) fn base_dates(&mut self, space: &Space) -> Result<Vec<(TreePath, u64, SystemTime)>> {
        let mut pending = vec![(TreePath::root(), self.base_tree(space)?)];
        let mut dates = Vec::new();
        while let Some((dir, tree)) = pending.pop() {
            for (name, entry) in space.shared_tree(tree)?.iter() {
                let path = dir.join(name);
                match self.locate(space, &path)? {
                    Some(Located::Base(Entry::Tree(id))) => pending.push((path, id)),
                    Some(Located::Local(local)) if is_real_dir(&local) => {
                        if let Entry::Tree(id) = entry {
                            pending.push((path, *id));
                        }
                    }
                    Some(Located::Base(entry)) => {
                        let at = self.stamp(&path, &entry)?;
                        dates.push((path, entry_key(&entry), at));
                    }
                    _ => {}
                }
            }
        }
        dates.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(dates)
    }

    /// The held version's entries the workspace shows, each with where the version keeps
    /// it, every directory before what it holds.
    pub(crate) fn shown_cached(&self, space: &Space) -> Result<Vec<(TreePath, PathBuf)>> {
        let mut shown = Vec::new();
        let Some(root) = self.cache_local(&TreePath::root()) else {
            return Ok(shown);
        };
        let mut pending = vec![(TreePath::root(), root)];
        while let Some((dir, local)) = pending.pop() {
            let mut children = fs::read_dir(&local)
                .map_err(|e| Error::io("listing", &local, e))?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|e| Error::io("listing", &local, e))?;
            children.sort_by_key(fs::DirEntry::file_name);
            for child in children {
                let Some(name) = child
                    .file_name()
                    .to_str()
                    .and_then(|name| Name::try_from(name).ok())
                else {
                    continue;
                };
                let path = dir.join(&name);
                let cached = child.path();
                let descend = match self.locate(space, &path)? {
                    Some(Located::Cached(found)) => {
                        shown.push((path.clone(), found));
                        is_real_dir(&cached)
                    }
                    Some(Located::Local(found)) => is_real_dir(&found) && is_real_dir(&cached),
                    Some(Located::Base(Entry::Tree(_))) => is_real_dir(&cached),
                    _ => false,
                };
                if descend {
                    pending.push((path, cached));
                }
            }
        }
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(shown)
    }

    /// Moves the overlay's outermost derived paths into `files`, at the same paths: a
    /// rename, so nothing is copied, unless `files` is on another file system. Returns
    /// them.
    pub(crate) fn take_derived(&mut self, space: &Space, files: &Path) -> Result<Vec<TreePath>> {
        let mut taken = Vec::new();
        for (path, class) in self.kept_local(space)? {
            if class != Class::Derived {
                continue;
            }
            let from = self.upper_path(&path);
            let mut to = files.to_owned();
            for name in path.components() {
                to.push(name.as_str());
            }
            if let Some(parent) = to.parent() {
                make_dirs(parent)?;
            }
            self.changed_overlay();
            match fs::rename(&from, &to) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                    copy_tree(&from, &to)?;
                    let meta =
                        fs::symlink_metadata(&from).map_err(|e| Error::io("reading", &from, e))?;
                    remove_local(&from, &meta)?;
                }
                Err(e) => return Err(Error::io("moving", &from, e)),
            }
            taken.push(path);
        }
        Ok(taken)
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
    /// `.gitignore` files the workspace shows in its overlay and base. One the cache layer
    /// shows is at a derived path already, so it decides nothing.
    fn classifier<'view>(&'view self, space: &'view Space) -> Result<Classifier<'view>> {
        let owner = space.owner_rules()?;
        Ok(Classifier::new(&owner, move |dir: &TreePath| {
            let file = Name::try_from(GITIGNORE).ok()?;
            self.read_in(space, &dir.join(&file), Layers::AboveCache)
                .ok()
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
        // The new base may hold other `.gitignore`s.
        self.forget_classes();
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
        self.forget_classes();
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

/// Reads a whiteout log: `+path` adds a whiteout, `-path` drops one, a bare path adds one.
/// A missing log has none.
fn read_whiteouts(path: &Path) -> Result<BTreeSet<TreePath>> {
    let mut whiteouts = BTreeSet::new();
    match fs::read_to_string(path) {
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
        Err(e) => return Err(Error::io("reading", path, e)),
    }
    Ok(whiteouts)
}

pub(crate) fn make_dirs(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| Error::io("creating", dir, e))
}

pub(crate) fn remove_local(path: &Path, meta: &fs::Metadata) -> Result<()> {
    let removed = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    removed.map_err(|e| Error::io("removing", path, e))
}

pub(crate) fn write_synced(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
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
