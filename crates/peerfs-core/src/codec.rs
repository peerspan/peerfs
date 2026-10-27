//! Canonical binary encoding: LEB128 integers, length-prefixed bytes and object ids.
//!
//! An object's identity is the hash of its encoding, so every value has exactly one
//! encoding: integers are minimal, maps are written in sorted order by their callers.

use crate::id::{ENCODED_LEN, ObjectId, ParseIdError};

/// Widens a length to `u64`, which holds every `usize` on the targets peerfs supports.
pub(crate) fn len_u64(len: usize) -> u64 {
    u64::try_from(len).expect("usize fits in u64 on supported targets")
}

/// Appends canonical encodings to a buffer.
#[derive(Debug, Default)]
pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u64(&mut self, mut value: u64) {
        loop {
            let low = u8::try_from(value & 0x7f).expect("masked to seven bits");
            value >>= 7;
            if value == 0 {
                self.bytes.push(low);
                return;
            }
            self.bytes.push(low | 0x80);
        }
    }

    pub(crate) fn usize(&mut self, value: usize) {
        self.u64(len_u64(value));
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.usize(value.len());
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub(crate) fn id(&mut self, value: &ObjectId) {
        self.bytes.extend_from_slice(&value.to_bytes());
    }
}

/// Reads canonical encodings back, rejecting anything non-canonical.
#[derive(Debug)]
pub(crate) struct Decoder<'src> {
    rest: &'src [u8],
}

impl<'src> Decoder<'src> {
    pub(crate) fn new(bytes: &'src [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Fails unless every byte was consumed.
    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes(self.rest.len()))
        }
    }

    fn take(&mut self, len: usize) -> Result<&'src [u8], DecodeError> {
        if self.rest.len() < len {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(DecodeError::Overflow);
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(DecodeError::NonCanonical);
                }
                return Ok(value);
            }
        }
        Err(DecodeError::Overflow)
    }

    pub(crate) fn usize(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u64()?).map_err(|_| DecodeError::Overflow)
    }

    pub(crate) fn bytes(&mut self) -> Result<&'src [u8], DecodeError> {
        let len = self.usize()?;
        self.take(len)
    }

    pub(crate) fn string(&mut self) -> Result<String, DecodeError> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::Utf8)
    }

    pub(crate) fn id(&mut self) -> Result<ObjectId, DecodeError> {
        Ok(ObjectId::from_bytes(self.take(ENCODED_LEN)?)?)
    }
}

/// Bytes that are not a canonical encoding of the expected value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The bytes ended in the middle of a value.
    #[error("encoding ends early")]
    Truncated,
    /// Bytes were left after the value.
    #[error("{0} bytes after the end of the value")]
    TrailingBytes(usize),
    /// An integer does not fit its type.
    #[error("integer out of range")]
    Overflow,
    /// An integer was encoded with more bytes than it needs.
    #[error("integer encoded non-canonically")]
    NonCanonical,
    /// A string was not UTF-8.
    #[error("string is not UTF-8")]
    Utf8,
    /// A tag byte names no known variant.
    #[error("unknown {what} tag {tag}")]
    Tag {
        /// What the tag selects.
        what: &'static str,
        /// The tag found.
        tag: u8,
    },
    /// The value is well formed but breaks a rule of its type.
    #[error("invalid value: {0}")]
    Invalid(String),
    /// An embedded object id could not be decoded.
    #[error(transparent)]
    Id(#[from] ParseIdError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip_and_reject_padding() {
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut encoder = Encoder::new();
            encoder.u64(value);
            let bytes = encoder.finish();
            let mut decoder = Decoder::new(&bytes);
            assert_eq!(decoder.u64(), Ok(value), "value {value}");
            assert_eq!(decoder.finish(), Ok(()));
        }
        let mut decoder = Decoder::new(&[0x81, 0x00]);
        assert_eq!(decoder.u64(), Err(DecodeError::NonCanonical));
    }
}
