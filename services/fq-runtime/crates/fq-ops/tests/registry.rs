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

use fq_ops::{Authority, Command, Domain, OpCategory, OpId, Registry, RegistryError, Verb};

// ------------------------------------------------------------------
// Exemplar declarations. Contract only — handlers arrive with the
// edge (plan Phases 2–3); these pin the shape a declaration takes:
// a constructor call whose generic parameters capture the schemas.
// ------------------------------------------------------------------

use fq_ops::fixtures::{
    control, control_down, cost_summary, invocation, invocation_drop, trigger, trigger_publish,
    turn,
};

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
/// at registration. Typed verb ids make this unrepresentable through
/// `Command::new` — the guarantee moved to the type level — so this
/// exercises the registry's residual line of defence against a future
/// enum variant that *renders* to a colliding word, by constructing
/// the collision raw.
#[test]
fn verb_collision_with_the_derived_surface_is_refused() {
    let template = fq_ops::fixtures::invocation_drop();
    let bad = Command {
        verb: fq_ops::VerbId::Unknown {
            domain: "invocation".to_string(),
            verb: "get".to_string(),
        },
        ..template
    };
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
        read(Domain::Cost)
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

/// An atom's List answers with its index, its Stream with its state —
/// and by default those are the same schema, because listing facts
/// hands back facts.
///
/// The default is the half worth pinning: `turn.list` returns full
/// payloads by design, and `Atom::with_index` exists precisely so that
/// an atom which wants a cheaper listing has to say so at its
/// declaration rather than by quietly returning something narrower.
/// Stream is unaffected either way — a stream is
/// creation-notification, and an index row is not the fact that was
/// created.
#[test]
fn an_atom_lists_its_index_and_streams_its_state() {
    let registry = exemplar_registry();
    let list = registry.resolve_named("turn.list").unwrap();
    let stream = registry.resolve_named("turn.stream").unwrap();
    let get = registry.resolve_named("turn.get").unwrap();
    assert_eq!(
        list.output_schema, stream.output_schema,
        "an atom declaring no index lists what it streams"
    );
    assert_eq!(
        list.output_schema, get.output_schema,
        "…and what it Gets: the default index IS the state"
    );

    // Declaring an index moves List and leaves Get and Stream alone.
    let mut indexed = Registry::new();
    indexed
        .register(fq_ops::Atom::with_index::<
            fq_ops::fixtures::EntryKey,
            fq_ops::fixtures::EntryState,
            fq_ops::fixtures::InvocationIndexRow,
            fq_ops::fixtures::EntryFilter,
        >(
            Domain::Turn,
            "an atom whose List is served from an index",
            fq_ops::Stability::Experimental,
        ))
        .unwrap();
    let list = indexed.resolve_named("turn.list").unwrap();
    let stream = indexed.resolve_named("turn.stream").unwrap();
    let get = indexed.resolve_named("turn.get").unwrap();
    assert_ne!(
        list.output_schema, stream.output_schema,
        "a declared index is what List answers with"
    );
    assert_eq!(
        stream.output_schema, get.output_schema,
        "Stream still answers with the atom itself"
    );
    // Categories are unmoved by any of this.
    assert_eq!(list.category, OpCategory::List);
    assert_eq!(stream.category, OpCategory::Stream);
}

/// Watermarks are per-domain: sequences from different domains are
/// not comparable, and read-your-writes watermarks a read of one
/// domain.
#[test]
fn receipt_watermark_is_per_domain() {
    let receipt = fq_ops::Receipt {
        atoms: vec![
            fq_ops::AtomRef {
                domain: Domain::Event,
                key: serde_json::json!({ "event_id": "0192-event-a" }),
            },
            fq_ops::AtomRef {
                domain: Domain::Turn,
                key: serde_json::json!({ "seq": 7 }),
            },
        ],
        watermarks: [(Domain::Event, 43), (Domain::Turn, 7)]
            .into_iter()
            .collect(),
    };
    assert_eq!(receipt.watermark(Domain::Event), Some(43));
    assert_eq!(receipt.watermark(Domain::Turn), Some(7));
    assert_eq!(receipt.watermark(Domain::Worker), None);
    assert_eq!(fq_ops::Receipt::empty().watermark(Domain::Event), None);
}

/// The watermark is recorded, not derived from the atoms.
///
/// It used to be `max(seq)` over the atoms, which only worked while
/// every atom carried a position. Now that atoms are named by
/// identity, a command can still report how far the log got — for a
/// domain whose atoms it names, and for one whose atoms it does not
/// enumerate at all.
#[test]
fn a_watermark_needs_no_atom_to_carry_it() {
    let receipt = fq_ops::Receipt {
        atoms: vec![],
        watermarks: [(Domain::Event, 12)].into_iter().collect(),
    };
    assert_eq!(receipt.watermark(Domain::Event), Some(12));
}

/// `Receipt::one` fills the identity and the watermark together, so
/// the two cannot be filled in inconsistently at a call site — which
/// is the failure the constructor exists to prevent as more commands
/// start minting receipts.
#[test]
fn one_names_the_atom_and_marks_its_domain() {
    let receipt = fq_ops::Receipt::one(
        Domain::Event,
        serde_json::json!({ "event_id": "0192-event-b" }),
        9,
    );
    assert_eq!(receipt.atoms.len(), 1);
    assert_eq!(receipt.atoms[0].domain, Domain::Event);
    assert_eq!(receipt.atoms[0].key["event_id"], "0192-event-b");
    assert_eq!(receipt.watermark(Domain::Event), Some(9));
}

/// A receipt's atom reference is the key its domain's Get takes, so
/// "here is what I appended" walks to "here is the thing itself".
/// Pinned because the whole point of the reshape is that this holds:
/// an identity that Get would not accept is not an identity.
#[test]
fn an_atom_reference_is_shaped_like_its_domains_key() {
    let receipt = fq_ops::Receipt::one(
        Domain::Event,
        serde_json::json!({ "event_id": "0192-event-c" }),
        3,
    );
    let key = &receipt.atoms[0].key;
    assert!(
        key.get("event_id").is_some(),
        "the Event key names `event_id`; got {key}"
    );
    assert!(
        key.get("seq").is_none(),
        "a position is not an identity and must not ride in the reference; got {key}"
    );
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

/// Serialised through [`canonical_json`] rather than
/// `to_string_pretty`, so the snapshot's bytes depend on the data and
/// nothing else.
///
/// Without it this oracle asserts on whichever map type `serde_json`
/// happens to be built with: `Map` is a `BTreeMap` or an `IndexMap`
/// depending on whether anything in the build graph enables
/// `preserve_order`, which is a decision a *dependency* makes. Because
/// Cargo features are additive per build, that made the expected bytes
/// depend on **which packages you compile together** — `cargo test -p
/// fq-ops` and `just runtime-ci` disagreed about this very file (#437,
/// when genai 0.7 turned the feature on). Canonicalising retires the
/// question; both now produce identical output.
#[test]
fn describe_matches_the_committed_snapshot() {
    let registry = exemplar_registry();
    let actual = fq_test_support::canonical_json(
        &serde_json::to_value(registry.describe()).expect("describe serialises"),
    );

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
