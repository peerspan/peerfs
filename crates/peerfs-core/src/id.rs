//! Object identities: a hash algorithm code followed by the digest.

use std::fmt;
use std::str::FromStr;

use crate::object::Kind;

/// Bytes in a digest.
pub const DIGEST_LEN: usize = 32;

/// Bytes in an encoded [`ObjectId`]: the algorithm code, then the digest.
pub const ENCODED_LEN: usize = 1 + DIGEST_LEN;

/// The hash algorithm an [`ObjectId`] was computed with.
///
/// Every stored and transmitted identity carries its algorithm, so a later change of hash
/// adds a variant rather than rewriting every object that names another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Algorithm {
    /// BLAKE3-256 in key-derivation mode, with a context string per object kind.
    Blake3,
}

impl Algorithm {
    const fn code(self) -> u8 {
        match self {
            Self::Blake3 => 1,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Blake3),
            _ => None,
        }
    }
}

/// The identity of an immutable object: a hash of its kind and its canonical bytes.
///
/// Two objects of different kinds never share an identity, even with the same bytes,
/// because each kind hashes under its own context.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    algorithm: Algorithm,
    digest: [u8; DIGEST_LEN],
}

impl ObjectId {
    /// Computes the identity of an object of `kind` whose canonical bytes are `bytes`.
    pub fn compute(kind: Kind, bytes: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key(kind.hash_context());
        hasher.update(bytes);
        Self {
            algorithm: Algorithm::Blake3,
            digest: *hasher.finalize().as_bytes(),
        }
    }

    /// The algorithm this identity was computed with.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// The digest without its algorithm code.
    pub fn digest(&self) -> &[u8; DIGEST_LEN] {
        &self.digest
    }

    /// The encoded form: the algorithm code, then the digest.
    pub fn to_bytes(&self) -> [u8; ENCODED_LEN] {
        let mut bytes = [0; ENCODED_LEN];
        bytes[0] = self.algorithm.code();
        bytes[1..].copy_from_slice(&self.digest);
        bytes
    }

    /// Decodes an identity from its encoded form.
    ///
    /// # Errors
    ///
    /// Fails when `bytes` is not [`ENCODED_LEN`] long or names an unknown algorithm.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ParseIdError> {
        let Ok(bytes) = <&[u8; ENCODED_LEN]>::try_from(bytes) else {
            return Err(ParseIdError::Length(bytes.len()));
        };
        let algorithm = Algorithm::from_code(bytes[0]).ok_or(ParseIdError::Algorithm(bytes[0]))?;
        let mut digest = [0; DIGEST_LEN];
        digest.copy_from_slice(&bytes[1..]);
        Ok(Self { algorithm, digest })
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.to_bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl FromStr for ObjectId {
    type Err = ParseIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != 2 * ENCODED_LEN {
            return Err(ParseIdError::Length(text.len() / 2));
        }
        let mut bytes = [0; ENCODED_LEN];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| ParseIdError::Hex)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| ParseIdError::Hex)?;
        }
        Self::from_bytes(&bytes)
    }
}

/// An identity that could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseIdError {
    /// The encoding had the wrong number of bytes.
    #[error("object id is {0} bytes, not {ENCODED_LEN}")]
    Length(usize),
    /// The algorithm code is not one this version knows.
    #[error("unknown hash algorithm code {0}")]
    Algorithm(u8),
    /// The text form was not hexadecimal.
    #[error("object id is not hexadecimal")]
    Hex,
}
