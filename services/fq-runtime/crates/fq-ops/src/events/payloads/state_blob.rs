//! How a reducer state blob is written on the wire, and how it is read
//! (#981).
//!
//! `invocation.archived` carries the run's terminal state, which is the
//! whole conversation, tool results included. Serde's default for a byte
//! vector is a JSON array of integers: one to three digits and a comma
//! for every byte, about four bytes on the wire for each byte of state,
//! so one tool result at the 256 KiB cap made an archive larger than a
//! stock broker accepts. Base64 writes four bytes for every three.
//! (`serde_bytes`, which the field's documentation once proposed, would
//! not have helped: `serde_json` writes bytes as that same array.)
//!
//! The text is the standard alphabet with `=` padding (RFC 4648 section
//! 4): the canonical form, and the one other readers decode without
//! being told, since Go's `encoding/json` writes and reads a `[]byte`
//! exactly so, as do `base64 -d` and `jq`'s `@base64d`. URL-safe text
//! would buy nothing, because the blob never goes in a URL, and leaving
//! out the padding saves two bytes at most.
//!
//! Every build before this one wrote the array, and the event stream
//! keeps what it wrote for its retention window, so a reader takes
//! either form: text is decoded, and an array is read as it always was.
//! The writer emits only the text. Anything else is a malformed payload.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserializer, Serializer};

/// Write `bytes` as padded standard base64 text.
pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD.encode(bytes))
}

/// Read a state blob in either form: base64 text, or the array of
/// integers earlier builds wrote.
pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    deserializer.deserialize_any(EitherForm)
}

struct EitherForm;

impl<'de> Visitor<'de> for EitherForm {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a state blob: padded standard base64 text, or an array of byte values")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Vec<u8>, E> {
        STANDARD.decode(text).map_err(|err| {
            E::custom(format_args!(
                "a state blob that is not padded standard base64: {err}"
            ))
        })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        let mut bytes = Vec::new();
        while let Some(byte) = seq.next_element::<u8>()? {
            bytes.push(byte);
        }
        Ok(bytes)
    }
}
