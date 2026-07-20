//! Registry invariants and the schema snapshot oracle.
//!
//! The snapshot (`tests/snapshots/exemplar_registry.json`) is this
//! crate's golden master: the serialized `describe()` output for three
//! exemplar ops, one per kind shape. Any change to the descriptor
//! shape, the metadata contract, or schemars' derived output is a
//! visible diff to review against P10's additive-change rules — never
//! silent drift. Regenerate after an intentional change with
//! `UPDATE_SNAPSHOT=1 cargo test -p fq-ops --test registry`.

use fq_ops::{
    OpDescriptor, OpKind, OpMeta, OpPermission, Operation, Registry, RegistryError, Stability, Verb,
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
    const NAME: &'static str = "invocation.show";
    const KIND: OpKind = OpKind::Query;
    type Input = ShowInput;
    type Output = ShowOutput;
    const META: OpMeta = OpMeta {
        permission: OpPermission {
            verb: Verb::Read,
            scope: "invocation",
        },
        read_audit: false,
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
    const NAME: &'static str = "invocation.drop";
    const KIND: OpKind = OpKind::Command;
    type Input = DropInput;
    type Output = fq_ops::Receipt;
    const META: OpMeta = OpMeta {
        permission: OpPermission {
            verb: Verb::Write,
            scope: "invocation",
        },
        read_audit: false,
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
    const NAME: &'static str = "invocation.transcript.tail";
    const KIND: OpKind = OpKind::Stream;
    type Input = TranscriptTailInput;
    type Output = TranscriptEntry;
    const META: OpMeta = OpMeta {
        permission: OpPermission {
            verb: Verb::Read,
            scope: "invocation",
        },
        read_audit: false,
        stability: Stability::Experimental,
        caveats: "",
    };
}

fn exemplar_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register::<InvocationShow>().unwrap();
    registry.register::<InvocationDrop>().unwrap();
    registry.register::<TranscriptTail>().unwrap();
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
            name: "invocation.show".into()
        })
    );
}

/// An op whose declared kind contradicts its parsed name is refused —
/// misclassification is a reviewable defect (P8), caught at
/// registration, not in review archaeology.
#[test]
fn kind_mismatch_is_refused() {
    struct DropMislabelled;
    impl Operation for DropMislabelled {
        const NAME: &'static str = "invocation.drop";
        const KIND: OpKind = OpKind::Query;
        type Input = DropInput;
        type Output = fq_ops::Receipt;
        const META: OpMeta = InvocationDrop::META;
    }
    let mut registry = Registry::new();
    assert_eq!(
        registry.register::<DropMislabelled>(),
        Err(RegistryError::KindMismatch {
            name: "invocation.drop".into(),
            declared: OpKind::Query,
            expected: OpKind::Command,
        })
    );
}

#[test]
fn unparseable_names_are_refused() {
    struct Frobnicate;
    impl Operation for Frobnicate {
        const NAME: &'static str = "invocation.frobnicate";
        const KIND: OpKind = OpKind::Command;
        type Input = DropInput;
        type Output = fq_ops::Receipt;
        const META: OpMeta = InvocationDrop::META;
    }
    let mut registry = Registry::new();
    assert!(matches!(
        registry.register::<Frobnicate>(),
        Err(RegistryError::Name(_))
    ));
}

#[test]
fn wire_names_carry_the_version() {
    let registry = exemplar_registry();
    assert_eq!(
        registry.get("invocation.show").unwrap().wire_name(),
        "invocation.show@1"
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
