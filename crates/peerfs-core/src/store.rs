//! Append-only packs of compressed objects, indexed in memory by identity.
//!
//! A pack is a sequence of records: a little-endian `u32` payload length, the kind code,
//! the codec code, the 33-byte object id, then the payload. Records are only appended. A
//! crash can leave a partial record at the end of the newest pack; opening the store cuts
//! it off. Every read is checked against the object's identity.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::codec::len_u64;
use crate::error::{Error, Result};
use crate::id::{ENCODED_LEN, ObjectId};
use crate::object::Kind;

/// A pack stops taking records once it is this large.
const PACK_TARGET: u64 = 16 * 1024 * 1024;

/// The zstd level for stored objects.
const ZSTD_LEVEL: i32 = 3;

const HEADER_LEN: usize = 4 + 1 + 1 + ENCODED_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Raw,
    Zstd,
}

impl Codec {
    const fn code(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Zstd => 1,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Raw),
            1 => Some(Self::Zstd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Location {
    pack: u32,
    offset: u64,
    len: u32,
    codec: Codec,
    kind: Kind,
}

#[derive(Debug)]
struct OpenPack {
    sequence: u32,
    file: File,
    len: u64,
    dirty: bool,
}

/// The objects of one space.
#[derive(Debug)]
pub(crate) struct Store {
    dir: PathBuf,
    index: HashMap<ObjectId, Location>,
    packs: Vec<u32>,
    open: Option<OpenPack>,
    new_pack: bool,
}

fn pack_name(sequence: u32) -> String {
    format!("pack-{sequence:08}.pack")
}

fn parse_pack_name(name: &str) -> Option<u32> {
    name.strip_prefix("pack-")?
        .strip_suffix(".pack")?
        .parse()
        .ok()
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).expect("record lengths are bounded by u32")
}

impl Store {
    /// Opens the store in `dir`, creating it if needed, and indexes every pack.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|e| Error::io("creating", dir, e))?;
        let mut packs = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| Error::io("listing", dir, e))? {
            let entry = entry.map_err(|e| Error::io("listing", dir, e))?;
            if let Some(sequence) = entry.file_name().to_str().and_then(parse_pack_name) {
                packs.push(sequence);
            }
        }
        packs.sort_unstable();
        let mut index = HashMap::new();
        let newest = packs.last().copied();
        for &sequence in &packs {
            let tail = if Some(sequence) == newest {
                Tail::Newest
            } else {
                Tail::Sealed
            };
            scan(dir, &mut index, sequence, tail)?;
        }
        Ok(Self {
            dir: dir.to_owned(),
            index,
            packs,
            open: None,
            new_pack: false,
        })
    }

    /// Whether the store holds `id`.
    pub(crate) fn contains(&self, id: &ObjectId) -> bool {
        self.index.contains_key(id)
    }

    /// The number of objects stored.
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    /// Stores `bytes` as an object of `kind` and returns its identity.
    ///
    /// The object is not durable until [`Store::sync`] returns.
    pub(crate) fn put(&mut self, kind: Kind, bytes: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::compute(kind, bytes);
        if self.contains(&id) {
            return Ok(id);
        }
        let compressed = zstd::bulk::compress(bytes, ZSTD_LEVEL)
            .map_err(|e| Error::io("compressing", &self.dir, e))?;
        let (codec, payload) = if compressed.len() < bytes.len() {
            (Codec::Zstd, compressed.as_slice())
        } else {
            (Codec::Raw, bytes)
        };
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| Error::Corrupt(format!("object of {} bytes is too large", bytes.len())))?;
        let mut record = Vec::with_capacity(HEADER_LEN + payload.len());
        record.extend_from_slice(&payload_len.to_le_bytes());
        record.push(kind.code());
        record.push(codec.code());
        record.extend_from_slice(&id.to_bytes());
        record.extend_from_slice(payload);

        self.ensure_pack(len_u64(record.len()))?;
        let Self { dir, open, .. } = self;
        let pack = open.as_mut().expect("ensure_pack opened a pack");
        let offset = pack.len;
        pack.file
            .write_all(&record)
            .map_err(|e| Error::io("appending to", dir.join(pack_name(pack.sequence)), e))?;
        pack.len += len_u64(record.len());
        pack.dirty = true;
        let sequence = pack.sequence;
        self.index.insert(
            id,
            Location {
                pack: sequence,
                offset: offset + len_u64(HEADER_LEN),
                len: payload_len,
                codec,
                kind,
            },
        );
        Ok(id)
    }

    fn ensure_pack(&mut self, record_len: u64) -> Result<()> {
        let full = self
            .open
            .as_ref()
            .is_some_and(|pack| pack.len > 0 && pack.len + record_len > PACK_TARGET);
        if full {
            self.sync()?;
            self.open = None;
        }
        if self.open.is_none() {
            let reuse = self.packs.last().copied().filter(|&sequence| {
                fs::metadata(self.dir.join(pack_name(sequence)))
                    .is_ok_and(|meta| meta.len() + record_len <= PACK_TARGET)
            });
            let sequence = if let Some(sequence) = reuse {
                sequence
            } else {
                let sequence = self.packs.last().map_or(0, |last| last + 1);
                self.packs.push(sequence);
                self.new_pack = true;
                sequence
            };
            let path = self.dir.join(pack_name(sequence));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| Error::io("opening", &path, e))?;
            let len = file
                .metadata()
                .map_err(|e| Error::io("reading", &path, e))?
                .len();
            self.open = Some(OpenPack {
                sequence,
                file,
                len,
                dirty: false,
            });
        }
        Ok(())
    }

    /// Makes every object stored so far durable.
    pub(crate) fn sync(&mut self) -> Result<()> {
        if let Some(pack) = self.open.as_mut().filter(|pack| pack.dirty) {
            pack.file
                .sync_data()
                .map_err(|e| Error::io("syncing", self.dir.join(pack_name(pack.sequence)), e))?;
            pack.dirty = false;
        }
        if self.new_pack {
            sync_dir(&self.dir)?;
            self.new_pack = false;
        }
        Ok(())
    }

    /// Reads `id`, checks it against its identity, and returns its kind and bytes.
    pub(crate) fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>> {
        let Some(location) = self.index.get(id) else {
            return Ok(None);
        };
        let path = self.dir.join(pack_name(location.pack));
        let file = File::open(&path).map_err(|e| Error::io("opening", &path, e))?;
        let mut payload = vec![0; to_usize(u64::from(location.len))];
        file.read_exact_at(&mut payload, location.offset)
            .map_err(|e| Error::io("reading", &path, e))?;
        let bytes = match location.codec {
            Codec::Raw => payload,
            Codec::Zstd => zstd::stream::decode_all(payload.as_slice())
                .map_err(|e| Error::Corrupt(format!("{id} does not decompress: {e}")))?,
        };
        if ObjectId::compute(location.kind, &bytes) != *id {
            return Err(Error::Corrupt(format!("{id} does not match its bytes")));
        }
        Ok(Some((location.kind, bytes)))
    }
}

/// Whether a pack may end in a partial record from a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// The newest pack: a partial record is cut off.
    Newest,
    /// An older pack: a partial record means corruption.
    Sealed,
}

/// Indexes one pack, cutting a partial record off the end of the newest.
fn scan(
    dir: &Path,
    index: &mut HashMap<ObjectId, Location>,
    sequence: u32,
    tail: Tail,
) -> Result<()> {
    let path = dir.join(pack_name(sequence));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| Error::io("opening", &path, e))?;
    let len = file
        .metadata()
        .map_err(|e| Error::io("reading", &path, e))?
        .len();
    let mut offset = 0;
    let mut header = [0; HEADER_LEN];
    while offset < len {
        let complete = offset + len_u64(HEADER_LEN) <= len
            && file.read_exact_at(&mut header, offset).is_ok()
            && {
                let payload = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                offset + len_u64(HEADER_LEN) + u64::from(payload) <= len
            };
        if !complete {
            if tail == Tail::Sealed {
                return Err(Error::Corrupt(format!(
                    "{} ends in a partial record at {offset}",
                    path.display()
                )));
            }
            file.set_len(offset)
                .and_then(|()| file.sync_all())
                .map_err(|e| Error::io("truncating", &path, e))?;
            break;
        }
        let payload = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let kind = Kind::from_code(header[4]).map_err(|_| {
            Error::Corrupt(format!(
                "{} has kind {} at {offset}",
                path.display(),
                header[4]
            ))
        })?;
        let codec = Codec::from_code(header[5]).ok_or_else(|| {
            Error::Corrupt(format!(
                "{} has codec {} at {offset}",
                path.display(),
                header[5]
            ))
        })?;
        let id = ObjectId::from_bytes(&header[6..])
            .map_err(|e| Error::Corrupt(format!("{} at {offset}: {e}", path.display())))?;
        index.entry(id).or_insert(Location {
            pack: sequence,
            offset: offset + len_u64(HEADER_LEN),
            len: payload,
            codec,
            kind,
        });
        offset += len_u64(HEADER_LEN) + u64::from(payload);
    }
    Ok(())
}

/// Makes a directory's entries durable, so a new or renamed file survives a crash.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| Error::io("syncing", dir, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_survive_reopening_and_a_torn_tail_is_cut() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut store = Store::open(dir.path()).expect("open");
        let a = store.put(Kind::Blob, b"first").expect("put");
        let b = store.put(Kind::Blob, &[7; 10_000]).expect("put");
        store.sync().expect("sync");
        drop(store);

        let pack = dir.path().join(pack_name(0));
        let mut file = OpenOptions::new().append(true).open(&pack).expect("pack");
        file.write_all(&[200, 0, 0, 0, 1, 0]).expect("torn record");
        drop(file);

        let store = Store::open(dir.path()).expect("reopen");
        assert_eq!(store.len(), 2);
        assert_eq!(
            store.get(&a).expect("read"),
            Some((Kind::Blob, b"first".to_vec()))
        );
        assert_eq!(
            store.get(&b).expect("read").map(|(_, bytes)| bytes.len()),
            Some(10_000)
        );
    }

    #[test]
    fn a_flipped_byte_is_reported_not_returned() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut store = Store::open(dir.path()).expect("open");
        let id = store
            .put(Kind::Blob, b"some incompressible text")
            .expect("put");
        store.sync().expect("sync");
        let pack = dir.path().join(pack_name(0));
        let mut bytes = fs::read(&pack).expect("pack");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&pack, bytes).expect("rewrite");
        assert!(matches!(store.get(&id), Err(Error::Corrupt(_))));
    }
}
