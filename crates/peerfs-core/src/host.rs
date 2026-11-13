//! Host-backed spaces (`peerfs space 3`): a space whose commits the Peerspan host daemon
//! signs with its host key, through its owner-local API, instead of a key file.
//!
//! The space keeps an immutable `host-binding.json`, written before its first commit,
//! naming the host binding it was prepared under and the host socket that signs for it. It
//! never holds or falls back to a key file: a `signing.key` or `hosts` file makes opening it
//! fail. While its first commit is being signed, `host-init.json` records the exact message,
//! so an interrupted initialization resumes with the same commit instead of making another.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::sign::PublicKey;
use crate::store::sync_dir;

/// The binding manifest's file name.
pub const BINDING_FILE: &str = "host-binding.json";

/// The pending initialization receipt's file name.
pub const INIT_RECEIPT_FILE: &str = "host-init.json";

/// The manifest's mode, the only one.
pub(crate) const MODE: &str = "peerspan-host-v1";

/// The longest Linux pathname a Unix socket can bind or connect to.
pub(crate) const MAX_SOCKET_PATH: usize = 107;

/// The longest message the host signs.
pub const MAX_SIGNING_MESSAGE: usize = 65_536;

/// A host-backed space's immutable binding to the host that signs its commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBinding {
    /// The logical space: `space_` and 32 lowercase hex digits.
    pub space_id: String,
    /// The registration lifetime: `fsbind_` and 32 lowercase hex digits.
    pub binding_id: String,
    /// The Peerspan network: `net_` and 64 lowercase hex digits.
    pub network_id: String,
    /// The network's context: `ctx_` and 64 lowercase hex digits.
    pub context_id: String,
    /// The signing host: `host_` and the lowercase hex SHA-256 of its public key.
    pub host_id: String,
    /// The host's Ed25519 public key.
    pub public_key: PublicKey,
    /// The host's owner-local socket, the one that signs.
    pub host_socket: PathBuf,
}

/// The manifest as it is written: exactly these fields.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u8,
    mode: String,
    space_id: String,
    binding_id: String,
    network_id: String,
    context_id: String,
    host_id: String,
    public_key: String,
    host_socket: String,
}

fn hex_id(id: &str, prefix: &str, digits: usize) -> bool {
    id.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == digits && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// `host_` and the lowercase hex SHA-256 of `key`, as Peerspan names hosts.
#[must_use]
pub fn host_id_of(key: &PublicKey) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(key.0);
    let mut id = String::from("host_");
    for byte in digest {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// Whether `path` is a socket path a host-backed space may name.
fn socket_path_ok(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\0') && path.len() <= MAX_SOCKET_PATH
}

fn invalid(dir: &Path, reason: impl Into<String>) -> Error {
    Error::NotASpace {
        path: dir.to_owned(),
        reason: reason.into(),
    }
}

impl HostBinding {
    /// Checks every field's form, and that the host ID names the public key.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::InvalidHost`] naming the first field that is malformed.
    pub fn validate(&self) -> Result<()> {
        let field = |ok: bool, name: &str| {
            if ok {
                Ok(())
            } else {
                Err(Error::InvalidHost(format!("host binding: invalid {name}")))
            }
        };
        field(hex_id(&self.space_id, "space_", 32), "space_id")?;
        field(hex_id(&self.binding_id, "fsbind_", 32), "binding_id")?;
        field(hex_id(&self.network_id, "net_", 64), "network_id")?;
        field(hex_id(&self.context_id, "ctx_", 64), "context_id")?;
        field(hex_id(&self.host_id, "host_", 64), "host_id")?;
        field(host_id_of(&self.public_key) == self.host_id, "public_key")?;
        field(
            self.host_socket.to_str().is_some_and(socket_path_ok),
            "host_socket",
        )
    }

    fn manifest(&self) -> Manifest {
        Manifest {
            format: 1,
            mode: MODE.to_owned(),
            space_id: self.space_id.clone(),
            binding_id: self.binding_id.clone(),
            network_id: self.network_id.clone(),
            context_id: self.context_id.clone(),
            host_id: self.host_id.clone(),
            public_key: self.public_key.to_string(),
            host_socket: self.host_socket.to_string_lossy().into_owned(),
        }
    }

    /// Writes the manifest into the space directory `dir`, mode 0600, and syncs it. It must
    /// not exist yet: the binding is immutable.
    pub(crate) fn write(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        let text = serde_json::to_string(&self.manifest())
            .map_err(|e| Error::Corrupt(format!("encoding the host binding: {e}")))?;
        write_new(&dir.join(BINDING_FILE), text.as_bytes())
    }

    /// Reads and checks the manifest of the space directory `dir`.
    ///
    /// # Errors
    ///
    /// Fails if it is missing, malformed, or names an invalid binding.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(BINDING_FILE);
        let text = fs::read_to_string(&path).map_err(|e| Error::io("reading", &path, e))?;
        let manifest: Manifest = serde_json::from_str(&text)
            .map_err(|_| invalid(dir, format!("{BINDING_FILE} is malformed")))?;
        if manifest.format != 1 || manifest.mode != MODE {
            return Err(invalid(
                dir,
                format!("{BINDING_FILE} has an unknown format"),
            ));
        }
        let public_key = manifest
            .public_key
            .parse()
            .map_err(|_| invalid(dir, format!("{BINDING_FILE} has an invalid public key")))?;
        let binding = Self {
            space_id: manifest.space_id,
            binding_id: manifest.binding_id,
            network_id: manifest.network_id,
            context_id: manifest.context_id,
            host_id: manifest.host_id,
            public_key,
            host_socket: PathBuf::from(manifest.host_socket),
        };
        binding
            .validate()
            .map_err(|e| invalid(dir, format!("{BINDING_FILE}: {e}")))?;
        Ok(binding)
    }
}

/// The receipt kept while a host-backed space's first commit is signed.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptFile {
    format: u8,
    binding_id: String,
    message_base64: String,
}

/// A pending initialization: the exact message the first commit is signed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitReceipt {
    pub binding_id: String,
    pub message: Vec<u8>,
}

impl InitReceipt {
    pub(crate) fn write(&self, dir: &Path) -> Result<()> {
        let text = serde_json::to_string(&ReceiptFile {
            format: 1,
            binding_id: self.binding_id.clone(),
            message_base64: STANDARD.encode(&self.message),
        })
        .map_err(|e| Error::Corrupt(format!("encoding the initialization receipt: {e}")))?;
        write_new(&dir.join(INIT_RECEIPT_FILE), text.as_bytes())
    }

    /// The receipt in `dir`, if there is one.
    pub(crate) fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(INIT_RECEIPT_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io("reading", &path, e)),
        };
        let damaged = || invalid(dir, format!("{INIT_RECEIPT_FILE} is damaged"));
        let file: ReceiptFile = serde_json::from_str(&text).map_err(|_| damaged())?;
        if file.format != 1 {
            return Err(damaged());
        }
        let message = STANDARD
            .decode(&file.message_base64)
            .map_err(|_| damaged())?;
        if STANDARD.encode(&message) != file.message_base64 || message.len() > MAX_SIGNING_MESSAGE {
            return Err(damaged());
        }
        Ok(Some(Self {
            binding_id: file.binding_id,
            message,
        }))
    }

    pub(crate) fn remove(dir: &Path) -> Result<()> {
        let path = dir.join(INIT_RECEIPT_FILE);
        fs::remove_file(&path).map_err(|e| Error::io("removing", &path, e))?;
        sync_dir(dir)
    }
}

/// Writes a new file with mode 0600, syncs it and its directory.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io("creating", path, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| Error::io("writing", path, e))?;
    match path.parent() {
        Some(dir) => sync_dir(dir),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Commit;

    /// RFC 8032 section 7.1 TEST 1, the contract's public test identity.
    const TEST_KEY: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    const TEST_HOST: &str = "host_21fe31dfa154a261626bf854046fd2271b7bed4b6abe45aa58877ef47f9721b9";

    pub(crate) fn binding() -> HostBinding {
        HostBinding {
            space_id: format!("space_{}", "a".repeat(32)),
            binding_id: format!("fsbind_{}", "b".repeat(32)),
            network_id: format!("net_{}", "c".repeat(64)),
            context_id: format!("ctx_{}", "d".repeat(64)),
            host_id: TEST_HOST.to_owned(),
            public_key: TEST_KEY.parse().unwrap(),
            host_socket: PathBuf::from("/run/peerspan/admin.sock"),
        }
    }

    #[test]
    fn host_id_is_the_key_hash() {
        assert_eq!(host_id_of(&TEST_KEY.parse().unwrap()), TEST_HOST);
    }

    #[test]
    fn manifest_round_trips_and_rejects_changes() {
        let dir = tempfile::tempdir().unwrap();
        let binding = binding();
        binding.write(dir.path()).unwrap();
        assert_eq!(HostBinding::load(dir.path()).unwrap(), binding);
        // Immutable: a second write is refused.
        assert!(binding.write(dir.path()).is_err());
        // An extra field, or a host ID that is not the key's, is refused.
        let path = dir.path().join(BINDING_FILE);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replacen('{', "{\"extra\":1,", 1)).unwrap();
        assert!(HostBinding::load(dir.path()).is_err());
        let mut wrong = binding;
        wrong.host_id = format!("host_{}", "0".repeat(64));
        assert!(wrong.validate().is_err());
    }

    /// A signer holding the contract's public test seed, standing in for the host.
    #[derive(Debug)]
    struct TestHost {
        key: ed25519_dalek::SigningKey,
        fail: std::sync::atomic::AtomicBool,
    }

    impl TestHost {
        fn new() -> Self {
            let seed: [u8; 32] = (0..32)
                .map(|i| u8::from_str_radix(&SEED[i * 2..i * 2 + 2], 16).unwrap())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            Self {
                key: ed25519_dalek::SigningKey::from_bytes(&seed),
                fail: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl crate::sign::Signer for TestHost {
        fn public_key(&self) -> PublicKey {
            PublicKey(self.key.verifying_key().to_bytes())
        }
        fn sign(&self, message: &[u8]) -> Result<[u8; crate::sign::SIGNATURE_LEN]> {
            use ed25519_dalek::Signer as _;
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::Signing("the host is unavailable".to_owned()));
            }
            Ok(self.key.sign(message).to_bytes())
        }
    }

    #[derive(Debug)]
    struct NoHosts;
    impl crate::sign::HostKeys for NoHosts {
        fn is_key_of(&self, _: &str, _: &PublicKey) -> Result<bool> {
            Ok(false)
        }
    }

    const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const VECTORS: &str = include_str!("testdata/peerfs-local-signing.json");

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2)
            .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    /// The host contract's public vectors: peerfs's commit encoding and signing message are
    /// exactly the bytes the host signs, and its signed encoding carries the signature.
    #[test]
    fn contract_vectors() {
        use crate::sign::Signer as _;
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        let host = TestHost::new();
        for vector in vectors["positive"].as_array().unwrap() {
            let unsigned = hex(vector["unsigned_commit_hex"].as_str().unwrap());
            let commit = Commit::decode(&unsigned).unwrap();
            assert!(commit.signature.is_none());
            assert_eq!(commit.encode(), unsigned, "{}", vector["name"]);
            let message = STANDARD
                .decode(vector["message_base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(commit.signing_message(), message, "{}", vector["name"]);
            let signature = host.sign(&message).unwrap();
            assert_eq!(
                STANDARD.encode(signature),
                vector["signature_base64"].as_str().unwrap(),
                "{}",
                vector["name"]
            );
            let signed = Commit {
                signature: Some(crate::sign::Signature {
                    key: host.public_key(),
                    bytes: signature,
                }),
                ..commit
            };
            assert_eq!(
                signed.encode(),
                hex(vector["signed_commit_hex"].as_str().unwrap()),
                "{}",
                vector["name"]
            );
        }
    }

    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn init_signs_the_first_commit_through_the_host() {
        use crate::Space;
        let dir = private_dir();
        let binding = binding();
        let space = Space::init_host(
            dir.path(),
            &binding,
            Box::new(TestHost::new()),
            Box::new(NoHosts),
            Some("codex".to_owned()),
            None,
        )
        .unwrap();
        let head = space.branch("main").unwrap().unwrap();
        let commit = space.commit(head).unwrap();
        assert_eq!(commit.author.host, binding.host_id);
        assert_eq!(commit.signature.unwrap().key, binding.public_key);
        assert_eq!(space.host_binding(), Some(&binding));
        drop(space);
        for gone in ["signing.key", "hosts", "HOST", INIT_RECEIPT_FILE] {
            assert!(!dir.path().join(gone).exists(), "{gone}");
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("FORMAT")).unwrap(),
            "peerfs space 3\n"
        );
        // The old opener refuses it; the host opener, with the binding's key, opens it.
        assert!(Space::open(dir.path()).is_err());
        let space =
            Space::open_host(dir.path(), Box::new(TestHost::new()), Box::new(NoHosts)).unwrap();
        assert_eq!(space.branch("main").unwrap(), Some(head));
        drop(space);
        // Initializing again under the same binding reports the space, changing nothing.
        let again = Space::init_host(
            dir.path(),
            &binding,
            Box::new(TestHost::new()),
            Box::new(NoHosts),
            None,
            None,
        )
        .unwrap();
        assert_eq!(again.branch("main").unwrap(), Some(head));
    }

    #[test]
    fn an_interrupted_init_resumes_with_the_same_commit() {
        use crate::Space;
        let dir = private_dir();
        let binding = binding();
        let host = TestHost::new();
        host.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            Space::init_host(
                dir.path(),
                &binding,
                Box::new(host),
                Box::new(NoHosts),
                None,
                None
            )
            .is_err()
        );
        let receipt = InitReceipt::load(dir.path()).unwrap().unwrap();
        // Not openable until the initialization finishes.
        assert!(
            Space::open_host(dir.path(), Box::new(TestHost::new()), Box::new(NoHosts)).is_err()
        );
        let space = Space::init_host(
            dir.path(),
            &binding,
            Box::new(TestHost::new()),
            Box::new(NoHosts),
            Some("other".to_owned()),
            None,
        )
        .unwrap();
        let commit = space
            .commit(space.branch("main").unwrap().unwrap())
            .unwrap();
        assert_eq!(
            Commit::signing_message(&Commit {
                signature: None,
                ..commit.clone()
            }),
            receipt.message,
            "the receipt fixed the first commit"
        );
        assert_eq!(commit.author.agent, None);
        assert!(!dir.path().join(INIT_RECEIPT_FILE).exists());
    }

    #[test]
    fn host_backed_spaces_refuse_key_files_and_other_bindings() {
        use crate::Space;
        let dir = private_dir();
        let binding = binding();
        drop(
            Space::init_host(
                dir.path(),
                &binding,
                Box::new(TestHost::new()),
                Box::new(NoHosts),
                None,
                None,
            )
            .unwrap(),
        );
        let mut other = binding.clone();
        other.binding_id = format!("fsbind_{}", "e".repeat(32));
        assert!(
            Space::init_host(
                dir.path(),
                &other,
                Box::new(TestHost::new()),
                Box::new(NoHosts),
                None,
                None
            )
            .is_err()
        );
        fs::write(dir.path().join("signing.key"), "x").unwrap();
        assert!(
            Space::open_host(dir.path(), Box::new(TestHost::new()), Box::new(NoHosts)).is_err()
        );
        fs::remove_file(dir.path().join("signing.key")).unwrap();
        let mut space =
            Space::open_host(dir.path(), Box::new(TestHost::new()), Box::new(NoHosts)).unwrap();
        assert!(
            space
                .add_known_host(&binding.host_id, &binding.public_key)
                .is_err()
        );
        assert!(!dir.path().join("hosts").exists());
        // A partial directory that never wrote a receipt is not a fresh space.
        let partial = private_dir();
        fs::write(partial.path().join("stray"), "x").unwrap();
        assert!(
            Space::init_host(
                partial.path(),
                &binding,
                Box::new(TestHost::new()),
                Box::new(NoHosts),
                None,
                None
            )
            .is_err()
        );
        assert!(partial.path().join("stray").exists());
    }

    #[test]
    fn socket_paths_are_bounded() {
        let mut long = binding();
        long.host_socket = PathBuf::from(format!("/{}", "s".repeat(MAX_SOCKET_PATH)));
        assert!(long.validate().is_err());
        let mut relative = binding();
        relative.host_socket = PathBuf::from("admin.sock");
        assert!(relative.validate().is_err());
    }
}
