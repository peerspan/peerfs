//! Commit signatures: who signs a space's commits, and which keys are trusted for a host.
//!
//! A signed commit carries its signer's Ed25519 public key and a signature over
//! [`SIGNING_CONTEXT`] followed by the commit's unsigned encoding. The signature is part of
//! the commit's identity, so it cannot be stripped or replaced without changing the id.
//!
//! Peerfs is meant never to hold a host key: the host daemon signs through its signing
//! endpoint and knows its peers' keys from signed membership. Until that endpoint exists
//! (milestone 3b), a space signs with its own key in `signing.key` ([`KeyFile`]) and trusts
//! the keys listed in its `hosts` file ([`KnownHosts`]). Both sit behind traits, so the
//! daemon replaces them without touching the protocol.

use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::error::{Error, Result};
use crate::id::ObjectId;
use crate::object::Commit;
use crate::store::sync_dir;

/// Prefixes the bytes a commit signature covers, so a host key signing commits cannot be
/// made to sign anything else. A signing endpoint should refuse messages without it.
pub const SIGNING_CONTEXT: &[u8] = b"peerfs 2026-10-27 commit signature\0";

/// Bytes in a public key.
pub const PUBLIC_KEY_LEN: usize = 32;

/// Bytes in a signature.
pub const SIGNATURE_LEN: usize = 64;

/// An Ed25519 public key, written as standard Base64 as Peerspan writes keys.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicKey(pub [u8; PUBLIC_KEY_LEN]);

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&STANDARD.encode(self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

impl FromStr for PublicKey {
    type Err = ParseKeyError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bytes = STANDARD
            .decode(text)
            .map_err(|_| ParseKeyError(text.to_owned()))?;
        let bytes = <[u8; PUBLIC_KEY_LEN]>::try_from(bytes.as_slice())
            .map_err(|_| ParseKeyError(text.to_owned()))?;
        // Only the canonical spelling, so one key has one written form.
        if STANDARD.encode(bytes) != text {
            return Err(ParseKeyError(text.to_owned()));
        }
        Ok(Self(bytes))
    }
}

/// Text that is not a Base64 Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a Base64 Ed25519 public key")]
pub struct ParseKeyError(String);

/// A commit's signature and the key that made it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature {
    /// The signer's public key.
    pub key: PublicKey,
    /// The Ed25519 signature.
    pub bytes: [u8; SIGNATURE_LEN],
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature(by {})", self.key)
    }
}

/// Signs a space's commits on behalf of its host.
pub trait Signer: Send + fmt::Debug {
    /// The key signatures verify against.
    fn public_key(&self) -> PublicKey;

    /// Signs `message`, which starts with [`SIGNING_CONTEXT`].
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Signing`] when the key cannot be used.
    fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN]>;
}

/// Which keys sign for which hosts, for checking commits other hosts made.
pub trait HostKeys: Send + fmt::Debug {
    /// Whether `key` is a key of `host`.
    ///
    /// # Errors
    ///
    /// Fails when the keys cannot be read.
    fn is_key_of(&self, host: &str, key: &PublicKey) -> Result<bool>;
}

/// A signer holding its Ed25519 key in a file: standard Base64 of the 32-byte seed, mode
/// 0600. It stands in for the host daemon's signing endpoint.
pub struct KeyFile {
    key: SigningKey,
}

impl KeyFile {
    /// Creates a key at `path`, which must not exist.
    ///
    /// # Errors
    ///
    /// Fails if `path` exists or cannot be written.
    pub fn create(path: &Path) -> Result<Self> {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed)
            .map_err(|e| Error::Signing(format!("no random bytes for a key: {e}")))?;
        let key = SigningKey::from_bytes(&seed);
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::io("creating", path, e))?;
        file.write_all(format!("{}\n", STANDARD.encode(seed)).as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| Error::io("writing", path, e))?;
        if let Some(dir) = path.parent() {
            sync_dir(dir)?;
        }
        Ok(Self { key })
    }

    /// Loads the key at `path`.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or does not hold a key.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io("reading", path, e))?;
        let seed = STANDARD
            .decode(text.trim())
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
            .ok_or_else(|| Error::Signing(format!("{} does not hold a key", path.display())))?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }
}

impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyFile({})", self.public_key())
    }
}

impl Signer for KeyFile {
    fn public_key(&self) -> PublicKey {
        PublicKey(self.key.verifying_key().to_bytes())
    }

    fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN]> {
        use ed25519_dalek::Signer as _;
        Ok(self.key.sign(message).to_bytes())
    }
}

/// Host keys listed in a file, one `<host> <key>` per line. A host may have several keys.
#[derive(Debug)]
pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    /// The keys listed at `path`; a missing file lists none.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Every listed host and key, in file order.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or a line is not `<host> <key>`.
    pub fn list(&self) -> Result<Vec<(String, PublicKey)>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io("reading", &self.path, e)),
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                line.split_once(' ')
                    .and_then(|(host, key)| Some((host.to_owned(), key.trim().parse().ok()?)))
                    .ok_or_else(|| {
                        Error::Corrupt(format!("{}: bad line {line:?}", self.path.display()))
                    })
            })
            .collect()
    }

    /// Lists `key` as a key of `host`. Returns whether it was new.
    ///
    /// # Errors
    ///
    /// Fails if `host` is not a single word, or on a file system error.
    pub fn add(&self, host: &str, key: &PublicKey) -> Result<bool> {
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(Error::InvalidHost(host.to_owned()));
        }
        let mut all = self.list()?;
        if all.iter().any(|(h, k)| h == host && k == key) {
            return Ok(false);
        }
        all.push((host.to_owned(), *key));
        let text = all.iter().fold(String::new(), |mut text, (h, k)| {
            let _ = writeln!(text, "{h} {k}");
            text
        });
        let temporary = self.path.with_extension("new");
        fs::write(&temporary, text)
            .and_then(|()| File::open(&temporary)?.sync_all())
            .map_err(|e| Error::io("writing", &temporary, e))?;
        fs::rename(&temporary, &self.path).map_err(|e| Error::io("renaming", &temporary, e))?;
        if let Some(dir) = self.path.parent() {
            sync_dir(dir)?;
        }
        Ok(true)
    }
}

impl HostKeys for KnownHosts {
    fn is_key_of(&self, host: &str, key: &PublicKey) -> Result<bool> {
        Ok(self.list()?.iter().any(|(h, k)| h == host && k == key))
    }
}

/// Signs `commit` with `signer`, replacing any signature it had.
pub(crate) fn sign_commit(commit: &Commit, signer: &dyn Signer) -> Result<Commit> {
    let key = signer.public_key();
    let bytes = signer.sign(&commit.signing_message())?;
    let signature = Signature { key, bytes };
    if verify_signature(commit, &signature).is_err() {
        return Err(Error::Signing(format!(
            "the signer returned a signature that does not verify against its key {key}"
        )));
    }
    Ok(Commit {
        signature: Some(signature),
        ..commit.clone()
    })
}

/// Checks that a commit another host sent is signed, by a key of its author's host.
pub(crate) fn verify_commit(id: ObjectId, commit: &Commit, hosts: &dyn HostKeys) -> Result<()> {
    let signature = commit.signature.ok_or(Error::RemoteObject {
        id,
        problem: "is a commit without a signature",
    })?;
    verify_signature(commit, &signature).map_err(|()| Error::RemoteObject {
        id,
        problem: "is a commit whose signature does not verify",
    })?;
    if !hosts.is_key_of(&commit.author.host, &signature.key)? {
        return Err(Error::UnknownHostKey {
            commit: id,
            host: commit.author.host.clone(),
            key: signature.key,
        });
    }
    Ok(())
}

fn verify_signature(commit: &Commit, signature: &Signature) -> Result<(), ()> {
    let key = VerifyingKey::from_bytes(&signature.key.0).map_err(|_| ())?;
    let bytes = ed25519_dalek::Signature::from_bytes(&signature.bytes);
    // Strict verification rejects weak keys and non-canonical signatures, so a valid
    // signature has one encoding and the commit one identity.
    key.verify_strict(&commit.signing_message(), &bytes)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_have_one_written_form() {
        let key = PublicKey([0xfb; PUBLIC_KEY_LEN]);
        assert_eq!(key.to_string().parse(), Ok(key));
        let text = key.to_string();
        for bad in [
            "",
            "not base64",
            &text[..text.len() - 4],
            &format!("{text}\n"),
        ] {
            assert!(bad.parse::<PublicKey>().is_err(), "{bad:?}");
        }
        // Thirty-two zero bytes end in `A=`; `B=` decodes to the same bytes with stray bits.
        let zeros = PublicKey([0; PUBLIC_KEY_LEN]).to_string();
        assert!(zeros.ends_with("A="));
        let stray = format!("{}B=", &zeros[..zeros.len() - 2]);
        assert!(stray.parse::<PublicKey>().is_err());
    }

    #[test]
    fn known_hosts_list_what_was_added_once() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let hosts = KnownHosts::new(dir.path().join("hosts"));
        let (a, b) = (PublicKey([1; 32]), PublicKey([2; 32]));
        assert!(hosts.list().expect("empty").is_empty());
        assert!(hosts.add("laptop", &a).expect("add"));
        assert!(hosts.add("laptop", &b).expect("add"));
        assert!(!hosts.add("laptop", &a).expect("add"));
        assert!(hosts.is_key_of("laptop", &b).expect("check"));
        assert!(!hosts.is_key_of("server", &a).expect("check"));
        assert_eq!(
            hosts.list().expect("list"),
            vec![("laptop".to_owned(), a), ("laptop".to_owned(), b)]
        );
        for bad in ["", "two words", "tab\there"] {
            assert!(matches!(hosts.add(bad, &a), Err(Error::InvalidHost(_))));
        }
    }

    #[test]
    fn a_key_file_signs_what_it_loads_back() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("signing.key");
        let created = KeyFile::create(&path).expect("create");
        assert!(KeyFile::create(&path).is_err(), "never overwrites a key");
        let loaded = KeyFile::load(&path).expect("load");
        assert_eq!(created.public_key(), loaded.public_key());
        let mode = fs::metadata(&path).expect("metadata").permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
    }
}
