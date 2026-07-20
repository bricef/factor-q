//! The operation registry: one description per operation, invariants
//! enforced at registration (ADR-0006 D1, P8, P11).

use std::collections::BTreeMap;

use crate::meta::OpKind;
use crate::name::{self, NameError};
use crate::operation::{OpDescriptor, Operation};

/// Why a registration was refused. Every variant is a defect in the
/// registering code, not a runtime condition to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(
        "`{name}` declares kind {declared:?} but its name parses as {expected:?} (P8) — \
         rename the operation or fix its kind"
    )]
    KindMismatch {
        name: String,
        declared: OpKind,
        expected: OpKind,
    },
    #[error("`{name}` is already registered — one registry, one description per operation (D1)")]
    Duplicate { name: String },
}

/// The registry. Ordered by name (a `BTreeMap`) so `describe` output —
/// and everything derived from it, schemas, docs, codegen — is
/// deterministic without a sort at every call site.
#[derive(Debug, Default)]
pub struct Registry {
    ops: BTreeMap<&'static str, OpDescriptor>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one operation, enforcing the registration-time
    /// invariants: the name parses under the P8 grammar, the parsed
    /// kind matches the declared kind, and the name is not taken.
    pub fn register<O: Operation>(&mut self) -> Result<(), RegistryError> {
        let expected = name::expected_kind(O::NAME)?;
        if expected != O::KIND {
            return Err(RegistryError::KindMismatch {
                name: O::NAME.to_string(),
                declared: O::KIND,
                expected,
            });
        }
        if self.ops.contains_key(O::NAME) {
            return Err(RegistryError::Duplicate {
                name: O::NAME.to_string(),
            });
        }
        self.ops.insert(O::NAME, OpDescriptor::of::<O>());
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&OpDescriptor> {
        self.ops.get(name)
    }

    /// Every registered operation, in name order — the payload of the
    /// future `registry.describe` op and the input to client-wrapper
    /// codegen.
    pub fn describe(&self) -> Vec<&OpDescriptor> {
        self.ops.values().collect()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}
