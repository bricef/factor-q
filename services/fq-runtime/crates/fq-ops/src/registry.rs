//! The operation registry: one description per operation (ADR-0006
//! D1). With names as types ([`OpName`]), the only invariant left to
//! enforce at registration is that no name is claimed twice — every
//! other misuse the old string grammar policed is now unrepresentable.

use std::collections::BTreeMap;

use crate::name::OpName;
use crate::operation::{OpDescriptor, Operation};

/// Why a registration was refused. A defect in the registering code,
/// not a runtime condition to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("`{name}` is already registered — one registry, one description per operation (D1)")]
    Duplicate { name: &'static str },
}

/// The registry. Keyed by rendered name (a `BTreeMap`) so `describe`
/// output — and everything derived from it: docs, codegen, MCP tool
/// listings — is deterministically name-ordered, and so
/// string-addressed adapters can look ops up without parsing.
#[derive(Debug, Default)]
pub struct Registry {
    ops: BTreeMap<&'static str, OpDescriptor>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one operation. Rendered names are collision-free
    /// across distinct [`OpName`] values (proven exhaustively in
    /// `naming.rs`), so a duplicate key here means the same operation
    /// was registered twice.
    pub fn register<O: Operation>(&mut self) -> Result<(), RegistryError> {
        let name = O::NAME.render();
        if self.ops.contains_key(name) {
            return Err(RegistryError::Duplicate { name });
        }
        self.ops.insert(name, OpDescriptor::of::<O>());
        Ok(())
    }

    pub fn get(&self, op: OpName) -> Option<&OpDescriptor> {
        self.ops.get(op.render())
    }

    /// Lookup by rendered name — for string-addressed adapters (MCP
    /// tool names, docs routes). The registry is the index; nothing
    /// parses.
    pub fn get_named(&self, name: &str) -> Option<&OpDescriptor> {
        self.ops.get(name)
    }

    /// Every registered operation, in rendered-name order — the
    /// payload of the future `registry.describe` op and the input to
    /// client-wrapper codegen.
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
