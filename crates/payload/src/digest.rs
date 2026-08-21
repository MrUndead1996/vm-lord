use std::{fmt, io::Read, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::PayloadError;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sha256Digest {
    bytes: [u8; 32],
    hex: String,
}

impl Sha256Digest {
    pub fn as_hex(&self) -> &str {
        &self.hex
    }

    pub fn hash_reader(mut reader: impl Read) -> Result<Self, PayloadError> {
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer).map_err(|source| {
                PayloadError::io("hash input", std::path::PathBuf::new(), source)
            })?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Self::from_bytes(hash.finalize().into())
    }

    /// An incremental digest, for content that is not one reader.
    pub fn hasher() -> Sha256Hasher {
        Sha256Hasher(Sha256::new())
    }

    /// A digest from the 32 bytes a hash produced.
    ///
    /// Public because hashing happens wherever bytes flow -- the archive
    /// reader, the cache, staging -- and each of those is free to be in
    /// another crate.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, PayloadError> {
        let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { bytes, hex })
    }
}

/// A digest under construction, fed in pieces rather than read from one place.
///
/// `io::Write` so that `io::copy` can pour a file into it without a buffer of its own.
pub struct Sha256Hasher(Sha256);

impl Sha256Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// A digest is 32 bytes of hash output, so this cannot fail in practice.
    pub fn finish(self) -> Sha256Digest {
        Sha256Digest::from_bytes(self.0.finalize().into()).expect("a hash is 32 bytes")
    }
}

impl std::io::Write for Sha256Hasher {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl FromStr for Sha256Digest {
    type Err = PayloadError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PayloadError::InvalidDigest(value.to_owned()));
        }
        let mut bytes = [0; 32];
        for (index, part) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = u8::from_str_radix(std::str::from_utf8(part).expect("hex is ASCII"), 16)
                .map_err(|_| PayloadError::InvalidDigest(value.to_owned()))?;
        }
        Self::from_bytes(bytes)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex)
    }
}
impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex)
    }
}
impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256Digest;
    use std::str::FromStr;
    #[test]
    fn a_digest_is_normalized_and_displayed_as_lowercase_hex() {
        let digest = Sha256Digest::from_str(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .unwrap();
        assert_eq!(
            digest.as_hex(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }
}
