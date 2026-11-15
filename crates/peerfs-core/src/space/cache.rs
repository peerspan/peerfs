//! Build caches: the derived files of one build, kept by the space on this host and shown
//! read-only under the derived paths of the workspaces that hold them.
//!
//! A cache has a name and a set of versions, the newest of which is current. A version never
//! changes once saved. It holds `files`, the derived files and directories at their
//! workspace paths with their modes and dates; `commit`, whose tree they were built from;
//! and `dates`, the date the building workspace showed each file of that tree. Caches are
//! local: never pushed, pulled or replicated, and in no commit or snapshot.
//!
//! On disk, under `<space>/caches/`:
//!
//! - `names/<name>` names the cache's current version.
//! - `versions/<version>/` is a version: `meta` (its cache, commit, when it was saved and
//!   its size), `dates`, and `files/`. A workspace holding it names it in its state.
//! - `versions/.new-<version>/` is a version being saved.
//!
//! Saving builds the new version beside the others, makes it durable, and only then points
//! the name at it, so a crash leaves the earlier version current. A version no name points
//! at and no workspace holds is removed by the next save or removal, and by `gc`.

use std::collections::HashSet;
use std::fs::{self, File, FileTimes};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{Space, branch_name};
use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{ChangeId, Name};
use crate::store::sync_dir;
use crate::workspace::{
    HeldCache, Workspace, make_dirs, parse_time, remove_local, stamp_line, time_text, write_synced,
};

/// The space's directory of caches.
const CACHES: &str = "caches";
/// The caches' names, each naming its current version.
const NAMES: &str = "names";
/// Every version held.
const VERSIONS: &str = "versions";
/// A version's derived files.
pub(crate) const FILES: &str = "files";
/// A version's dates of its commit's files.
pub(crate) const DATES: &str = "dates";
/// A version's description.
const META: &str = "meta";
/// The start of a version's directory while it is saved.
const STAGING: &str = ".new-";

/// The longest cache name.
const NAME_LEN: usize = 128;

/// The directory of `version` in the space at `space_dir`.
///
/// # Errors
///
/// Fails if `version` is not a version's name: 32 lowercase hexadecimal digits.
pub(crate) fn version_dir(space_dir: &Path, version: &str) -> Result<PathBuf> {
    let valid = version.len() == 32
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(Error::Corrupt(format!("bad cache version {version:?}")));
    }
    Ok(space_dir.join(CACHES).join(VERSIONS).join(version))
}

/// Checks a cache name: 1 to 128 lowercase ASCII letters, digits, `.`, `_` and `-`,
/// starting with a letter or digit.
///
/// # Errors
///
/// Fails with [`Error::InvalidCacheName`] otherwise.
pub fn check_cache_name(name: &str) -> Result<()> {
    let allowed = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let valid = !name.is_empty()
        && name.len() <= NAME_LEN
        && name.bytes().next().is_some_and(allowed)
        && name
            .bytes()
            .all(|byte| allowed(byte) || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidCacheName(name.to_owned()))
    }
}

/// A cache, as its current version describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CacheInfo {
    /// The cache.
    pub name: String,
    /// Its current version.
    pub version: String,
    /// The commit whose tree the current version was built from.
    pub commit: ObjectId,
    /// When the current version was saved.
    pub saved_at: SystemTime,
    /// The bytes of the current version's files, a file with several links counted once.
    pub bytes: u64,
    /// The versions of this cache held: the current one, and earlier ones workspaces still
    /// hold.
    pub versions: usize,
}

/// A version's `meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Meta {
    cache: String,
    commit: ObjectId,
    saved_at: SystemTime,
    bytes: u64,
}

impl Meta {
    fn encode(&self) -> String {
        format!(
            "cache {}\ncommit {}\nsaved {}\nbytes {}\n",
            self.cache,
            self.commit,
            time_text(self.saved_at),
            self.bytes
        )
    }

    fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(META);
        let text = fs::read_to_string(&path).map_err(|e| Error::io("reading", &path, e))?;
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
        };
        let corrupt = || Error::Corrupt(format!("{}: malformed", path.display()));
        Ok(Self {
            cache: field("cache").ok_or_else(corrupt)?.to_owned(),
            commit: field("commit")
                .and_then(|text| text.parse().ok())
                .ok_or_else(corrupt)?,
            saved_at: field("saved").and_then(parse_time).ok_or_else(corrupt)?,
            bytes: field("bytes")
                .and_then(|text| text.parse().ok())
                .ok_or_else(corrupt)?,
        })
    }
}

impl Space {
    fn caches_dir(&self) -> PathBuf {
        self.dir.join(CACHES)
    }

    /// The current version of the cache `name`, if the cache exists.
    fn current_version(&self, name: &str) -> Result<Option<String>> {
        let path = self.caches_dir().join(NAMES).join(name);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io("reading", path, e)),
        }
    }

    /// The names of every version held, and each one's `meta`.
    fn versions(&self) -> Result<Vec<(String, Meta)>> {
        let dir = self.caches_dir().join(VERSIONS);
        let mut versions = Vec::new();
        for entry in list_names(&dir)? {
            if entry.starts_with(STAGING) {
                continue;
            }
            let path = version_dir(&self.dir, &entry)?;
            versions.push((entry, Meta::load(&path)?));
        }
        Ok(versions)
    }

    /// Every cache, by name, as its current version describes it.
    ///
    /// # Errors
    ///
    /// Fails if a cache's files cannot be read or are malformed.
    pub fn caches(&self) -> Result<Vec<CacheInfo>> {
        let versions = self.versions()?;
        let mut caches = Vec::new();
        for name in list_names(&self.caches_dir().join(NAMES))? {
            if name.starts_with('.') {
                continue;
            }
            let Some(current) = self.current_version(&name)? else {
                continue;
            };
            let meta = Meta::load(&version_dir(&self.dir, &current)?)?;
            caches.push(CacheInfo {
                versions: versions
                    .iter()
                    .filter(|(_, other)| other.cache == name)
                    .count(),
                name,
                version: current,
                commit: meta.commit,
                saved_at: meta.saved_at,
                bytes: meta.bytes,
            });
        }
        caches.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(caches)
    }

    /// The cache `name`, if it exists.
    ///
    /// # Errors
    ///
    /// Fails if the name is invalid, or the cache's files cannot be read or are malformed.
    pub fn cache(&self, name: &str) -> Result<Option<CacheInfo>> {
        check_cache_name(name)?;
        Ok(self.caches()?.into_iter().find(|cache| cache.name == name))
    }

    /// Creates a workspace that follows `branch`, starting at its head, holding the current
    /// version of the cache `cache` when one is named and it exists. A cache that does not
    /// exist is no error: the workspace holds none.
    ///
    /// # Errors
    ///
    /// Fails if the workspace exists, the branch does not, the cache's name is invalid, or
    /// on a file system error.
    pub fn create_workspace_with_cache(
        &mut self,
        workspace: &str,
        branch: &str,
        cache: Option<&str>,
    ) -> Result<Workspace> {
        let holding = match cache {
            Some(name) => {
                check_cache_name(name)?;
                match self.current_version(name)? {
                    Some(version) => {
                        let meta = Meta::load(&version_dir(&self.dir, &version)?)?;
                        Some(HeldCache {
                            name: name.to_owned(),
                            version,
                            commit: meta.commit,
                        })
                    }
                    None => None,
                }
            }
            None => None,
        };
        let name = Name::try_from(workspace)?;
        let head = self
            .branch(branch)?
            .ok_or_else(|| Error::NoSuchBranch(branch.to_owned()))?;
        let dir = self.workspace_dir(&name);
        if dir.exists() {
            return Err(Error::WorkspaceExists(workspace.to_owned()));
        }
        let generation = self.generation(&name);
        Workspace::create(dir, name, branch_name(branch)?, head, generation, holding)
    }

    /// Saves the derived files `workspace` shows, its overlay's and those of a cache version
    /// it holds, as a new version of the cache `name`, which becomes current; then discards
    /// the workspace. The overlay's files are moved into the version, and the held
    /// version's are linked into it, so nothing is copied. Secret-class paths are never
    /// taken, though a secret file inside a derived directory goes with it. The version's
    /// `commit` is the workspace's base, and its `dates` the dates the workspace shows its
    /// base files.
    ///
    /// The new version is durable before the name points at it, so a crash leaves the
    /// earlier version current. Earlier versions no workspace holds are removed afterwards.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::WorkspaceNotClean`] when the workspace has uncommitted source
    /// changes or an unfinished merge, since its output may have come from other sources
    /// than its base's; with [`Error::InvalidCacheName`]; or on a file system error. A
    /// failure after files were moved leaves the earlier version current and the workspace
    /// without them.
    pub fn save_cache(&mut self, name: &str, workspace: &str) -> Result<CacheInfo> {
        check_cache_name(name)?;
        let mut source = self.workspace(workspace)?;
        if source.merging().is_some() || !source.changes(self)?.is_empty() {
            return Err(Error::WorkspaceNotClean(workspace.to_owned()));
        }
        let caches = self.caches_dir();
        let versions = caches.join(VERSIONS);
        let names = caches.join(NAMES);
        for dir in [&versions, &names] {
            make_dirs(dir)?;
        }
        sync_dir(&caches)?;
        sync_dir(&self.dir)?;
        let version = ChangeId::random().to_string();
        let staging = versions.join(format!("{STAGING}{version}"));
        let files = staging.join(FILES);
        make_dirs(&files)?;

        let mut dates = String::new();
        for (path, key, at) in source.base_dates(self)? {
            // A name holding a line break cannot be listed; its file is dated afresh.
            if !path.to_string().contains(['\n', '\r']) {
                dates.push_str(&stamp_line(&path, key, at));
            }
        }
        write_synced(&staging, DATES, dates.as_bytes())?;
        let shown = source.shown_cached(self)?;
        source.take_derived(self, &files)?;
        link_cached(&shown, &files)?;

        let meta = Meta {
            cache: name.to_owned(),
            commit: source.base(),
            saved_at: SystemTime::now(),
            bytes: size_of_tree(&files)?,
        };
        write_synced(&staging, META, meta.encode().as_bytes())?;
        // Everything moved and linked in is durable before the version can be current.
        let staged = File::open(&staging).map_err(|e| Error::io("opening", &staging, e))?;
        rustix::fs::syncfs(&staged).map_err(|e| Error::io("syncing", &staging, e.into()))?;
        let target = versions.join(&version);
        fs::rename(&staging, &target).map_err(|e| Error::io("renaming", &staging, e))?;
        sync_dir(&versions)?;
        write_synced(&names, name, format!("{version}\n").as_bytes())?;

        drop(source);
        self.discard_workspace(workspace)?;
        self.drop_unheld_versions()?;
        self.cache(name)?
            .ok_or_else(|| Error::NoSuchCache(name.to_owned()))
    }

    /// Removes the cache `name`: new workspaces can no longer name it, and its versions go
    /// once no workspace holds them.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NoSuchCache`] if there is no such cache, or on a file system
    /// error.
    pub fn remove_cache(&mut self, name: &str) -> Result<()> {
        check_cache_name(name)?;
        let names = self.caches_dir().join(NAMES);
        let path = names.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NoSuchCache(name.to_owned()));
            }
            Err(e) => return Err(Error::io("removing", path, e)),
        }
        sync_dir(&names)?;
        self.drop_unheld_versions()?;
        Ok(())
    }

    /// The versions no name points at and no workspace holds, and versions left half saved.
    fn unheld_versions(&self) -> Result<Vec<PathBuf>> {
        let dir = self.caches_dir().join(VERSIONS);
        let mut held = HashSet::new();
        for name in list_names(&self.caches_dir().join(NAMES))? {
            if !name.starts_with('.')
                && let Some(version) = self.current_version(&name)?
            {
                held.insert(version);
            }
        }
        for workspace in self.workspaces()? {
            if let Some(cache) = self.workspace(&workspace)?.cache() {
                held.insert(cache.version.clone());
            }
        }
        Ok(list_names(&dir)?
            .into_iter()
            .filter(|version| !held.contains(version))
            .map(|version| dir.join(version))
            .collect())
    }

    /// Removes the versions no name points at and no workspace holds. Returns how many
    /// there were, not counting versions left half saved, which go too.
    pub(crate) fn drop_unheld_versions(&self) -> Result<usize> {
        let mut dropped = 0;
        for dir in self.unheld_versions()? {
            let staging = dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(STAGING));
            remove_tree(&dir)?;
            if !staging {
                dropped += 1;
            }
        }
        if dropped > 0 {
            sync_dir(&self.caches_dir().join(VERSIONS))?;
        }
        Ok(dropped)
    }

    /// How many versions [`Space::drop_unheld_versions`] would remove.
    pub(crate) fn count_unheld_versions(&self) -> Result<usize> {
        Ok(self
            .unheld_versions()?
            .iter()
            .filter(|dir| {
                !dir.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(STAGING))
            })
            .count())
    }
}

/// The names in `dir`, which may not exist.
fn list_names(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io("listing", dir, e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("listing", dir, e))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Links each of the held version's entries `shown`, directories first, into `files` at its
/// path: a hard link for a file, so the versions share it, a new link for a link, and a
/// directory with the same mode, set once everything is in it.
fn link_cached(shown: &[(crate::path::TreePath, PathBuf)], files: &Path) -> Result<()> {
    let mut dirs = Vec::new();
    for (path, cached) in shown {
        let mut target = files.to_owned();
        for name in path.components() {
            target.push(name.as_str());
        }
        let meta = fs::symlink_metadata(cached).map_err(|e| Error::io("reading", cached, e))?;
        if let Some(parent) = target.parent() {
            make_dirs(parent)?;
        }
        if meta.is_dir() {
            make_dirs(&target)?;
            dirs.push((target, meta.permissions().mode() & 0o7777));
        } else if meta.is_symlink() {
            let link = fs::read_link(cached).map_err(|e| Error::io("reading", cached, e))?;
            symlink(link, &target).map_err(|e| Error::io("linking", &target, e))?;
        } else if fs::hard_link(cached, &target).is_err() {
            copy_file(cached, &target, &meta)?;
        }
    }
    for (dir, mode) in dirs.iter().rev() {
        fs::set_permissions(dir, fs::Permissions::from_mode(*mode))
            .map_err(|e| Error::io("setting the mode of", dir, e))?;
    }
    Ok(())
}

/// Copies the file `from`, whose metadata is `meta`, to `to` with its mode and dates.
fn copy_file(from: &Path, to: &Path, meta: &fs::Metadata) -> Result<()> {
    fs::copy(from, to).map_err(|e| Error::io("copying", from, e))?;
    let mut times = FileTimes::new();
    if let Ok(modified) = meta.modified() {
        times = times.set_modified(modified);
    }
    if let Ok(accessed) = meta.accessed() {
        times = times.set_accessed(accessed);
    }
    File::options()
        .write(true)
        .open(to)
        .and_then(|file| file.set_times(times))
        .map_err(|e| Error::io("copying", to, e))
}

/// Copies the entry at `from`, and everything under it, to `to`, with modes and the files'
/// dates: for moving derived files to another file system.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).map_err(|e| Error::io("reading", from, e))?;
    if meta.is_symlink() {
        let link = fs::read_link(from).map_err(|e| Error::io("reading", from, e))?;
        return symlink(link, to).map_err(|e| Error::io("linking", to, e));
    }
    if !meta.is_dir() {
        return copy_file(from, to, &meta);
    }
    fs::create_dir(to).map_err(|e| Error::io("creating", to, e))?;
    for entry in fs::read_dir(from).map_err(|e| Error::io("listing", from, e))? {
        let entry = entry.map_err(|e| Error::io("listing", from, e))?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    fs::set_permissions(
        to,
        fs::Permissions::from_mode(meta.permissions().mode() & 0o7777),
    )
    .map_err(|e| Error::io("setting the mode of", to, e))
}

/// The bytes of the regular files under `dir`, each file counted once however many links
/// it has.
fn size_of_tree(dir: &Path) -> Result<u64> {
    let mut seen = HashSet::new();
    let mut bytes = 0_u64;
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| Error::io("listing", &dir, e))? {
            let entry = entry.map_err(|e| Error::io("listing", &dir, e))?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| Error::io("reading", &path, e))?;
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() && seen.insert((meta.dev(), meta.ino())) {
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    Ok(bytes)
}

/// Removes `path` and everything under it, making its directories writable first if that
/// is what stops it, as for a Go module cache, whose directories are read-only.
fn remove_tree(path: &Path) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    match remove_local(path, &meta) {
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable(path)?;
            remove_local(path, &meta)
        }
        other => other,
    }
}

/// Gives the owner write and search permission on `dir` and every directory under it.
fn make_writable(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir).map_err(|e| Error::io("reading", dir, e))?;
    if !meta.is_dir() {
        return Ok(());
    }
    let mode = meta.permissions().mode() | 0o700;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode))
        .map_err(|e| Error::io("setting the mode of", dir, e))?;
    for entry in fs::read_dir(dir).map_err(|e| Error::io("listing", dir, e))? {
        let entry = entry.map_err(|e| Error::io("listing", dir, e))?;
        make_writable(&entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_names_are_restricted() {
        for good in ["a", "bc-0123abcd", "x.y_z-1", &"a".repeat(128)] {
            assert!(check_cache_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-a",
            ".a",
            "_a",
            "A",
            "a/b",
            "a b",
            "é",
            &"a".repeat(129),
        ] {
            assert!(
                matches!(check_cache_name(bad), Err(Error::InvalidCacheName(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn meta_round_trips() {
        let meta = Meta {
            cache: "bc-1".to_owned(),
            commit: ObjectId::compute(crate::object::Kind::Commit, b"c"),
            saved_at: SystemTime::UNIX_EPOCH + std::time::Duration::new(1_793_000_000, 5),
            bytes: 42,
        };
        let dir = tempfile::tempdir().expect("temporary directory");
        fs::write(dir.path().join(META), meta.encode()).expect("write");
        assert_eq!(Meta::load(dir.path()).expect("load"), meta);
    }
}
