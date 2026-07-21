//! The `Operation` contract (ADR-0006 D1) and its runtime descriptor.

use schemars::{JsonSchema, Schema, schema_for};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::meta::{OpKind, OpMeta};
use crate::name::OpName;

/// One operator- or system-facing capability, defined exactly once.
/// Every surface — tarpc edge, CLI, MCP, REST — derives from this
/// definition (P1); handlers register daemon-side against it.
///
/// There is no `KIND` to declare: the kind is a property of
/// [`OpName`] itself ([`OpName::kind`]), so a name/kind mismatch is
/// unrepresentable.
pub trait Operation {
    const NAME: OpName;
    /// Schema version, carried beside the name on the wire (P10).
    /// Additive input/output changes keep the version; anything a
    /// caller could observe as a break bumps it. The registry's schema
    /// snapshot tests are the review oracle for the difference.
    const VERSION: u32 = 1;
    type Input: Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const META: OpMeta;
}

/// A registered operation, described: what `registry.describe` serves
/// and what client-wrapper codegen consumes. The schemas are derived
/// from the `Input`/`Output` types — one type language for op schemas,
/// event schemas, and signature schemas (P10). `name` is the rendered
/// human-readable form; `op` is the structured identity the wire
/// carries.
#[derive(Debug, Clone, Serialize)]
pub struct OpDescriptor {
    pub op: OpName,
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
            op: O::NAME,
            name: O::NAME.render(),
            version: O::VERSION,
            kind: O::NAME.kind(),
            meta: O::META,
            input_schema: schema_for!(O::Input),
            output_schema: schema_for!(O::Output),
        }
    }
}
