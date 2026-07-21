//! Registry invariants and the schema snapshot oracle.
//!
//! With names as types, most of what the old string grammar policed is
//! unrepresentable — what's left to test is the one value-level
//! invariant (no double registration), lookup and ordering, and the
//! snapshot (`tests/snapshots/exemplar_registry.json`): the serialized
//! `describe()` output for three exemplar ops, one per kind shape.
//! Any change to the descriptor shape, the metadata contract, or
//! schemars' derived output is a visible diff to review against P10's
//! additive-change rules — never silent drift. Regenerate after an
//! intentional change with
//! `UPDATE_SNAPSHOT=1 cargo test -p fq-ops --test registry`.

use fq_ops::name::{InvocationOp, OpName};
use fq_ops::{
    OpDescriptor, OpMeta, OpPermission, Operation, Registry, RegistryError, Stability, Verb,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------
// Exemplar operations — the ADR-0006 Phase-1 trio, contract only.
// The real handlers arrive with the edge (plan Phases 2–3); these
// pin the *shape* a definition takes.
// ------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ShowInput {
    invocation_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ShowOutput {
    invocation_id: String,
    agent_id: String,
    phase: String,
}

struct InvocationShow;

impl Operation for InvocationShow {
    const NAME: OpName = OpName::Invocation(InvocationOp::Show);
    type Input = ShowInput;
    type Output = ShowOutput;
    const META: OpMeta = OpMeta {
        permission: OpPermission {
            verb: Verb::Read,
            scope: "invocation",
        },
        stability: Stability::Experimental,
        caveats: "",
    };
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct DropInput {
    invocation_id: String,
    reason: Option<String>,
}

struct InvocationDrop;

impl Operation for InvocationDrop {
    const NAME: OpName = OpName::Invocation(InvocationOp::Drop);
    type Input = DropInput;
    type Output = fq_ops::Receipt;
    const META: OpMeta = OpMeta {
        permission: OpPermission {
            verb: Verb::Write,
            scope: "invocation",
        },
        stability: Stability::Experimental,
        caveats: "kill-switch semantics: the invocation is archived as failed; \
                  workers observe the drop at their next step boundary",
    };
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TranscriptTailInput {
    invocation_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TranscriptEntry {
    role: String,
    content: String,
}

struct TranscriptTail;

impl Operation for TranscriptTail {
    const NAME: OpName = OpName::Invocation(InvocationOp::TranscriptTail);
    type Input = TranscriptTailInput;
    type Output = TranscriptEntry;
    const META: OpMeta = OpMeta {
        permission: OpPermission {
            verb: Verb::Read,
            scope: "invocation",
        },
        stability: Stability::Experimental,
        caveats: "",
    };
}

fn exemplar_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register::<TranscriptTail>().unwrap();
    registry.register::<InvocationShow>().unwrap();
    registry.register::<InvocationDrop>().unwrap();
    registry
}

// ------------------------------------------------------------------
// Invariants
// ------------------------------------------------------------------

#[test]
fn describe_is_name_ordered_regardless_of_registration_order() {
    let registry = exemplar_registry();
    let names: Vec<&str> = registry.describe().iter().map(|d| d.name).collect();
    assert_eq!(
        names,
        vec![
            "invocation.drop",
            "invocation.show",
            "invocation.transcript.tail"
        ]
    );
}

#[test]
fn duplicate_registration_is_refused() {
    let mut registry = exemplar_registry();
    assert_eq!(
        registry.register::<InvocationShow>(),
        Err(RegistryError::Duplicate {
            name: "invocation.show"
        })
    );
}

#[test]
fn lookup_works_by_op_and_by_rendered_name() {
    let registry = exemplar_registry();
    let by_op = registry
        .get(OpName::Invocation(InvocationOp::Show))
        .expect("lookup by OpName");
    let by_name = registry
        .get_named("invocation.show")
        .expect("lookup by rendered name");
    assert_eq!(by_op.name, by_name.name);
    assert_eq!(by_op.version, 1);
    assert!(registry.get_named("invocation.frobnicate").is_none());
}

/// The kind in a descriptor comes from the name itself — there is no
/// second declaration to disagree with.
#[test]
fn descriptor_kind_is_derived_from_the_name() {
    let registry = exemplar_registry();
    assert_eq!(
        registry
            .get(OpName::Invocation(InvocationOp::Drop))
            .unwrap()
            .kind,
        fq_ops::OpKind::Command
    );
    assert_eq!(
        registry
            .get(OpName::Invocation(InvocationOp::TranscriptTail))
            .unwrap()
            .kind,
        fq_ops::OpKind::Stream
    );
}

#[test]
fn receipt_watermark_is_the_highest_appended_seq() {
    let receipt = fq_ops::Receipt {
        events: vec![
            fq_ops::EventRef {
                subject: "fq.agent.researcher.failed".into(),
                stream: "fq-events".into(),
                seq: 41,
            },
            fq_ops::EventRef {
                subject: "fq.agent.researcher.archived".into(),
                stream: "fq-events".into(),
                seq: 43,
            },
        ],
    };
    assert_eq!(receipt.watermark(), Some(43));
    assert_eq!(fq_ops::Receipt { events: vec![] }.watermark(), None);
}

// ------------------------------------------------------------------
// The schema snapshot oracle
// ------------------------------------------------------------------

#[test]
fn describe_matches_the_committed_snapshot() {
    let registry = exemplar_registry();
    let descriptors: Vec<&OpDescriptor> = registry.describe();
    let actual = serde_json::to_string_pretty(&descriptors).unwrap() + "\n";

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/exemplar_registry.json");
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing snapshot {path:?} — run `UPDATE_SNAPSHOT=1 cargo test -p fq-ops \
             --test registry` and commit the result"
        )
    });
    assert_eq!(
        actual, expected,
        "registry describe() drifted from the committed snapshot. If intentional, \
         review the diff against P10's additive-change rules (does any op need a \
         version bump?), then UPDATE_SNAPSHOT=1 and commit."
    );
}
