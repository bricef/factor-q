//! The wire boundary: bytes off the bus into an [`Event`], version first.
//!
//! Every reader of the event log used to hand the bytes straight to
//! serde, which made the envelope's `schema_version` a field that was
//! stamped on every event and inspected by nothing. The two ways a
//! message can fail to become an event were therefore one error, and
//! the consumers' poison policy — log and ack, because bytes that do
//! not parse never will — was applied to both. A version-skewed replay
//! (a stream holding v2 events read by a v3 binary) was acked away
//! event by event, and a projection rebuilt from that stream completed
//! "successfully" with a hole where the history had been
//! (<https://github.com/bricef/factor-q/issues/409>).
//!
//! So the boundary reads the version before the shape. Bytes that
//! declare a version this build reads are parsed with this build's
//! deserialisers and either become an event or are malformed. Bytes
//! that declare any other version are refused as *unsupported*: a
//! distinct error carrying what was found and what would have been
//! accepted. The caller decides what well-formed-but-unreadable history
//! means for it; for a durable consumer that is a halt, never an ack.

use serde::Deserialize;
use serde_json::Value;

use super::{Event, SCHEMA_VERSION};

/// The envelope versions this build reads: exactly the set its
/// deserialisers parse, which today is the current version alone.
///
/// There is no shim for an older version and none is planned while the
/// runtime is pre-alpha. A reader that learns to parse a second version
/// adds it here, and nothing downstream needs to know.
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[SCHEMA_VERSION];

/// Why bytes did not become an [`Event`].
///
/// Two variants because they call for opposite responses. Malformed
/// bytes will never parse, so a consumer acks them and moves on. An
/// unsupported version is well-formed history this build cannot read;
/// acking it would erase it from every projection built from this
/// point on, so a consumer stops instead.
#[derive(Debug, thiserror::Error)]
pub enum EventParseError {
    /// The envelope declares a version outside
    /// [`SUPPORTED_SCHEMA_VERSIONS`]. The body was not examined: a
    /// version this build does not read is refused before its shape is
    /// interpreted, so an older envelope that happens to parse against
    /// the current types is refused all the same.
    #[error("event schema_version {found} is not one this build reads (supported: {supported:?})")]
    UnsupportedSchemaVersion {
        found: u32,
        supported: &'static [u32],
        /// The `event_id` as the envelope spelled it, read by name so
        /// the message can be found on the stream. Nothing else about
        /// an envelope of that version is assumed, hence a string;
        /// `None` when it carried no id.
        event_id: Option<String>,
    },
    /// Not an event in any version: JSON that does not parse, a
    /// document with no `schema_version` anywhere, or a supported
    /// version whose body does not match its declared shape.
    #[error("malformed event: {0}")]
    Malformed(#[source] serde_json::Error),
}

/// The envelope fields the version check reads, at both of the places
/// the format has kept them.
///
/// v1 carried `schema_version` and `event_id` at the top level; the
/// `envelope` object arrived with v2. Where the version sits is part
/// of what changed between versions, so a probe that only looked
/// inside `envelope` would report a v1 event as malformed. The version
/// is the one thing every version of the format has agreed to carry,
/// and the probe looks wherever one has ever lived.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default)]
    envelope: Option<EnvelopeProbe>,
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    event_id: Option<Value>,
}

#[derive(Deserialize)]
struct EnvelopeProbe {
    schema_version: u32,
    #[serde(default)]
    event_id: Option<Value>,
}

impl VersionProbe {
    /// The declared version and id: the envelope's when there is one,
    /// else the v1 top level's. `None` when no version was declared
    /// anywhere, which is not an event in any version.
    fn declared(self) -> Option<(u32, Option<String>)> {
        let (version, id) = match self.envelope {
            Some(envelope) => (envelope.schema_version, envelope.event_id),
            None => (self.schema_version?, self.event_id),
        };
        Some((version, id.map(id_text)))
    }
}

/// An id as a log line would show it: a string verbatim, anything
/// else as its JSON.
fn id_text(id: Value) -> String {
    match id {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

impl Event {
    /// Read an event off the wire: the version first, then the shape.
    ///
    /// The one way bytes become an [`Event`] for anything that consumes
    /// the log; see the module doc for why it is not a plain
    /// `serde_json::from_slice`.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, EventParseError> {
        let Some((found, event_id)) = probe_version(bytes)? else {
            return Err(EventParseError::Malformed(serde::de::Error::missing_field(
                "schema_version",
            )));
        };
        if !SUPPORTED_SCHEMA_VERSIONS.contains(&found) {
            return Err(EventParseError::UnsupportedSchemaVersion {
                found,
                supported: SUPPORTED_SCHEMA_VERSIONS,
                event_id,
            });
        }
        serde_json::from_slice(bytes).map_err(EventParseError::Malformed)
    }
}

/// The version and id `bytes` declare, looked for wherever a version
/// has ever lived; `Ok(None)` for JSON that declares no version, `Err`
/// for bytes that are not JSON at all. The one probe behind
/// [`Event::from_wire`] and [`declared_schema_version`].
fn probe_version(bytes: &[u8]) -> Result<Option<(u32, Option<String>)>, EventParseError> {
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(EventParseError::Malformed)?;
    Ok(probe.declared())
}

/// The envelope version `bytes` declare, and nothing else about them:
/// `None` for bytes that are not an event in any version. The body is
/// not examined and no shape is assumed, exactly as [`Event::from_wire`]
/// reads the version before it reads anything else.
///
/// For a reader that needs the version of a message it is not going
/// to consume — the projection's replay floor, which finds the first
/// stream sequence whose version this build reads by probing messages
/// by sequence (<https://github.com/bricef/factor-q/issues/648>).
/// Whether a version is one this build reads is
/// [`SUPPORTED_SCHEMA_VERSIONS`]'s to say.
pub fn declared_schema_version(bytes: &[u8]) -> Option<u32> {
    probe_version(bytes)
        .ok()
        .flatten()
        .map(|(version, _)| version)
}
