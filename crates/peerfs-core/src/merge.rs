//! Three-way merge of trees: whole subtrees when one side left them alone, lines of text
//! when both changed a file, and a [`Conflict`] for everything else.

use std::collections::BTreeSet;
use std::fmt;

use crate::error::Result;
use crate::id::ObjectId;
use crate::object::{Content, Entry, FileMode, Name, Tree};
use crate::path::TreePath;
use crate::space::Space;

/// The largest file merged line by line; larger files conflict as binary.
const TEXT_MERGE_LIMIT: usize = 1024 * 1024;

/// A path both sides changed in ways the merge could not combine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Where.
    pub path: TreePath,
    /// How.
    pub kind: ConflictKind,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.kind)
    }
}

/// How two changes to one path collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConflictKind {
    /// Both sides changed the same lines of a text file. The result holds markers.
    Content,
    /// Both sides created the same path with different text. The result holds markers.
    AddAdd,
    /// This side changed a file the other side deleted. The result keeps this side's.
    ModifyDelete,
    /// This side deleted a file the other side changed. The result keeps the other's.
    DeleteModify,
    /// Both sides changed a file that is not text, or too large to merge as text. The
    /// result keeps this side's.
    Binary,
    /// Both sides changed whether a file is executable, differently. The result keeps this
    /// side's.
    Mode,
    /// The sides disagree on whether a path is a file, a link or a directory, or both
    /// changed a link. The result keeps this side's.
    Type,
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Content => "both changed the same lines",
            Self::AddAdd => "both added different text",
            Self::ModifyDelete => "changed here, deleted there",
            Self::DeleteModify => "deleted here, changed there",
            Self::Binary => "both changed a binary file",
            Self::Mode => "both changed the mode",
            Self::Type => "the kinds of entry differ",
        })
    }
}

/// Merges `ours` and `theirs` against their common `base` and returns the merged tree and
/// every conflict. Conflicted text files hold conflict markers in the result.
pub(crate) fn merge_trees(
    space: &mut Space,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<(ObjectId, Vec<Conflict>)> {
    let mut conflicts = Vec::new();
    let base = base.map(|id| space.tree(id)).transpose()?;
    let ours = space.tree(ours)?;
    let theirs = space.tree(theirs)?;
    let merged = merge_dir(
        space,
        &TreePath::root(),
        base.as_ref(),
        &ours,
        &theirs,
        &mut conflicts,
    )?;
    Ok((space.put_tree(&merged)?, conflicts))
}

fn merge_dir(
    space: &mut Space,
    path: &TreePath,
    base: Option<&Tree>,
    ours: &Tree,
    theirs: &Tree,
    conflicts: &mut Vec<Conflict>,
) -> Result<Tree> {
    let names: BTreeSet<&Name> = ours
        .iter()
        .chain(theirs.iter())
        .chain(base.into_iter().flat_map(Tree::iter))
        .map(|(name, _)| name)
        .collect();
    let mut merged = Tree::new();
    for name in names {
        let entry = merge_entry(
            space,
            &path.join(name),
            base.and_then(|tree| tree.get(name)),
            ours.get(name),
            theirs.get(name),
            conflicts,
        )?;
        if let Some(entry) = entry {
            merged.insert(name.clone(), entry);
        }
    }
    Ok(merged)
}

fn merge_entry(
    space: &mut Space,
    path: &TreePath,
    base: Option<&Entry>,
    ours: Option<&Entry>,
    theirs: Option<&Entry>,
    conflicts: &mut Vec<Conflict>,
) -> Result<Option<Entry>> {
    if ours == theirs || theirs == base {
        return Ok(ours.cloned());
    }
    if ours == base {
        return Ok(theirs.cloned());
    }
    let mut conflict = |kind| {
        conflicts.push(Conflict {
            path: path.clone(),
            kind,
        });
    };
    let merged = match (ours, theirs) {
        (Some(Entry::Tree(ours)), Some(Entry::Tree(theirs))) => {
            let base = match base {
                Some(Entry::Tree(id)) => Some(space.tree(*id)?),
                _ => None,
            };
            let ours = space.tree(*ours)?;
            let theirs = space.tree(*theirs)?;
            let merged = merge_dir(space, path, base.as_ref(), &ours, &theirs, conflicts)?;
            Some(Entry::Tree(space.put_tree(&merged)?))
        }
        (
            Some(Entry::File {
                content: ours_content,
                mode: ours_mode,
            }),
            Some(Entry::File {
                content: theirs_content,
                mode: theirs_mode,
            }),
        ) => {
            let (base_content, base_mode) = match base {
                Some(Entry::File { content, mode }) => (Some(*content), Some(*mode)),
                _ => (None, None),
            };
            let mode = merge_mode(*ours_mode, *theirs_mode, base_mode, &mut conflict);
            let content = if ours_content == theirs_content || Some(*theirs_content) == base_content
            {
                *ours_content
            } else if Some(*ours_content) == base_content {
                *theirs_content
            } else {
                merge_text(
                    space,
                    base_content,
                    *ours_content,
                    *theirs_content,
                    &mut conflict,
                )?
            };
            Some(Entry::File { content, mode })
        }
        (Some(ours), None) => {
            conflict(ConflictKind::ModifyDelete);
            Some(ours.clone())
        }
        (None, Some(theirs)) => {
            conflict(ConflictKind::DeleteModify);
            Some(theirs.clone())
        }
        (Some(ours), Some(_)) => {
            conflict(ConflictKind::Type);
            Some(ours.clone())
        }
        (None, None) => None,
    };
    Ok(merged)
}

fn merge_mode(
    ours: FileMode,
    theirs: FileMode,
    base: Option<FileMode>,
    conflict: &mut impl FnMut(ConflictKind),
) -> FileMode {
    if ours == theirs || Some(theirs) == base {
        ours
    } else if Some(ours) == base {
        theirs
    } else {
        conflict(ConflictKind::Mode);
        ours
    }
}

fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.len() > TEXT_MERGE_LIMIT || bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn merge_text(
    space: &mut Space,
    base: Option<Content>,
    ours: Content,
    theirs: Content,
    conflict: &mut impl FnMut(ConflictKind),
) -> Result<Content> {
    let base_bytes = base
        .map(|content| space.read_content(&content))
        .transpose()?;
    let ours_bytes = space.read_content(&ours)?;
    let theirs_bytes = space.read_content(&theirs)?;
    let texts = (
        base_bytes.as_deref().map_or(Some(""), as_text),
        as_text(&ours_bytes),
        as_text(&theirs_bytes),
    );
    let (Some(base_text), Some(ours_text), Some(theirs_text)) = texts else {
        conflict(ConflictKind::Binary);
        return Ok(ours);
    };
    match diffy::merge(base_text, ours_text, theirs_text) {
        Ok(merged) => space.write_content(merged.as_bytes()),
        Err(marked) => {
            conflict(if base.is_some() {
                ConflictKind::Content
            } else {
                ConflictKind::AddAdd
            });
            space.write_content(marked.as_bytes())
        }
    }
}
