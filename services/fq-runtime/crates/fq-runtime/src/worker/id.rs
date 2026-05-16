//! Worker identifier.
//!
//! A worker is the role that hosts agent invocations (see the
//! crate-level `worker` module docs). `WorkerId` is the stable,
//! NATS-subject-safe identifier for a single worker instance.
//!
//! The newtype mirrors [`crate::agent::AgentId`] in shape and
//! discipline: both reuse [`crate::events::subjects::validate_token`]
//! for construction and deserialise validation, both serialise as
//! a bare string, and neither is convertible to the other (the
//! whole point of the distinct newtype is that it's a compile
//! error to pass an `AgentId` where a `WorkerId` is expected).

use serde::{Deserialize, Serialize};

use crate::events::subjects::{SubjectTokenError, validate_token};

/// A validated worker identifier.
///
/// Enforces non-empty and NATS-subject-token safety (no `.`, `*`,
/// `>`, or whitespace) at construction time and at serde
/// deserialisation. Used as the correlation key for
/// `coordination_worker` rows and as the token in
/// `fq.worker.{worker_id}.heartbeat` subjects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerId(String);

impl WorkerId {
    /// Construct a worker id from a string, validating its shape.
    pub fn new(s: impl Into<String>) -> Result<Self, SubjectTokenError> {
        let s = s.into();
        validate_token(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the newtype and return the inner `String`. Used at
    /// boundaries that need owned strings (CLI args, sqlx binds).
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for WorkerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for WorkerId {
    type Err = SubjectTokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl PartialEq<str> for WorkerId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for WorkerId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<WorkerId> for str {
    fn eq(&self, other: &WorkerId) -> bool {
        self == other.0.as_str()
    }
}

impl Serialize for WorkerId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WorkerId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        validate_token(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_id_accepts_typical_shapes() {
        for ok in ["worker-001", "w1", "fq-worker-01HXJABC"] {
            assert!(WorkerId::new(ok).is_ok(), "expected {ok:?} to be valid");
        }
    }

    #[test]
    fn worker_id_rejects_subject_unsafe_input() {
        assert!(matches!(WorkerId::new(""), Err(SubjectTokenError::Empty)));
        for bad in ["foo.bar", "w*", "w>", "has space"] {
            assert!(
                matches!(WorkerId::new(bad), Err(SubjectTokenError::InvalidChar(_))),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn worker_id_serialises_as_bare_string() {
        let id = WorkerId::new("worker-001").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"worker-001\"");
    }

    #[test]
    fn worker_id_round_trips_through_serde() {
        let id = WorkerId::new("worker-001").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: WorkerId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn worker_id_deserialise_rejects_invalid_input() {
        // Wire-boundary contract: a malformed worker_id from the
        // wire fails to parse rather than landing in the runtime
        // as a NATS-subject-unsafe value.
        for raw in ["\"\"", "\"foo.bar\"", "\"w*\"", "\"with space\""] {
            assert!(
                serde_json::from_str::<WorkerId>(raw).is_err(),
                "deserialise should have rejected {raw}"
            );
        }
    }
}
