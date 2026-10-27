//! Paths inside a tree: sequences of valid names separated by `/`.

use std::fmt;

use crate::error::Error;
use crate::object::Name;

/// A path relative to a tree's root. The empty path is the root itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TreePath(Vec<Name>);

impl TreePath {
    /// The root.
    pub fn root() -> Self {
        Self::default()
    }

    /// Whether this is the root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The names from the root down.
    pub fn components(&self) -> &[Name] {
        &self.0
    }

    /// This path with `name` appended.
    #[must_use]
    pub fn join(&self, name: &Name) -> Self {
        let mut components = self.0.clone();
        components.push(name.clone());
        Self(components)
    }

    /// The parent directory and the last name, or `None` for the root.
    pub fn split_last(&self) -> Option<(Self, &Name)> {
        let (last, parent) = self.0.split_last()?;
        Some((Self(parent.to_vec()), last))
    }

    /// Whether `self` is `other` or lies inside it.
    pub fn starts_with(&self, other: &Self) -> bool {
        self.0.starts_with(&other.0)
    }
}

impl TryFrom<&str> for TreePath {
    type Error = Error;

    /// Parses `a/b/c`. Leading, trailing and doubled slashes are ignored, and `""` or `"."`
    /// is the root; `.` and `..` components are rejected.
    fn try_from(text: &str) -> Result<Self, Self::Error> {
        if text == "." {
            return Ok(Self::root());
        }
        text.split('/')
            .filter(|part| !part.is_empty())
            .map(|part| Name::try_from(part).map_err(|_| Error::InvalidPath(text.to_owned())))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

impl fmt::Display for TreePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str(".");
        }
        for (i, name) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            f.write_str(name.as_str())?;
        }
        Ok(())
    }
}
