//! The registry exercised over an exemplar slice of the catalogue —
//! one resource per nature and one declared op per category — plus
//! the schema snapshot oracle.
//!
//! The snapshot (`tests/snapshots/exemplar_registry.json`) is this
//! crate's golden master: the serialized `describe()` output. Any
//! change to the descriptor shape, derived authority, or schemars'
//! output is a visible diff to review against P10's additive-change
//! rules — never silent drift. Regenerate after an intentional change
//! with `UPDATE_SNAPSHOT=1 cargo test -p fq-ops --test registry`.

use fq_ops::{
    Authority, Command, Domain, Nature, OpCategory, OpId, OpMeta, Registry, RegistryError, Report,
    Resource, Stability, Verb,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------
// Exemplar catalogue slice. Contract only — handlers arrive with the
// edge (plan Phases 2–3); these pin the shape a definition takes.
// ------------------------------------------------------------------

/// Turn: an atom — Get/List/Stream derive. Tabletop vocabulary
/// (settled in review): a **Turn** is one action — an assistant
/// output or a tool result — and a **Round** is the bundle of Turns
/// in one agent-loop iteration (the ADR-0027 step boundary is a Round
/// boundary). Rounds are not a resource: they are recoverable from
/// the turn stream via the `round` grouping key, and become a view
/// over Turns if round-level reads ever earn a catalogue row.
struct TurnR;

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

impl Resource for TurnR {
    const DOMAIN: Domain = Domain::Turn;
    const NATURE: Nature = Nature::Atom;
    type Key = EntryKey;
    type State = EntryState;
    type Filter = EntryFilter;
    const META: OpMeta = EXEMPLAR_META;
}

/// Invocation: a view — Get/List derive; no Stream (stream its atoms).
struct InvocationR;

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

impl Resource for InvocationR {
    const DOMAIN: Domain = Domain::Invocation;
    const NATURE: Nature = Nature::View;
    type Key = InvocationKey;
    type State = InvocationState;
    type Filter = InvocationFilter;
    const META: OpMeta = EXEMPLAR_META;
}

/// Trigger: an atom that operators may create.
struct TriggerR;

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

impl Resource for TriggerR {
    const DOMAIN: Domain = Domain::Trigger;
    const NATURE: Nature = Nature::Atom;
    type Key = TriggerKey;
    type State = TriggerState;
    type Filter = TriggerFilter;
    const META: OpMeta = EXEMPLAR_META;
}

/// trigger.publish: creation is not a generic verb — dispatching work
/// is a command with semantics (delivery budget, at-least-once), and
/// its authority (Write trigger) stays separately grantable from the
/// machinery's lifecycle authority (Write control).
struct TriggerPublish;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct PublishInput {
    agent_id: String,
    payload: serde_json::Value,
}

impl Command for TriggerPublish {
    const DOMAIN: Domain = Domain::Trigger;
    const VERB: &'static str = "publish";
    type Input = PublishInput;
    const AUTHORITY: Authority = Authority {
        verb: Verb::Write,
        scope: Domain::Trigger,
    };
    const META: OpMeta = OpMeta {
        description: "Dispatch a trigger to an agent via the durable trigger stream.",
        stability: Stability::Experimental,
        caveats: "at-least-once delivery with a bounded budget; the receipt references the appended trigger atom",
    };
}

/// Control: the synthetic resource — Get alone derives (the machinery
/// describing itself); its verbs register as commands.
struct ControlR;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ControlState {
    version: String,
    nats_connected: bool,
    stream_ok: bool,
}

impl Resource for ControlR {
    const DOMAIN: Domain = Domain::Control;
    const NATURE: Nature = Nature::Synthetic;
    type Key = ();
    type State = ControlState;
    type Filter = ();
    const META: OpMeta = EXEMPLAR_META;
}

/// invocation.drop: a domain verb — declared at one site: identity
/// (resource + verb), input, authority, and contract text all here.
struct InvocationDrop;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct DropInput {
    invocation_id: String,
    reason: Option<String>,
}

impl Command for InvocationDrop {
    const DOMAIN: Domain = Domain::Invocation;
    const VERB: &'static str = "drop";
    type Input = DropInput;
    const AUTHORITY: Authority = Authority {
        verb: Verb::Write,
        scope: Domain::Invocation,
    };
    const META: OpMeta = OpMeta {
        description: "Drop an in-flight invocation, archiving it as failed.",
        stability: Stability::Experimental,
        caveats: "kill-switch semantics: workers observe the drop at their next step boundary",
    };
}

/// control.down: a machinery verb on the synthetic resource — same
/// one-site declaration, manual authority.
struct ControlDown;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct DownInput {
    #[serde(default)]
    now: bool,
}

impl Command for ControlDown {
    const DOMAIN: Domain = Domain::Control;
    const VERB: &'static str = "down";
    type Input = DownInput;
    const AUTHORITY: Authority = Authority {
        verb: Verb::Write,
        scope: Domain::Control,
    };
    const META: OpMeta = OpMeta {
        description: "Stop the daemon, draining in-flight work to a step boundary.",
        stability: Stability::Experimental,
        caveats: "confirmation is the shutdown event, not the ack",
    };
}

/// cost.summary: a report — a named computation, Read on its inputs.
struct CostSummary;

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

impl Report for CostSummary {
    const NAME: &'static str = "cost.summary";
    type Params = CostParams;
    type Output = CostOutput;
    const READS: &'static [Domain] = &[Domain::Event];
    const META: OpMeta = OpMeta {
        description: "Aggregate cost across all agents.",
        stability: Stability::Experimental,
        caveats: "cost figures are retained indefinitely; totals never window",
    };
}

/// Shared exemplar contract text — a real catalogue entry writes its
/// own.
const EXEMPLAR_META: OpMeta = OpMeta {
    description: "exemplar resource",
    stability: Stability::Experimental,
    caveats: "",
};

fn exemplar_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register_resource::<TurnR>().unwrap();
    registry.register_resource::<InvocationR>().unwrap();
    registry.register_resource::<TriggerR>().unwrap();
    registry.register_command::<TriggerPublish>().unwrap();
    registry.register_resource::<ControlR>().unwrap();
    registry.register_command::<InvocationDrop>().unwrap();
    registry.register_command::<ControlDown>().unwrap();
    registry.register_report::<CostSummary>().unwrap();
    registry
}

// ------------------------------------------------------------------
// Invariants
// ------------------------------------------------------------------

/// One atom row buys three derived ops; a view two; a synthetic one;
/// the declared surface registers one each. Names render structurally
/// and describe() is name-ordered.
#[test]
fn derivation_yields_the_expected_surface() {
    let registry = exemplar_registry();
    let names: Vec<&str> = registry
        .describe()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(
        names,
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
    // The resource-level entry is checked first: re-registering a
    // domain is a catalogue duplicate, before any op name collides.
    assert_eq!(
        registry.register_resource::<InvocationR>(),
        Err(RegistryError::DuplicateResource {
            domain: Domain::Invocation
        })
    );
    assert_eq!(
        registry.register_command::<InvocationDrop>(),
        Err(RegistryError::Duplicate {
            name: "invocation.drop".into()
        })
    );
}

/// A declared verb that collides with a derived generic name is caught
/// at registration — the one guarantee the verb strings owe us.
#[test]
fn verb_collision_with_the_derived_surface_is_refused() {
    struct BadLeaf;
    impl Command for BadLeaf {
        const DOMAIN: Domain = Domain::Invocation;
        const VERB: &'static str = "get";
        type Input = DropInput;
        const AUTHORITY: Authority = InvocationDrop::AUTHORITY;
        const META: OpMeta = InvocationDrop::META;
    }
    let mut registry = exemplar_registry();
    assert_eq!(
        registry.register_command::<BadLeaf>(),
        Err(RegistryError::Duplicate {
            name: "invocation.get".into()
        })
    );
}

/// Authority derives for the generic surface; declared ops carry what
/// they declared.
#[test]
fn authority_derivation() {
    let registry = exemplar_registry();
    let read = |scope| Authority {
        verb: Verb::Read,
        scope,
    };
    assert_eq!(
        registry.get(&OpId::Stream(Domain::Turn)).unwrap().authority,
        vec![read(Domain::Turn)]
    );
    assert_eq!(
        registry.get(&TriggerPublish::op()).unwrap().authority,
        vec![Authority {
            verb: Verb::Write,
            scope: Domain::Trigger
        }]
    );
    assert_eq!(
        registry.get(&OpId::Get(Domain::Control)).unwrap().authority,
        vec![read(Domain::Control)]
    );
    assert_eq!(
        registry.get(&ControlDown::op()).unwrap().authority,
        vec![ControlDown::AUTHORITY]
    );
}

/// Natures live on the catalogue's resource entries, not on ops:
/// views and synthetics get no stream, synthetics get no list, and
/// categories say which envelope an op rides.
#[test]
fn natures_and_categories() {
    let registry = exemplar_registry();
    assert!(registry.get(&OpId::Stream(Domain::Invocation)).is_none());
    assert!(registry.get(&OpId::List(Domain::Control)).is_none());
    assert!(registry.get(&OpId::Stream(Domain::Control)).is_none());
    assert_eq!(
        registry.get_resource(Domain::Invocation).unwrap().nature,
        Nature::View
    );
    assert_eq!(
        registry.get_resource(Domain::Control).unwrap().nature,
        Nature::Synthetic
    );
    assert_eq!(
        registry.get_resource(Domain::Turn).unwrap().nature,
        Nature::Atom
    );
    assert!(registry.get_resource(Domain::Event).is_none());
    assert_eq!(
        registry.get(&InvocationDrop::op()).unwrap().category,
        OpCategory::DomainVerb
    );
    assert_eq!(
        registry.get_named("trigger.publish").unwrap().category,
        OpCategory::DomainVerb
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

    let verb = ControlDown::op();
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
    let payload = serde_json::json!({
        "resources": registry.describe_resources(),
        "ops": registry.describe(),
    });
    let actual = serde_json::to_string_pretty(&payload).unwrap() + "\n";

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
         review the diff against P10's additive-change rules (does any resource or \
         declared op need a version bump?), then UPDATE_SNAPSHOT=1 and commit."
    );
}
