//! [`super::one_line`]: what reaches a terminal or a page of someone
//! else's text.

use super::*;

/// **Every bidirectional control is written as an escape**: all twelve,
/// so none can reorder what a line shows.
#[test]
fn every_bidirectional_control_is_escaped() {
    for (c, escaped) in [
        ('\u{061c}', "\\u{61c}"),
        ('\u{200e}', "\\u{200e}"),
        ('\u{200f}', "\\u{200f}"),
        ('\u{202a}', "\\u{202a}"),
        ('\u{202b}', "\\u{202b}"),
        ('\u{202c}', "\\u{202c}"),
        ('\u{202d}', "\\u{202d}"),
        ('\u{202e}', "\\u{202e}"),
        ('\u{2066}', "\\u{2066}"),
        ('\u{2067}', "\\u{2067}"),
        ('\u{2068}', "\\u{2068}"),
        ('\u{2069}', "\\u{2069}"),
    ] {
        assert_eq!(
            one_line(&format!("a{c}b"), 200),
            format!("a{escaped}b"),
            "U+{:04X}",
            c as u32
        );
    }
}

/// **Control characters and invisible ones are written as escapes**:
/// a newline cannot forge a line, an escape sequence cannot reach the
/// terminal, and nothing invisible passes unseen.
#[test]
fn control_and_invisible_characters_are_escaped() {
    for (c, escaped) in [
        ('\n', "\\n"),
        ('\r', "\\r"),
        ('\t', "\\t"),
        ('\u{1b}', "\\u{1b}"),
        ('\u{7f}', "\\u{7f}"),
        ('\u{9b}', "\\u{9b}"),
        ('\u{00ad}', "\\u{ad}"),
        ('\u{200b}', "\\u{200b}"),
        ('\u{200d}', "\\u{200d}"),
        ('\u{2028}', "\\u{2028}"),
        ('\u{2029}', "\\u{2029}"),
        ('\u{2060}', "\\u{2060}"),
        ('\u{feff}', "\\u{feff}"),
        ('\u{3164}', "\\u{3164}"),
        ('\u{e0041}', "\\u{e0041}"),
    ] {
        assert_eq!(
            one_line(&format!("a{c}b"), 200),
            format!("a{escaped}b"),
            "U+{:04X}",
            c as u32
        );
    }
    assert_eq!(
        one_line("why\n  status:   completed\u{1b}[2J", 200),
        "why\\n  status:   completed\\u{1b}[2J"
    );
    assert_eq!(
        one_line("stuck on a tool, é ü ✓", 200),
        "stuck on a tool, é ü ✓"
    );
}

/// **The cut bounds what is printed, escapes included**: 200 characters
/// that each print as an escape print 200 characters and the mark, not
/// some 1,600; a value that fits exactly is not cut.
#[test]
fn the_cut_bounds_what_is_printed() {
    let exactly = "x".repeat(200);
    assert_eq!(
        one_line(&exactly, 200),
        exactly,
        "exactly the bound: no mark"
    );
    let over = one_line(&"x".repeat(201), 200);
    assert_eq!(over.chars().count(), 201);
    assert!(over.ends_with("x…"), "{over}");
    let escapes = one_line(&"\u{1b}".repeat(200), 200);
    assert!(
        escapes.chars().count() <= 201,
        "{} characters printed",
        escapes.chars().count()
    );
    assert!(escapes.ends_with('…'), "{escapes}");
    assert!(!escapes.contains('\u{1b}'), "{escapes}");
    assert_eq!(
        one_line("ab\u{1b}", 4),
        "ab…",
        "an escape that would pass the bound is not split"
    );
    assert_eq!(one_line(&"é".repeat(3), 2), "éé…", "cut by characters");
}
