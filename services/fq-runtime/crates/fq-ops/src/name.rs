//! Operation names as types: each domain declares exactly its own
//! operations, so a nonsensical pairing (`cost.drop`) is a compile
//! error, not a registration error. Kind is not parsed from anything —
//! [`OpName::spec`] is the single, exhaustive declaration point tying
//! every operation to its human-readable name and its CQRS kind, and
//! the compiler's exhaustiveness check means a new variant cannot be
//! added without deciding both.
//!
//! The rendered name is **self-documentation, not transport**: tarpc
//! carries `OpName` natively, and string-addressed adapters (MCP tool
//! names, docs, `registry.describe`) index rendered names from the
//! registry rather than parsing them. The one guarantee the strings
//! owe us is collision-freedom, enforced exhaustively by test — the
//! name space is finite, so it isn't even a property test.
//!
//! Extending a domain's operations (or adding a domain) is the P11
//! curation gate, now expressed as an enum variant: deliberate,
//! reviewable, and impossible to do halfway.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::{EnumDiscriminants, EnumIter, IntoEnumIterator};

use crate::meta::OpKind;

macro_rules! domain_op {
    ($(#[$doc:meta])* $name:ident { $($variant:ident),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash,
            Serialize, Deserialize, JsonSchema, EnumIter,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}

domain_op!(AgentOp { List, Show });
domain_op!(ControlOp { Down, Reload });
domain_op!(CostOp { ByAgent, Summary });
domain_op!(DeadletterOp { List, Requeue });
domain_op!(EventOp { Query, Tail });
domain_op!(InvocationOp {
    Drop,
    List,
    Show,
    Transcript,
    TranscriptTail
});
domain_op!(RegistryOp { Describe });
domain_op!(RuntimeOp {
    Doctor,
    Health,
    Status,
    Version
});
domain_op!(TraversalOp { Run, Status, Tail });
domain_op!(TriggerOp { Publish });
domain_op!(WorkerOp { List, Prune, Show });

/// Every operation the runtime can expose, one domain enum per
/// variant. The derived [`DomainTag`] discriminant enum exists so the
/// completeness of [`OpName::all`] is testable: every tag must appear
/// in the iteration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, EnumDiscriminants,
)]
#[strum_discriminants(name(DomainTag), derive(EnumIter, Hash))]
#[serde(rename_all = "snake_case")]
pub enum OpName {
    Agent(AgentOp),
    Control(ControlOp),
    Cost(CostOp),
    Deadletter(DeadletterOp),
    Event(EventOp),
    Invocation(InvocationOp),
    Registry(RegistryOp),
    Runtime(RuntimeOp),
    Traversal(TraversalOp),
    Trigger(TriggerOp),
    Worker(WorkerOp),
}

impl OpName {
    /// The single declaration point: rendered name + kind, per
    /// operation. Exhaustive — adding a variant without extending
    /// this match is a compile error, and the name-collision test
    /// pins the rendered column.
    pub const fn spec(&self) -> (&'static str, OpKind) {
        use OpKind::{Command, Probe, Query, Stream};
        match self {
            OpName::Agent(AgentOp::List) => ("agent.list", Query),
            OpName::Agent(AgentOp::Show) => ("agent.show", Query),
            OpName::Control(ControlOp::Down) => ("control.down", Command),
            OpName::Control(ControlOp::Reload) => ("control.reload", Command),
            OpName::Cost(CostOp::ByAgent) => ("cost.by_agent", Query),
            OpName::Cost(CostOp::Summary) => ("cost.summary", Query),
            OpName::Deadletter(DeadletterOp::List) => ("deadletter.list", Query),
            OpName::Deadletter(DeadletterOp::Requeue) => ("deadletter.requeue", Command),
            OpName::Event(EventOp::Query) => ("event.query", Query),
            OpName::Event(EventOp::Tail) => ("event.tail", Stream),
            OpName::Invocation(InvocationOp::Drop) => ("invocation.drop", Command),
            OpName::Invocation(InvocationOp::List) => ("invocation.list", Query),
            OpName::Invocation(InvocationOp::Show) => ("invocation.show", Query),
            OpName::Invocation(InvocationOp::Transcript) => ("invocation.transcript", Query),
            OpName::Invocation(InvocationOp::TranscriptTail) => {
                ("invocation.transcript.tail", Stream)
            }
            OpName::Registry(RegistryOp::Describe) => ("registry.describe", Query),
            OpName::Runtime(RuntimeOp::Doctor) => ("runtime.doctor", Query),
            OpName::Runtime(RuntimeOp::Health) => ("runtime.health", Probe),
            OpName::Runtime(RuntimeOp::Status) => ("runtime.status", Probe),
            OpName::Runtime(RuntimeOp::Version) => ("runtime.version", Query),
            OpName::Traversal(TraversalOp::Run) => ("traversal.run", Command),
            OpName::Traversal(TraversalOp::Status) => ("traversal.status", Query),
            OpName::Traversal(TraversalOp::Tail) => ("traversal.tail", Stream),
            OpName::Trigger(TriggerOp::Publish) => ("trigger.publish", Command),
            OpName::Worker(WorkerOp::List) => ("worker.list", Query),
            OpName::Worker(WorkerOp::Prune) => ("worker.prune", Command),
            OpName::Worker(WorkerOp::Show) => ("worker.show", Query),
        }
    }

    /// The human-readable name (self-documentation, MCP tool names,
    /// `registry.describe` — never load-bearing for the tarpc wire).
    pub const fn render(&self) -> &'static str {
        self.spec().0
    }

    /// The CQRS kind — derived from the declaration, never declared
    /// separately, so a kind/name mismatch cannot exist.
    pub const fn kind(&self) -> OpKind {
        self.spec().1
    }

    /// Every operation, in declaration order. New domains must be
    /// chained here; `naming.rs` proves completeness against
    /// [`DomainTag`].
    pub fn all() -> impl Iterator<Item = OpName> {
        AgentOp::iter()
            .map(OpName::Agent)
            .chain(ControlOp::iter().map(OpName::Control))
            .chain(CostOp::iter().map(OpName::Cost))
            .chain(DeadletterOp::iter().map(OpName::Deadletter))
            .chain(EventOp::iter().map(OpName::Event))
            .chain(InvocationOp::iter().map(OpName::Invocation))
            .chain(RegistryOp::iter().map(OpName::Registry))
            .chain(RuntimeOp::iter().map(OpName::Runtime))
            .chain(TraversalOp::iter().map(OpName::Traversal))
            .chain(TriggerOp::iter().map(OpName::Trigger))
            .chain(WorkerOp::iter().map(OpName::Worker))
    }
}

impl std::fmt::Display for OpName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.render())
    }
}
