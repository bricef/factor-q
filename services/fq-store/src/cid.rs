use std::fmt;

use crate::error::StoreError;

/// A content identifier: the BLAKE3 hash (32 bytes) of an object's — or a
/// block's — bytes. Content-addressing means a `Cid` both names and verifies
/// its content.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cid([u8; 32]);

impl Cid {
    /// Compute the `Cid` of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Cid(*blake3::hash(bytes).as_bytes())
    }

    /// Wrap raw hash bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Cid(bytes)
    }

    /// The raw 32 hash bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding (64 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse a 64-char lowercase hex encoding.
    pub fn from_hex(s: &str) -> Result<Self, StoreError> {
        let bytes =
            hex::decode(s).map_err(|e| StoreError::Corrupt(format!("invalid cid hex: {e}")))?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| StoreError::Corrupt("cid must be 32 bytes".to_string()))?;
        Ok(Cid(array))
    }
}

impl fmt::Display for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cid({})", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_hex_roundtrips() {
        let a = Cid::of(b"hello");
        let b = Cid::of(b"hello");
        assert_eq!(a, b);
        assert_ne!(a, Cid::of(b"world"));
        assert_eq!(Cid::from_hex(&a.to_hex()).unwrap(), a);
    }
}
