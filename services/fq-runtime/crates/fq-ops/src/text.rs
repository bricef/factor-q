//! Free text someone else supplied, made safe to show on one line: a
//! drop's reason, the principal who asked. The CLI prints it to a
//! terminal and the dashboard into a page, so both read it through
//! [`one_line`] (issue 1070). JSON carries such text exact.

/// The longest reason a line shows before it is cut.
pub const REASON_CHARS: usize = 200;
/// The longest name of who asked a line shows before it is cut.
pub const REQUESTER_CHARS: usize = 80;

/// Whether `c` must not reach a terminal or a page as it is: a control
/// character (the newline and the escape that starts a terminal sequence
/// among them), a bidirectional control, which reorders what is shown,
/// or an invisible formatting character (zero-width ones, the line and
/// paragraph separators, the soft hyphen, fillers, tag characters),
/// which shows nothing of itself.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'
                | '\u{1160}'
                | '\u{17b4}'
                | '\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// `text` as one line that shows only what it says: every character
/// [`is_hidden`] names is written as its escape (`\n`, `\u{1b}`,
/// `\u{202e}`), and the line is cut, with a visible `…`, once what it
/// prints would pass `max_chars` characters. The bound is on what is
/// printed, escapes included, not on what is read.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for c in text.chars() {
        let piece: String = if is_hidden(c) {
            c.escape_default().collect()
        } else {
            c.to_string()
        };
        let width = piece.chars().count();
        if shown + width > max_chars {
            out.push('…');
            break;
        }
        out.push_str(&piece);
        shown += width;
    }
    out
}

#[cfg(test)]
mod tests;
