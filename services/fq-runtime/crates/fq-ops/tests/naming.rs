//! The naming guarantees, proven exhaustively — the op space is a
//! finite set of enum values, so nothing here is a property test.
//!
//! Rendered names are self-documentation (MCP tool names, docs,
//! `registry.describe`), not transport — tarpc carries `OpName`
//! natively. The strings owe us exactly two things: no collisions,
//! and stability (a rename is a deliberate, visible diff, because
//! adapters and operator muscle memory index on them). The committed
//! snapshot (`tests/snapshots/op_names.txt`, `name kind` per line)
//! pins both the rendering and the kind mapping; regenerate after an
//! intentional change with
//! `UPDATE_SNAPSHOT=1 cargo test -p fq-ops --test naming`.

use std::collections::BTreeMap;

use fq_ops::name::{DomainTag, OpName};
use strum::IntoEnumIterator;

#[test]
fn rendered_names_never_collide() {
    let mut seen: BTreeMap<&'static str, OpName> = BTreeMap::new();
    for op in OpName::all() {
        if let Some(previous) = seen.insert(op.render(), op) {
            panic!(
                "`{}` renders both {previous:?} and {op:?} — rendered names must be unique",
                op.render()
            );
        }
    }
}

/// `OpName::all` chains each domain's iterator by hand; this proves no
/// domain was forgotten (the one completeness property the compiler's
/// exhaustiveness checks cannot see).
#[test]
fn all_covers_every_domain() {
    let covered: std::collections::HashSet<DomainTag> =
        OpName::all().map(|op| DomainTag::from(&op)).collect();
    for tag in DomainTag::iter() {
        assert!(
            covered.contains(&tag),
            "domain {tag:?} is missing from OpName::all() — chain its iterator"
        );
    }
}

#[test]
fn names_and_kinds_match_the_committed_snapshot() {
    let mut lines: Vec<String> = OpName::all()
        .map(|op| format!("{} {:?}", op.render(), op.kind()))
        .collect();
    lines.sort();
    let actual = lines.join("\n") + "\n";

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/op_names.txt");
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing snapshot {path:?} — run `UPDATE_SNAPSHOT=1 cargo test -p fq-ops \
             --test naming` and commit the result"
        )
    });
    assert_eq!(
        actual, expected,
        "the op roster drifted from the committed snapshot. A rename or kind change \
         is a contract change for every string-addressed adapter — if intentional, \
         UPDATE_SNAPSHOT=1 and review the diff."
    );
}

/// The wire form is serde's native enum encoding, not the rendered
/// string — pin one example of each so a serde attribute change is a
/// visible diff (it would break client/daemon compatibility).
#[test]
fn wire_encoding_is_native_not_rendered() {
    use fq_ops::name::InvocationOp;
    let op = OpName::Invocation(InvocationOp::TranscriptTail);
    let encoded = serde_json::to_string(&op).unwrap();
    assert_eq!(encoded, r#"{"invocation":"transcript_tail"}"#);
    let decoded: OpName = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, op);
    assert_eq!(op.render(), "invocation.transcript.tail");
    assert_eq!(op.to_string(), op.render());
}
