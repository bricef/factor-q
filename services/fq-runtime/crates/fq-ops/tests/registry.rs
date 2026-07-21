//! The registry exercised over an exemplar slice of the catalogue —
//! one resource per nature and one declaration per category — plus
//! the schema snapshot oracle.
//!
//! The snapshot (`tests/snapshots/exemplar_registry.json`) is this
//! crate's golden master: the serialized `describe()` output — the
//! declarations themselves. Any change to the value shapes or
//! schemars' output is a visible diff to review against P10's
//! additive-change rules — never silent drift. Regenerate after an
//! intentional change with
//! `UPDATE_SNAPSHOT=1 cargo test -p fq-ops --test registry`.

use fq_ops::{
    Atom, Authority, Command, Domain, OpCategory, OpId, Registry, RegistryError, Report, Stability,
    Synthetic, Verb, View,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------
// Exemplar declarations. Contract only — handlers arrive with the
// edge (plan Phases 2–3); these pin the shape a declaration takes:
// a constructor call whose generic parameters capture the schemas.
// ------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EntryKey {
    seq: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EntryState {
    seq: u64,
    invocation_id: String,
    round: u64,
    role: String,
    content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EntryFilter {
    invocation_id: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// Turn: an atom — Get/List/Stream derive. A **Turn** is one action
/// (assistant output or tool result); a **Round** is the bundle of
/// Turns in one agent-loop iteration, recoverable via the `round`
/// grouping key (the ADR-0027 step boundary is a Round boundary).
fn turn() -> Atom {
    Atom::new::<EntryKey, EntryState, EntryFilter>(
        Domain::Turn,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct InvocationKey {
    invocation_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct InvocationState {
    invocation_id: String,
    agent_id: String,
    phase: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct InvocationFilter {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// Invocation: a view — Get/List derive; no Stream (stream its atoms).
fn invocation() -> View {
    View::new::<InvocationKey, InvocationState, InvocationFilter>(
        Domain::Invocation,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TriggerKey {
    seq: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TriggerState {
    seq: u64,
    agent_id: String,
    payload: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TriggerFilter {
    #[serde(default)]
    agent_id: Option<String>,
}

/// Trigger: an atom. Not operator-creatable via a generic verb —
/// dispatching work is `trigger.publish`, a command.
fn trigger() -> Atom {
    Atom::new::<TriggerKey, TriggerState, TriggerFilter>(
        Domain::Trigger,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ControlState {
    version: String,
    nats_connected: bool,
    stream_ok: bool,
}

/// Control: the synthetic resource — Get alone derives (the machinery
/// describing itself); its verbs register as commands.
fn control() -> Synthetic {
    Synthetic::new::<ControlState>(
        Domain::Control,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct DropInput {
    invocation_id: String,
    reason: Option<String>,
}

/// invocation.drop: a command — the declaration is one constructor
/// call carrying identity, input type, authority, and contract text.
fn invocation_drop() -> Command {
    Command::new::<DropInput>(
        Domain::Invocation,
        "drop",
        Authority {
            verb: Verb::Write,
            scope: Domain::Invocation,
        },
        "Drop an in-flight invocation, archiving it as failed.",
        Stability::Experimental,
    )
    .description("Kill-switch semantics: workers observe the drop at their next step boundary.")
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct DownInput {
    #[serde(default)]
    now: bool,
}

/// control.down: a machinery verb on the synthetic resource — manual
/// authority, same one-site declaration.
fn control_down() -> Command {
    Command::new::<DownInput>(
        Domain::Control,
        "down",
        Authority {
            verb: Verb::Write,
            scope: Domain::Control,
        },
        "Stop the daemon, draining in-flight work to a step boundary.",
        Stability::Experimental,
    )
    .description("Confirmation is the shutdown event, not the ack.")
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct PublishInput {
    agent_id: String,
    payload: serde_json::Value,
}

/// trigger.publish: creation is not a generic verb — dispatching work
/// is a command, and its authority (Write trigger) stays separately
/// grantable from the machinery's lifecycle authority (Write control).
fn trigger_publish() -> Command {
    Command::new::<PublishInput>(
        Domain::Trigger,
        "publish",
        Authority {
            verb: Verb::Write,
            scope: Domain::Trigger,
        },
        "Dispatch a trigger to an agent via the durable trigger stream.",
        Stability::Experimental,
    )
    .description(
        "At-least-once delivery with a bounded budget; the receipt references the \
         appended trigger atom.",
    )
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CostParams {
    #[serde(default)]
    since: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CostOutput {
    total_cost: f64,
    total_llm_calls: u64,
}

/// cost.summary: a report — a named computation, Read on its inputs.
fn cost_summary() -> Report {
    Report::new::<CostParams, CostOutput>(
        "cost.summary",
        &[Domain::Event],
        "Aggregate cost across all agents.",
        Stability::Experimental,
    )
    .description("Cost figures are retained indefinitely; totals never window.")
}

fn exemplar_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(turn()).unwrap();
    registry.register(invocation()).unwrap();
    registry.register(trigger()).unwrap();
    registry.register(control()).unwrap();
    registry.register(invocation_drop()).unwrap();
    registry.register(control_down()).unwrap();
    registry.register(trigger_publish()).unwrap();
    registry.register(cost_summary()).unwrap();
    registry
}

// ------------------------------------------------------------------
// Invariants
// ------------------------------------------------------------------

/// One atom declaration claims three derived names; a view two; a
/// synthetic one; commands and reports one each. Names render
/// structurally, in order.
#[test]
fn derivation_yields_the_expected_surface() {
    let registry = exemplar_registry();
    assert_eq!(
        registry.names(),
        vec![
            "control.down",
            "control.get",
            "cost.summary",
            "invocation.drop",
            "invocation.get",
            "invocation.list",
            "trigger.get",
            "trigger.list",
            "trigger.publish",
            "trigger.stream",
            "turn.get",
            "turn.list",
            "turn.stream",
        ]
    );
}

#[test]
fn duplicate_registration_is_refused() {
    let mut registry = exemplar_registry();
    assert_eq!(
        registry.register(invocation()),
        Err(RegistryError::DuplicateResource {
            domain: Domain::Invocation
        })
    );
    assert_eq!(
        registry.register(invocation_drop()),
        Err(RegistryError::Duplicate {
            name: "invocation.drop".into()
        })
    );
}

/// A declared verb that collides with a derived generic name is caught
/// at registration — the one guarantee the verb strings owe us.
#[test]
fn verb_collision_with_the_derived_surface_is_refused() {
    let bad = Command::new::<DropInput>(
        Domain::Invocation,
        "get",
        Authority {
            verb: Verb::Write,
            scope: Domain::Invocation,
        },
        "shadows a derived name",
        Stability::Experimental,
    );
    let mut registry = exemplar_registry();
    assert_eq!(
        registry.register(bad),
        Err(RegistryError::Duplicate {
            name: "invocation.get".into()
        })
    );
}

/// Authority derives for the generic surface (Read on the domain, and
/// nothing else — the generic surface is read-only); declared ops
/// carry what they declared.
#[test]
fn authority_derivation() {
    let registry = exemplar_registry();
    let read = |scope| {
        vec![Authority {
            verb: Verb::Read,
            scope,
        }]
    };
    assert_eq!(
        registry
            .resolve(&OpId::Stream(Domain::Turn))
            .unwrap()
            .authority,
        read(Domain::Turn)
    );
    assert_eq!(
        registry
            .resolve(&OpId::Get(Domain::Control))
            .unwrap()
            .authority,
        read(Domain::Control)
    );
    assert_eq!(
        registry.resolve(&control_down().op()).unwrap().authority,
        vec![control_down().authority]
    );
    assert_eq!(
        registry.resolve(&cost_summary().op()).unwrap().authority,
        read(Domain::Event)
    );
}

/// Natures live on the declarations; the derived surface follows
/// them: views and synthetics get no stream, synthetics no list, and
/// categories say which envelope an op rides.
#[test]
fn natures_and_categories() {
    let registry = exemplar_registry();
    assert!(
        registry
            .resolve(&OpId::Stream(Domain::Invocation))
            .is_none()
    );
    assert!(registry.resolve(&OpId::List(Domain::Control)).is_none());
    assert!(registry.resolve(&OpId::Stream(Domain::Control)).is_none());
    assert_eq!(
        registry
            .resolve(&OpId::List(Domain::Invocation))
            .unwrap()
            .category,
        OpCategory::List
    );
    assert_eq!(
        registry.resolve(&invocation_drop().op()).unwrap().category,
        OpCategory::DomainVerb
    );
    assert_eq!(
        registry.resolve_named("trigger.publish").unwrap().category,
        OpCategory::DomainVerb
    );
    assert_eq!(
        registry.resolve_named("turn.stream").unwrap().category,
        OpCategory::Stream
    );
    // A machinery singleton has no key: its Get takes no input.
    assert!(
        registry
            .resolve(&OpId::Get(Domain::Control))
            .unwrap()
            .input_schema
            .is_none()
    );
    assert!(registry.resolve_named("invocation.frobnicate").is_none());
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

/// The wire form of an op identity is serde's native encoding, not
/// the rendered string — pin one of each shape so an attribute change
/// (which would break client/daemon compatibility) is a visible diff.
#[test]
fn wire_encoding_is_native_not_rendered() {
    let op = OpId::Stream(Domain::Turn);
    let encoded = serde_json::to_string(&op).unwrap();
    assert_eq!(encoded, r#"{"stream":"turn"}"#);
    assert_eq!(serde_json::from_str::<OpId>(&encoded).unwrap(), op);
    assert_eq!(op.to_string(), "turn.stream");

    let verb = control_down().op();
    assert_eq!(
        serde_json::to_string(&verb).unwrap(),
        r#"{"verb":{"domain":"control","verb":"down"}}"#
    );
    assert_eq!(
        serde_json::from_str::<OpId>(r#"{"verb":{"domain":"control","verb":"down"}}"#).unwrap(),
        verb
    );
    assert_eq!(verb.to_string(), "control.down");
}

// ------------------------------------------------------------------
// The schema snapshot oracle
// ------------------------------------------------------------------

#[test]
fn describe_matches_the_committed_snapshot() {
    let registry = exemplar_registry();
    let actual = serde_json::to_string_pretty(registry.describe()).unwrap() + "\n";

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
         review the diff against P10's additive-change rules (does any declaration \
         need a version bump?), then UPDATE_SNAPSHOT=1 and commit."
    );
}
