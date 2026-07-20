//! The `Operation` contract (ADR-0006 D1) and its runtime descriptor.

use schemars::{JsonSchema, Schema, schema_for};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::meta::{OpKind, OpMeta};

/// One operator- or system-facing capability, defined exactly once.
/// Every surface — tarpc edge, CLI, MCP, REST — derives from this
/// definition (P1); handlers register daemon-side against it.
pub trait Operation {
    /// Must parse under the P8 grammar ([`crate::name::expected_kind`])
    /// to exactly [`Self::KIND`]; the registry refuses anything else.
    const NAME: &'static str;
    /// Schema version, addressed on the wire as `name@version` (P10).
    /// Additive input/output changes keep the version; anything a
    /// caller could observe as a break bumps it. The registry's schema
    /// snapshot tests are the review oracle for the difference.
    const VERSION: u32 = 1;
    const KIND: OpKind;
    type Input: Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const META: OpMeta;
}

/// A registered operation, described: what `registry.describe` serves
/// and what client-wrapper codegen consumes. The schemas are derived
/// from the `Input`/`Output` types — one type language for op schemas,
/// event schemas, and signature schemas (P10).
#[derive(Debug, Clone, Serialize)]
pub struct OpDescriptor {
    pub name: &'static str,
    pub version: u32,
    pub kind: OpKind,
    pub meta: OpMeta,
    pub input_schema: Schema,
    pub output_schema: Schema,
}

impl OpDescriptor {
    pub fn of<O: Operation>() -> Self {
        OpDescriptor {
            name: O::NAME,
            version: O::VERSION,
            kind: O::KIND,
            meta: O::META,
            input_schema: schema_for!(O::Input),
            output_schema: schema_for!(O::Output),
        }
    }

    /// The wire address: `name@version` (P10).
    pub fn wire_name(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}
