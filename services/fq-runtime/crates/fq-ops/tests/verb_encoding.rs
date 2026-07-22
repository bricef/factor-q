//! Locks `Verb::segment()` — the string the edge authorizer puts on
//! the authz wire — to `Verb`'s serde snake_case encoding for *every*
//! variant. EnumIter makes this exhaustive, so a future multi-word
//! verb (`GrantAll` → `grant_all`) can never let the two encodings
//! drift and silently fail authorisation closed.

use fq_ops::Verb;
use strum::IntoEnumIterator;

#[test]
fn verb_segment_matches_serde_snake_case() {
    for verb in Verb::iter() {
        let serde = serde_json::to_value(verb).unwrap();
        assert_eq!(
            serde_json::Value::String(verb.segment().to_string()),
            serde,
            "Verb::{verb:?} wire segment must equal its serde snake_case encoding"
        );
    }
}
