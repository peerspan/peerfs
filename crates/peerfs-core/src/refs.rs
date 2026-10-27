//! Branch references: one file per branch naming its head commit.
//!
//! A branch moves only by compare-and-swap, and each update is written to a temporary
//! file, synced and renamed into place, so a crash leaves either the old head or the new.

use std::fs;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::Name;
use crate::store::sync_dir;

#[derive(Debug)]
pub(crate) struct Refs {
    dir: PathBuf,
}

impl Refs {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn get(&self, branch: &Name) -> Result<Option<ObjectId>> {
        let path = self.dir.join(branch.as_str());
        match fs::read_to_string(&path) {
            Ok(text) => text
                .trim()
                .parse()
                .map(Some)
                .map_err(|e| Error::Corrupt(format!("branch {branch}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io("reading", path, e)),
        }
    }

    pub(crate) fn list(&self) -> Result<Vec<(Name, ObjectId)>> {
        let mut branches = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|e| Error::io("listing", &self.dir, e))? {
            let entry = entry.map_err(|e| Error::io("listing", &self.dir, e))?;
            let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|n| Name::try_from(n).ok())
            else {
                continue;
            };
            if name.as_str().starts_with('.') {
                continue;
            }
            if let Some(head) = self.get(&name)? {
                branches.push((name, head));
            }
        }
        branches.sort();
        Ok(branches)
    }

    /// Points `branch` at `new` if it still points at `expected` (`None`: does not exist).
    pub(crate) fn compare_and_swap(
        &self,
        branch: &Name,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<()> {
        let found = self.get(branch)?;
        if found != expected {
            return Err(Error::BranchMoved {
                branch: branch.to_string(),
                expected,
                found,
            });
        }
        let path = self.dir.join(branch.as_str());
        let temporary = self.dir.join(format!(".{branch}.new"));
        fs::write(&temporary, format!("{new}\n"))
            .and_then(|()| fs::File::open(&temporary)?.sync_all())
            .map_err(|e| Error::io("writing", &temporary, e))?;
        fs::rename(&temporary, &path).map_err(|e| Error::io("renaming", &temporary, e))?;
        sync_dir(&self.dir)
    }
}
