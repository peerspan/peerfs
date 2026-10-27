//! Editing a stored tree in memory, loading only the directories an edit reaches.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{Entry, Name, Tree};
use crate::path::TreePath;
use crate::space::Space;

#[derive(Debug)]
enum Node {
    Leaf(Entry),
    Stored(ObjectId),
    Loaded(Dir),
}

#[derive(Debug, Default)]
struct Dir {
    entries: BTreeMap<Name, Node>,
}

impl Dir {
    fn load(space: &Space, id: ObjectId) -> Result<Self> {
        let tree = space.tree(id)?;
        let entries = tree
            .iter()
            .map(|(name, entry)| {
                let node = match entry {
                    Entry::Tree(id) => Node::Stored(*id),
                    other => Node::Leaf(other.clone()),
                };
                (name.clone(), node)
            })
            .collect();
        Ok(Self { entries })
    }
}

/// A tree being edited.
#[derive(Debug)]
pub(crate) struct TreeEdit {
    root: Dir,
}

impl TreeEdit {
    pub(crate) fn new(space: &Space, root: ObjectId) -> Result<Self> {
        Ok(Self {
            root: Dir::load(space, root)?,
        })
    }

    /// The loaded directory at `path`, creating directories and replacing anything else in
    /// the way when `create` holds.
    fn dir_mut<'tree>(
        space: &Space,
        mut dir: &'tree mut Dir,
        path: &[Name],
        create: Create,
    ) -> Result<Option<&'tree mut Dir>> {
        for name in path {
            let node = match (dir.entries.contains_key(name), create) {
                (false, Create::No) => return Ok(None),
                (false, Create::Yes) => dir
                    .entries
                    .entry(name.clone())
                    .or_insert_with(|| Node::Loaded(Dir::default())),
                (true, _) => dir.entries.get_mut(name).expect("checked present"),
            };
            if let Node::Stored(id) = node {
                *node = Node::Loaded(Dir::load(space, *id)?);
            }
            if let Node::Leaf(_) = node {
                match create {
                    Create::No => return Ok(None),
                    Create::Yes => *node = Node::Loaded(Dir::default()),
                }
            }
            let Node::Loaded(next) = node else {
                unreachable!("stored and leaf nodes were replaced above")
            };
            dir = next;
        }
        Ok(Some(dir))
    }

    /// Removes whatever is at `path`. Returns whether something was there.
    pub(crate) fn remove(&mut self, space: &Space, path: &TreePath) -> Result<bool> {
        let Some((parent, name)) = path.split_last() else {
            let had = !self.root.entries.is_empty();
            self.root = Dir::default();
            return Ok(had);
        };
        let Some(dir) = Self::dir_mut(space, &mut self.root, parent.components(), Create::No)?
        else {
            return Ok(false);
        };
        Ok(dir.entries.remove(name).is_some())
    }

    /// Puts a file or link at `path`, creating its directories.
    pub(crate) fn insert(&mut self, space: &Space, path: &TreePath, entry: Entry) -> Result<()> {
        let Some((parent, name)) = path.split_last() else {
            return Err(Error::InvalidPath(path.to_string()));
        };
        let dir = Self::dir_mut(space, &mut self.root, parent.components(), Create::Yes)?
            .expect("directories are created on the way");
        let node = match entry {
            Entry::Tree(id) => Node::Stored(id),
            other => Node::Leaf(other),
        };
        dir.entries.insert(name.clone(), node);
        Ok(())
    }

    /// Writes every loaded directory and returns the root's identity.
    pub(crate) fn finish(self, space: &mut Space) -> Result<ObjectId> {
        persist(space, self.root)
    }
}

fn persist(space: &mut Space, dir: Dir) -> Result<ObjectId> {
    let mut tree = Tree::new();
    for (name, node) in dir.entries {
        let entry = match node {
            Node::Leaf(entry) => entry,
            Node::Stored(id) => Entry::Tree(id),
            Node::Loaded(dir) => Entry::Tree(persist(space, dir)?),
        };
        tree.insert(name, entry);
    }
    space.put_tree(&tree)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Create {
    No,
    Yes,
}
