//! Pulling a branch's history from another host, object by object.
//!
//! A store is *closed*: it holds an object only if it holds every object that object
//! references. Every local write keeps this (blobs before chunk lists, subtrees before
//! trees, trees before commits), and a crash only cuts off the newest records. So holding a
//! commit means holding all of its history, and a pull fetches only what is missing.
//!
//! A pull stores fetched blobs as they arrive, since they reference nothing, and keeps the
//! other objects in memory until everything they reach is stored, then stores them children
//! first. The tracking reference is written last. A pull that fails part way leaves the
//! store closed and the reference unchanged; the next pull fetches the rest. A [`Pull`]
//! holds no borrow of the space between steps, so a server need not hold the space while
//! it waits on another host.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codec::{DecodeError, Decoder, Encoder, len_u64};
use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::{ChunkList, Commit, Content, Entry, Kind, Name, Tree};
use crate::store::{Store, sync_dir};

/// The most objects asked for in one fetch.
pub const FETCH_BATCH: usize = 256;

/// An object as stored: its identity, kind and canonical bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObject {
    /// Its identity.
    pub id: ObjectId,
    /// Its kind.
    pub kind: Kind,
    /// Its canonical bytes.
    pub bytes: Vec<u8>,
}

/// Another host's space, as a pull sees it.
pub trait Remote {
    /// The head of `branch` there, if it exists.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Remote`] when the host cannot be asked.
    fn branch(&mut self, branch: &str) -> Result<Option<ObjectId>>;

    /// The objects among `ids` that the host holds. Objects it lacks are left out.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Remote`] when the host cannot be asked.
    fn fetch(&mut self, ids: &[ObjectId]) -> Result<Vec<RawObject>>;
}

/// What a pull did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// The remote's head of the branch, now held locally with all its history.
    pub head: ObjectId,
    /// Objects fetched.
    pub objects: u64,
    /// Bytes of those objects, uncompressed.
    pub bytes: u64,
}

/// A local copy of another host's branch head, as of the last pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    /// The host it came from.
    pub remote: String,
    /// The branch there.
    pub branch: String,
    /// Its head when pulled.
    pub head: ObjectId,
    /// When it was pulled. The branch may have moved since.
    pub pulled: SystemTime,
}

/// Encodes objects for transfer: for each, its id, kind code, length and bytes.
pub fn encode_objects(objects: &[RawObject]) -> Vec<u8> {
    let mut out = Encoder::new();
    out.usize(objects.len());
    for object in objects {
        out.id(&object.id);
        out.u8(object.kind.code());
        out.bytes(&object.bytes);
    }
    out.finish()
}

/// Decodes objects encoded by [`encode_objects`]. Identities are checked by the pull.
///
/// # Errors
///
/// Fails if the bytes are not such an encoding.
pub fn decode_objects(bytes: &[u8]) -> Result<Vec<RawObject>, DecodeError> {
    let mut input = Decoder::new(bytes);
    let count = input.usize()?;
    let mut objects = Vec::with_capacity(count.min(FETCH_BATCH));
    for _ in 0..count {
        let id = input.id()?;
        let kind = Kind::from_code(input.u8()?)?;
        let bytes = input.bytes()?.to_vec();
        objects.push(RawObject { id, kind, bytes });
    }
    input.finish()?;
    Ok(objects)
}

/// The objects an object refers to, with the kind each must be.
pub(crate) fn references(id: ObjectId, kind: Kind, bytes: &[u8]) -> Result<Vec<(ObjectId, Kind)>> {
    let decode_error = |source| Error::Decode { kind, id, source };
    Ok(match kind {
        Kind::Blob => Vec::new(),
        Kind::File => ChunkList::decode(bytes)
            .map_err(decode_error)?
            .chunks
            .into_iter()
            .map(|chunk| (chunk, Kind::Blob))
            .collect(),
        Kind::Tree => Tree::decode(bytes)
            .map_err(decode_error)?
            .iter()
            .filter_map(|(_, entry)| match entry {
                Entry::File {
                    content: Content::Blob(id),
                    ..
                } => Some((*id, Kind::Blob)),
                Entry::File {
                    content: Content::Chunked(id),
                    ..
                } => Some((*id, Kind::File)),
                Entry::Tree(id) => Some((*id, Kind::Tree)),
                Entry::File {
                    content: Content::Empty,
                    ..
                }
                | Entry::Symlink(_) => None,
            })
            .collect(),
        Kind::Commit => {
            let commit = Commit::decode(bytes).map_err(decode_error)?;
            let mut refs = vec![(commit.tree, Kind::Tree)];
            refs.extend(
                commit
                    .parents
                    .into_iter()
                    .map(|parent| (parent, Kind::Commit)),
            );
            refs
        }
    })
}

/// A fetched object that cannot be stored until everything it references is.
#[derive(Debug)]
struct Pending {
    kind: Kind,
    bytes: Vec<u8>,
    references: Vec<ObjectId>,
}

/// A pull in progress: which objects are still wanted, and fetched objects waiting for
/// their references.
///
/// It does not borrow the space, so a server can release the space between steps while it
/// waits on the network: ask [`Pull::wanted`], fetch those objects from the remote, hand
/// them to [`Space::receive`](crate::Space::receive), and repeat until nothing is wanted; then [`Space::finish_pull`](crate::Space::finish_pull).
#[derive(Debug)]
pub struct Pull {
    remote: Name,
    branch: Name,
    head: ObjectId,
    queue: VecDeque<(ObjectId, Kind)>,
    in_flight: Vec<(ObjectId, Kind)>,
    seen: HashSet<ObjectId>,
    pending: HashMap<ObjectId, Pending>,
    discovered: Vec<ObjectId>,
    objects: u64,
    bytes: u64,
}

impl Pull {
    pub(crate) fn new(store: &Store, remote: Name, branch: Name, head: ObjectId) -> Self {
        let mut pull = Self {
            remote,
            branch,
            head,
            queue: VecDeque::new(),
            in_flight: Vec::new(),
            seen: HashSet::new(),
            pending: HashMap::new(),
            discovered: Vec::new(),
            objects: 0,
            bytes: 0,
        };
        if !store.contains(&head) {
            pull.queue.push_back((head, Kind::Commit));
            pull.seen.insert(head);
        }
        pull
    }

    /// The head being pulled.
    pub fn head(&self) -> ObjectId {
        self.head
    }

    /// The next objects to fetch, at most [`FETCH_BATCH`]; empty when the pull is complete.
    /// They are expected by the next [`Space::receive`](crate::Space::receive).
    pub fn wanted(&mut self) -> Vec<ObjectId> {
        if self.in_flight.is_empty() {
            let take = self.queue.len().min(FETCH_BATCH);
            self.in_flight = self.queue.drain(..take).collect();
        }
        self.in_flight.iter().map(|(id, _)| *id).collect()
    }

    pub(crate) fn receive(&mut self, store: &mut Store, objects: Vec<RawObject>) -> Result<()> {
        let mut got: HashMap<ObjectId, RawObject> = objects
            .into_iter()
            .map(|object| (object.id, object))
            .collect();
        for (id, kind) in std::mem::take(&mut self.in_flight) {
            let object = got.remove(&id).ok_or(Error::RemoteObject {
                id,
                problem: "is missing on the remote",
            })?;
            if object.kind != kind {
                return Err(Error::RemoteObject {
                    id,
                    problem: "has the wrong kind",
                });
            }
            if ObjectId::compute(kind, &object.bytes) != id {
                return Err(Error::RemoteObject {
                    id,
                    problem: "does not match its identity",
                });
            }
            self.objects += 1;
            self.bytes += len_u64(object.bytes.len());
            if kind == Kind::Blob {
                store.put(kind, &object.bytes)?;
                continue;
            }
            let mut references = Vec::new();
            for (child, child_kind) in self::references(id, kind, &object.bytes)? {
                if !store.contains(&child) {
                    references.push(child);
                    if self.seen.insert(child) {
                        self.queue.push_back((child, child_kind));
                    }
                }
            }
            self.discovered.push(id);
            self.pending.insert(
                id,
                Pending {
                    kind,
                    bytes: object.bytes,
                    references,
                },
            );
        }
        Ok(())
    }

    pub(crate) fn finish(mut self, store: &mut Store, tracking: &TrackingRefs) -> Result<Pulled> {
        if !self.queue.is_empty() || !self.in_flight.is_empty() {
            return Err(Error::Remote(
                "the pull finished before it was complete".to_owned(),
            ));
        }
        store_children_first(&self.discovered, &mut self.pending, store)?;
        store.sync()?;
        tracking.set(&self.remote, &self.branch, self.head)?;
        Ok(Pulled {
            head: self.head,
            objects: self.objects,
            bytes: self.bytes,
        })
    }
}

/// Stores pending objects so that each follows everything it references.
fn store_children_first(
    discovered: &[ObjectId],
    pending: &mut HashMap<ObjectId, Pending>,
    store: &mut Store,
) -> Result<()> {
    let order = children_first(discovered, |id| {
        pending.get(id).map(|object| object.references.as_slice())
    });
    for id in order {
        if let Some(object) = pending.remove(&id) {
            store.put(object.kind, &object.bytes)?;
        }
    }
    Ok(())
}

/// Orders `discovered` and the objects they reach among themselves so that each comes after
/// everything it references. `references` gives an object's references, or `None` for an
/// object outside the set.
fn children_first<'refs>(
    discovered: &[ObjectId],
    references: impl Fn(&ObjectId) -> Option<&'refs [ObjectId]>,
) -> Vec<ObjectId> {
    let mut order = Vec::with_capacity(discovered.len());
    let mut visited: HashSet<ObjectId> = HashSet::new();
    for root in discovered {
        if !visited.insert(*root) {
            continue;
        }
        let mut stack = vec![(*root, Visit::Enter)];
        while let Some((id, visit)) = stack.pop() {
            match visit {
                Visit::Enter => {
                    stack.push((id, Visit::Leave));
                    for child in references(&id).unwrap_or_default() {
                        if references(child).is_some() && visited.insert(*child) {
                            stack.push((*child, Visit::Enter));
                        }
                    }
                }
                Visit::Leave => order.push(id),
            }
        }
    }
    order
}

#[derive(Debug, Clone, Copy)]
enum Visit {
    Enter,
    Leave,
}

/// A push in progress: which objects to ask the receiver about, and which it lacks.
///
/// Like [`Pull`], it borrows no space between steps. Ask the receiver which of
/// [`Push::to_ask`] it holds, give the answers to [`Space::answer_push`](crate::Space::answer_push),
/// and repeat until nothing is left to ask; then send [`Space::push_order`](crate::Space::push_order)'s
/// objects in that order. The receiver's store being closed, an object it holds needs
/// nothing beneath it sent.
#[derive(Debug)]
pub struct Push {
    head: ObjectId,
    queue: VecDeque<ObjectId>,
    asked: Vec<ObjectId>,
    seen: HashSet<ObjectId>,
    missing: Vec<ObjectId>,
    references: HashMap<ObjectId, Vec<ObjectId>>,
}

impl Push {
    pub(crate) fn new(head: ObjectId) -> Self {
        Self {
            head,
            queue: VecDeque::from([head]),
            asked: Vec::new(),
            seen: HashSet::from([head]),
            missing: Vec::new(),
            references: HashMap::new(),
        }
    }

    /// The commit being pushed.
    pub fn head(&self) -> ObjectId {
        self.head
    }

    /// The next objects to ask the receiver about, at most [`FETCH_BATCH`]; empty when every
    /// missing object is known.
    pub fn to_ask(&mut self) -> Vec<ObjectId> {
        if self.asked.is_empty() {
            let take = self.queue.len().min(FETCH_BATCH);
            self.asked = self.queue.drain(..take).collect();
        }
        self.asked.clone()
    }

    pub(crate) fn answer(&mut self, store: &Store, held: &[bool]) -> Result<()> {
        let asked = std::mem::take(&mut self.asked);
        if held.len() != asked.len() {
            return Err(Error::Remote(format!(
                "asked about {} objects, answered {}",
                asked.len(),
                held.len()
            )));
        }
        for (id, held) in asked.into_iter().zip(held) {
            if *held {
                continue;
            }
            let (kind, bytes) = store.get(&id)?.ok_or(Error::Corrupt(format!(
                "{id} is referenced but missing here"
            )))?;
            let children: Vec<ObjectId> = references(id, kind, &bytes)?
                .into_iter()
                .map(|(child, _)| child)
                .collect();
            for child in &children {
                if self.seen.insert(*child) {
                    self.queue.push_back(*child);
                }
            }
            self.missing.push(id);
            self.references.insert(id, children);
        }
        Ok(())
    }

    /// The objects the receiver lacks, each after everything it references.
    pub(crate) fn order(&self) -> Result<Vec<ObjectId>> {
        if !self.queue.is_empty() || !self.asked.is_empty() {
            return Err(Error::Remote(
                "the push was ordered before it was complete".to_owned(),
            ));
        }
        Ok(children_first(&self.missing, |id| {
            self.references.get(id).map(Vec::as_slice)
        }))
    }
}

/// Stores objects another host sent, in order, refusing any that does not match its
/// identity or references an object not yet held, so the store stays closed.
pub(crate) fn accept_objects(store: &mut Store, objects: &[RawObject]) -> Result<u64> {
    let mut stored = 0;
    for object in objects {
        if ObjectId::compute(object.kind, &object.bytes) != object.id {
            return Err(Error::RemoteObject {
                id: object.id,
                problem: "does not match its identity",
            });
        }
        if store.contains(&object.id) {
            continue;
        }
        for (child, kind) in references(object.id, object.kind, &object.bytes)? {
            match store.kind_of(&child) {
                Some(found) if found == kind => {}
                Some(_) => {
                    return Err(Error::RemoteObject {
                        id: object.id,
                        problem: "references an object of the wrong kind",
                    });
                }
                None => {
                    return Err(Error::RemoteObject {
                        id: object.id,
                        problem: "references an object this space does not hold",
                    });
                }
            }
        }
        store.put(object.kind, &object.bytes)?;
        stored += 1;
    }
    store.sync()?;
    Ok(stored)
}

/// Tracking references: `refs/remotes/<remote>/<branch>`, holding a head and the time of
/// the pull that wrote it.
#[derive(Debug)]
pub(crate) struct TrackingRefs {
    dir: PathBuf,
}

impl TrackingRefs {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn get(&self, remote: &Name, branch: &Name) -> Result<Option<Tracking>> {
        let path = self.dir.join(remote.as_str()).join(branch.as_str());
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io("reading", path, e)),
        };
        let corrupt = || Error::Corrupt(format!("tracking reference {remote}/{branch}"));
        let mut lines = text.lines();
        let head = lines
            .next()
            .and_then(|line| line.parse().ok())
            .ok_or_else(corrupt)?;
        let millis: u64 = lines
            .next()
            .and_then(|line| line.parse().ok())
            .ok_or_else(corrupt)?;
        Ok(Some(Tracking {
            remote: remote.to_string(),
            branch: branch.to_string(),
            head,
            pulled: UNIX_EPOCH + Duration::from_millis(millis),
        }))
    }

    pub(crate) fn set(&self, remote: &Name, branch: &Name, head: ObjectId) -> Result<()> {
        let dir = self.dir.join(remote.as_str());
        fs::create_dir_all(&dir).map_err(|e| Error::io("creating", &dir, e))?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let temporary = dir.join(format!(".{branch}.new"));
        fs::write(&temporary, format!("{head}\n{millis}\n"))
            .and_then(|()| fs::File::open(&temporary)?.sync_all())
            .map_err(|e| Error::io("writing", &temporary, e))?;
        let path = dir.join(branch.as_str());
        fs::rename(&temporary, &path).map_err(|e| Error::io("renaming", &temporary, e))?;
        sync_dir(&dir)
    }

    pub(crate) fn list(&self) -> Result<Vec<Tracking>> {
        let mut all = Vec::new();
        let remotes = match fs::read_dir(&self.dir) {
            Ok(remotes) => remotes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(all),
            Err(e) => return Err(Error::io("listing", &self.dir, e)),
        };
        for remote in remotes {
            let remote = remote.map_err(|e| Error::io("listing", &self.dir, e))?;
            let Some(remote_name) = remote
                .file_name()
                .to_str()
                .and_then(|n| Name::try_from(n).ok())
            else {
                continue;
            };
            let dir = remote.path();
            for branch in fs::read_dir(&dir).map_err(|e| Error::io("listing", &dir, e))? {
                let branch = branch.map_err(|e| Error::io("listing", &dir, e))?;
                let Some(branch_name) = branch
                    .file_name()
                    .to_str()
                    .filter(|n| !n.starts_with('.'))
                    .and_then(|n| Name::try_from(n).ok())
                else {
                    continue;
                };
                if let Some(tracking) = self.get(&remote_name, &branch_name)? {
                    all.push(tracking);
                }
            }
        }
        all.sort_by(|a, b| (&a.remote, &a.branch).cmp(&(&b.remote, &b.branch)));
        Ok(all)
    }
}
