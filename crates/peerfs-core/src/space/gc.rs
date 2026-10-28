//! Garbage collection for one space, with snapshot retention.
//!
//! Marking starts from what this host must keep: branches, tracking references, and each
//! workspace's base, merge and snapshots. Per-call snapshots are kept like a reflog: those
//! of a committed change while the commit naming them is younger than
//! [`GcOptions::reachable`], and any snapshot younger than [`GcOptions::unreachable`]. A
//! kept snapshot keeps the older ones it follows, so the store stays closed. Packs written
//! within [`GcOptions::grace`] are not touched, so objects of a pull or push that has not
//! yet moved a reference survive.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use super::Space;
use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{Commit, Kind};
use crate::store::Sweep;
use crate::sync::references;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// How a collection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcOptions {
    /// The time the periods count back from.
    pub now: SystemTime,
    /// How long a committed change's snapshots are kept after the commit.
    pub reachable: Duration,
    /// How long any snapshot is kept after it was taken.
    pub unreachable: Duration,
    /// Packs written more recently than this are left alone.
    pub grace: Duration,
    /// Report what would be dropped, and drop nothing.
    pub dry_run: bool,
}

impl Default for GcOptions {
    /// The default periods: 90 days for a committed change's snapshots, 30 for any, and a
    /// day's grace.
    fn default() -> Self {
        Self {
            now: SystemTime::now(),
            reachable: 90 * DAY,
            unreachable: 30 * DAY,
            grace: DAY,
            dry_run: false,
        }
    }
}

/// What a collection dropped, or would drop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcReport {
    /// Objects kept.
    pub objects_kept: usize,
    /// Objects dropped.
    pub objects_dropped: usize,
    /// Objects to drop that were left for now, being in packs written within the grace
    /// period.
    pub objects_waiting: usize,
    /// Bytes of packs freed, duplicates included.
    pub bytes_freed: u64,
    /// Packs rewritten.
    pub packs_rewritten: usize,
    /// Snapshots kept.
    pub snapshots_kept: usize,
    /// Snapshots dropped.
    pub snapshots_dropped: usize,
}

impl Space {
    /// Drops what this space no longer needs to keep, under its snapshot retention: see
    /// the module documentation. A dry run reports the same and changes nothing.
    ///
    /// # Errors
    ///
    /// Fails if a reference or workspace cannot be read, an object is corrupt, or on a file
    /// system error. Nothing is dropped unless marking finished.
    pub fn collect_garbage(&mut self, options: &GcOptions) -> Result<GcReport> {
        let since = |period: Duration| options.now.checked_sub(period).unwrap_or(options.now);
        let snapshots = self.snapshot_commits()?;
        let mut roots: Vec<ObjectId> = self.branches()?.into_iter().map(|(_, id)| id).collect();
        roots.extend(self.tracking()?.into_iter().map(|tracking| tracking.head));
        for name in self.workspaces()? {
            let workspace = self.workspace(&name)?;
            roots.push(workspace.base());
            roots.extend(workspace.merging());
            roots.extend(workspace.snapshot());
        }
        let recent = since(options.unreachable);
        roots.extend(
            snapshots
                .iter()
                .filter(|(_, time)| *time >= recent)
                .map(|(id, _)| *id),
        );
        let live = self.mark(roots, since(options.reachable))?;

        let sweep = if options.dry_run {
            Sweep::Report
        } else {
            Sweep::Apply
        };
        let compacted = self.store.compact(&live, since(options.grace), sweep)?;
        // Decoded trees of dropped objects would otherwise linger.
        self.trees.borrow_mut().clear();
        let dropped = |id: &ObjectId| compacted.dropped.contains(id);
        Ok(GcReport {
            objects_kept: live.iter().filter(|id| self.store.contains(id)).count(),
            objects_dropped: compacted.dropped.len(),
            objects_waiting: compacted.waiting.len(),
            bytes_freed: compacted.bytes,
            packs_rewritten: compacted.packs,
            snapshots_kept: snapshots.iter().filter(|(id, _)| live.contains(id)).count(),
            snapshots_dropped: snapshots.iter().filter(|(id, _)| dropped(id)).count(),
        })
    }

    /// Every unsigned commit held, which are the snapshots, with when each was taken.
    fn snapshot_commits(&self) -> Result<Vec<(ObjectId, SystemTime)>> {
        let commits: Vec<ObjectId> = self
            .store
            .objects()
            .filter(|(_, kind)| *kind == Kind::Commit)
            .map(|(id, _)| id)
            .collect();
        let mut snapshots = Vec::new();
        for id in commits {
            let commit = self.commit(id)?;
            if commit.signature.is_none() {
                snapshots.push((id, commit.time));
            }
        }
        Ok(snapshots)
    }

    /// Everything `roots` reach. A signed commit made after `predecessors_since` also
    /// reaches its predecessor, the last snapshot of its change.
    fn mark(
        &self,
        roots: Vec<ObjectId>,
        predecessors_since: SystemTime,
    ) -> Result<HashSet<ObjectId>> {
        let mut live = HashSet::new();
        let mut pending = roots;
        while let Some(id) = pending.pop() {
            if !live.insert(id) {
                continue;
            }
            // An object this store lacks has nothing to mark beneath it.
            let Some(kind) = self.store.kind_of(&id) else {
                continue;
            };
            if kind == Kind::Blob {
                continue;
            }
            let bytes = self.object(id, kind)?;
            if kind == Kind::Commit {
                let commit =
                    Commit::decode(&bytes).map_err(|source| Error::Decode { kind, id, source })?;
                if commit.signature.is_some() && commit.time >= predecessors_since {
                    pending.extend(commit.predecessor);
                }
            }
            for (child, _) in references(id, kind, &bytes)? {
                if !live.contains(&child) {
                    pending.push(child);
                }
            }
        }
        Ok(live)
    }
}
