//! Immutable objects: blobs, chunked files, trees and commits.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::id::ObjectId;
use crate::sign::{PUBLIC_KEY_LEN, PublicKey, SIGNATURE_LEN, SIGNING_CONTEXT, Signature};

/// What an object is. Each kind hashes under its own context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// File content stored whole, or one chunk of a larger file.
    Blob,
    /// The ordered chunks of a file too large to store whole.
    File,
    /// A directory listing.
    Tree,
    /// A snapshot of a tree with its history.
    Commit,
}

impl Kind {
    pub(crate) const fn hash_context(self) -> &'static str {
        match self {
            Self::Blob => "peerfs 2026-10-27 blob",
            Self::File => "peerfs 2026-10-27 file",
            Self::Tree => "peerfs 2026-10-27 tree",
            Self::Commit => "peerfs 2026-10-27 commit",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Blob => 1,
            Self::File => 2,
            Self::Tree => 3,
            Self::Commit => 4,
        }
    }

    pub(crate) fn from_code(code: u8) -> Result<Self, DecodeError> {
        match code {
            1 => Ok(Self::Blob),
            2 => Ok(Self::File),
            3 => Ok(Self::Tree),
            4 => Ok(Self::Commit),
            tag => Err(DecodeError::Tag { what: "kind", tag }),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blob => "blob",
            Self::File => "file",
            Self::Tree => "tree",
            Self::Commit => "commit",
        })
    }
}

/// One path component: not empty, not `.` or `..`, and without `/` or NUL.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

impl Name {
    /// The component as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for Name {
    type Error = InvalidNameError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        if text.is_empty() || text == "." || text == ".." || text.contains(['/', '\0']) {
            return Err(InvalidNameError(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A path component that cannot name an entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid name {0:?}")]
pub struct InvalidNameError(String);

/// Whether a regular file is executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum FileMode {
    /// Not executable.
    #[default]
    Regular,
    /// Executable.
    Executable,
}

/// Where a regular file's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Content {
    /// An empty file; nothing is stored.
    Empty,
    /// A file stored whole as one [`Kind::Blob`].
    Blob(ObjectId),
    /// A file stored as a [`Kind::File`] listing its chunks.
    Chunked(ObjectId),
}

/// One entry of a [`Tree`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Entry {
    /// A regular file.
    File {
        /// Its bytes.
        content: Content,
        /// Whether it is executable.
        mode: FileMode,
    },
    /// A symbolic link, with its target inline.
    Symlink(String),
    /// A subdirectory.
    Tree(ObjectId),
}

impl Entry {
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Symlink(_) => "symlink",
            Self::Tree(_) => "directory",
        }
    }
}

/// A directory: entries sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    entries: BTreeMap<Name, Entry>,
}

impl Tree {
    /// An empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// The entry called `name`, if any.
    pub fn get(&self, name: &Name) -> Option<&Entry> {
        self.entries.get(name)
    }

    /// Adds or replaces the entry called `name`.
    pub fn insert(&mut self, name: Name, entry: Entry) {
        self.entries.insert(name, entry);
    }

    /// Removes the entry called `name` and returns it.
    pub fn remove(&mut self, name: &Name) -> Option<Entry> {
        self.entries.remove(name)
    }

    /// The entries in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&Name, &Entry)> {
        self.entries.iter()
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the directory has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.usize(self.entries.len());
        for (name, entry) in &self.entries {
            out.str(name.as_str());
            match entry {
                Entry::File { content, mode } => {
                    out.u8(match mode {
                        FileMode::Regular => 1,
                        FileMode::Executable => 2,
                    });
                    match content {
                        Content::Empty => out.u8(0),
                        Content::Blob(id) => {
                            out.u8(1);
                            out.id(id);
                        }
                        Content::Chunked(id) => {
                            out.u8(2);
                            out.id(id);
                        }
                    }
                }
                Entry::Symlink(target) => {
                    out.u8(3);
                    out.str(target);
                }
                Entry::Tree(id) => {
                    out.u8(4);
                    out.id(id);
                }
            }
        }
        out.finish()
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(bytes);
        let count = input.usize()?;
        let mut tree = Self::new();
        let mut previous: Option<Name> = None;
        for _ in 0..count {
            let name = Name::try_from(input.string()?.as_str())
                .map_err(|error| DecodeError::Invalid(error.to_string()))?;
            if previous.as_ref().is_some_and(|previous| *previous >= name) {
                return Err(DecodeError::Invalid(format!("entry {name} out of order")));
            }
            let entry = match input.u8()? {
                tag @ (1 | 2) => {
                    let mode = if tag == 1 {
                        FileMode::Regular
                    } else {
                        FileMode::Executable
                    };
                    let content = match input.u8()? {
                        0 => Content::Empty,
                        1 => Content::Blob(input.id()?),
                        2 => Content::Chunked(input.id()?),
                        tag => {
                            return Err(DecodeError::Tag {
                                what: "content",
                                tag,
                            });
                        }
                    };
                    Entry::File { content, mode }
                }
                3 => Entry::Symlink(input.string()?),
                4 => Entry::Tree(input.id()?),
                tag => return Err(DecodeError::Tag { what: "entry", tag }),
            };
            previous = Some(name.clone());
            tree.insert(name, entry);
        }
        input.finish()?;
        Ok(tree)
    }
}

/// How a chunked file was split. Frozen: changing it changes every identity it produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chunking {
    /// `FastCDC` 2020 with level-1 normalization at 16 / 64 / 256 KiB.
    FastCdcV1,
}

impl Chunking {
    const fn code(self) -> u8 {
        match self {
            Self::FastCdcV1 => 1,
        }
    }

    fn from_code(code: u8) -> Result<Self, DecodeError> {
        match code {
            1 => Ok(Self::FastCdcV1),
            tag => Err(DecodeError::Tag {
                what: "chunking",
                tag,
            }),
        }
    }
}

/// The chunks of a file too large to store whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkList {
    pub(crate) chunking: Chunking,
    pub(crate) size: u64,
    pub(crate) chunks: Vec<ObjectId>,
}

impl ChunkList {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.u8(self.chunking.code());
        out.u64(self.size);
        out.usize(self.chunks.len());
        for chunk in &self.chunks {
            out.id(chunk);
        }
        out.finish()
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(bytes);
        let chunking = Chunking::from_code(input.u8()?)?;
        let size = input.u64()?;
        let count = input.usize()?;
        let chunks = (0..count)
            .map(|_| input.id())
            .collect::<Result<Vec<_>, _>>()?;
        input.finish()?;
        Ok(Self {
            chunking,
            size,
            chunks,
        })
    }
}

/// A stable identity for one piece of work across the commits that revise it.
///
/// Snapshots taken during a task share its change ID, so they can be related to the
/// commit that replaces them when the task is replicated.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangeId([u8; 16]);

impl ChangeId {
    /// A new random change ID.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot supply random bytes.
    pub fn random() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).expect("the operating system supplies random bytes");
        Self(bytes)
    }

    pub(crate) fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 32 {
            return None;
        }
        let mut bytes = [0; 16];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({self})")
    }
}

/// Who made a commit: a host, and the agent and session on it when known.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Author {
    /// The host that wrote the commit.
    pub host: String,
    /// The registered agent, when an agent made it.
    pub agent: Option<String>,
    /// The agent session, when known.
    pub session: Option<String>,
}

/// A snapshot of a tree, its parents and who made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The root tree.
    pub tree: ObjectId,
    /// The commits this one follows: none for a first commit, two after a merge.
    pub parents: Vec<ObjectId>,
    /// The piece of work this commit belongs to.
    pub change: ChangeId,
    /// Who made it.
    pub author: Author,
    /// When it was made.
    pub time: SystemTime,
    /// Free text describing it.
    pub message: String,
    /// The authoring host's signature, which the space adds when it stores the commit.
    pub signature: Option<Signature>,
}

const COMMIT_FORMAT: u8 = 2;

/// The signature algorithm code: Ed25519, the only one.
const ED25519: u8 = 1;

fn encode_optional(out: &mut Encoder, value: Option<&str>) {
    match value {
        None => out.u8(0),
        Some(text) => {
            out.u8(1);
            out.str(text);
        }
    }
}

fn decode_optional(input: &mut Decoder<'_>) -> Result<Option<String>, DecodeError> {
    match input.u8()? {
        0 => Ok(None),
        1 => Ok(Some(input.string()?)),
        tag => Err(DecodeError::Tag {
            what: "option",
            tag,
        }),
    }
}

impl Commit {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = self.encode_fields();
        match &self.signature {
            None => out.u8(0),
            Some(signature) => {
                out.u8(ED25519);
                out.bytes(&signature.key.0);
                out.bytes(&signature.bytes);
            }
        }
        out.finish()
    }

    /// What the authoring host signs: [`SIGNING_CONTEXT`], then the commit's encoding
    /// without a signature.
    pub(crate) fn signing_message(&self) -> Vec<u8> {
        let mut out = self.encode_fields();
        out.u8(0);
        let mut message = SIGNING_CONTEXT.to_vec();
        message.extend(out.finish());
        message
    }

    fn encode_fields(&self) -> Encoder {
        let millis = self
            .time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut out = Encoder::new();
        out.u8(COMMIT_FORMAT);
        out.id(&self.tree);
        out.usize(self.parents.len());
        for parent in &self.parents {
            out.id(parent);
        }
        out.bytes(&self.change.0);
        out.str(&self.author.host);
        encode_optional(&mut out, self.author.agent.as_deref());
        encode_optional(&mut out, self.author.session.as_deref());
        out.u64(u64::try_from(millis).unwrap_or(u64::MAX));
        out.str(&self.message);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(bytes);
        match input.u8()? {
            COMMIT_FORMAT => {}
            tag => {
                return Err(DecodeError::Tag {
                    what: "commit",
                    tag,
                });
            }
        }
        let tree = input.id()?;
        let count = input.usize()?;
        let parents = (0..count)
            .map(|_| input.id())
            .collect::<Result<Vec<_>, _>>()?;
        let change = <[u8; 16]>::try_from(input.bytes()?)
            .map(ChangeId)
            .map_err(|_| DecodeError::Invalid("change id is not 16 bytes".to_owned()))?;
        let author = Author {
            host: input.string()?,
            agent: decode_optional(&mut input)?,
            session: decode_optional(&mut input)?,
        };
        let time = UNIX_EPOCH + Duration::from_millis(input.u64()?);
        let message = input.string()?;
        let signature = match input.u8()? {
            0 => None,
            ED25519 => {
                let key = <[u8; PUBLIC_KEY_LEN]>::try_from(input.bytes()?)
                    .map_err(|_| DecodeError::Invalid("key is not 32 bytes".to_owned()))?;
                let bytes = <[u8; SIGNATURE_LEN]>::try_from(input.bytes()?)
                    .map_err(|_| DecodeError::Invalid("signature is not 64 bytes".to_owned()))?;
                Some(Signature {
                    key: PublicKey(key),
                    bytes,
                })
            }
            tag => {
                return Err(DecodeError::Tag {
                    what: "signature",
                    tag,
                });
            }
        };
        input.finish()?;
        Ok(Self {
            tree,
            parents,
            change,
            author,
            time,
            message,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        Name::try_from(text).expect("valid name")
    }

    #[test]
    fn trees_round_trip() {
        let blob = ObjectId::compute(Kind::Blob, b"hello");
        let mut tree = Tree::new();
        tree.insert(
            name("b"),
            Entry::File {
                content: Content::Blob(blob),
                mode: FileMode::Executable,
            },
        );
        tree.insert(
            name("a"),
            Entry::File {
                content: Content::Empty,
                mode: FileMode::Regular,
            },
        );
        tree.insert(name("link"), Entry::Symlink("b".to_owned()));
        tree.insert(name("sub"), Entry::Tree(blob));
        assert_eq!(Tree::decode(&tree.encode()), Ok(tree));
    }

    #[test]
    fn commits_round_trip_at_millisecond_precision() {
        let commit = Commit {
            tree: ObjectId::compute(Kind::Tree, b""),
            parents: vec![ObjectId::compute(Kind::Commit, b"x")],
            change: ChangeId::random(),
            author: Author {
                host: "laptop".to_owned(),
                agent: Some("claude".to_owned()),
                session: None,
            },
            time: UNIX_EPOCH + Duration::from_millis(1_793_000_000_123),
            message: "edit".to_owned(),
            signature: None,
        };
        assert_eq!(Commit::decode(&commit.encode()), Ok(commit.clone()));
        let signed = Commit {
            signature: Some(Signature {
                key: PublicKey([7; PUBLIC_KEY_LEN]),
                bytes: [9; SIGNATURE_LEN],
            }),
            ..commit
        };
        assert_eq!(Commit::decode(&signed.encode()), Ok(signed));
    }

    #[test]
    fn names_reject_separators_and_dots() {
        for bad in ["", ".", "..", "a/b", "nul\0"] {
            assert!(Name::try_from(bad).is_err(), "{bad:?}");
        }
        assert!(Name::try_from(".git").is_ok());
    }
}
