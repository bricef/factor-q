//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

/// The one renderer has to reproduce, byte for byte, what the two
/// copied loops produced: `uri — name`, a `: description` suffix only
/// when there is one, one row per line including the last.
#[test]
fn render_listing_matches_the_shape_both_arms_produced() {
    let rendered = render_listing(
        [
            ("res://a", "Alpha", Some("the first")),
            ("res://b", "Beta", None),
        ],
        "(nothing)",
    );
    assert_eq!(rendered, "res://a — Alpha: the first\nres://b — Beta\n");
}

/// An empty listing is the caller's sentence, not a blank string — the
/// model gets "there are none", not silence. The two arms word it
/// differently, which is the only thing they are still allowed to
/// disagree about.
#[test]
fn an_empty_listing_renders_the_callers_sentence() {
    let rows: [(&str, &str, Option<&str>); 0] = [];
    assert_eq!(render_listing(rows, "(no resources)"), "(no resources)");
    assert_eq!(
        render_listing(rows, "(no resource templates)"),
        "(no resource templates)"
    );
}
