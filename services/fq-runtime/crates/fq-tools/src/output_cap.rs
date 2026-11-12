//! Bounding the text a tool result carries, and saying what was cut.
//!
//! A tool's output is written to the worker's WAL, published as a
//! `tool.result` event, and appended to the conversation that is sent
//! to the model again on every later turn. The runtime bounds it once,
//! where every tool's answer comes back, with [`cap_output`] (`[tools]
//! max_result_bytes`, <https://github.com/bricef/factor-q/issues/827>).
//!
//! **The cap is measured in the bytes a result takes inside an event**,
//! not in bytes of text, because what it protects is a publish the
//! broker refuses by serialised size. JSON writes most text as it is,
//! but a control character below U+0020 takes six bytes (`\u0000`) and
//! a quote, a backslash or one of `\b \t \n \f \r` takes two, so 256 KiB
//! of NUL bytes — most of an uncompressed tar — is 1.5 MiB in the event.
//! Ordinary text is cut at the number; text dense in control characters,
//! quotes or backslashes is cut shorter.
//!
//! `exec` keeps its own per-stream cap (`builtin/exec/output.rs`), and
//! the two are deliberately not one function. `exec`'s bounds *memory
//! while a pipe is read*: it works on raw bytes that need not be UTF-8,
//! can keep a head or a tail, and its note points at `exec`'s own
//! `max_lines` / `tail_lines` parameters. This one cuts a finished string
//! to a budget, and no tool has a parameter it could point the model at.
//! What they share is how a size is said, [`human_bytes`], and the shape
//! of the note: the cap, how much was kept, how much there was. With the
//! default cap, ordinary `exec` output is cut once, by `exec`; output
//! with many invalid bytes or control characters renders larger than
//! `exec`'s raw-byte cap and can be cut again here, losing `exec`'s note
//! (see #985).

/// The most bytes one byte of text can take inside a JSON string: a
/// control character written as `\u00XX`.
const MAX_JSON_WIDTH: usize = 6;

/// Cut `output` so that it takes at most `max_bytes` bytes inside a JSON
/// string as `serde_json` writes it ([`json_encoded_len`]), **the marker
/// included**.
///
/// Output that fits is returned untouched. Longer output keeps its
/// longest prefix that ends on a character boundary and still leaves
/// room for the marker, then the marker, which names the cap and the
/// exact number of bytes of text kept and produced. The marker says the
/// rest was dropped and nothing more: no tool can return the rest yet,
/// so it does not suggest asking for it.
///
/// The cap counts the marker, and counts encoded bytes, because the
/// number is meant to bound what gets recorded and published, not what
/// the tool produced. A result therefore never takes more than
/// `max_bytes` in an event. Room is kept for the marker's widest form,
/// so a capped result can fall a few bytes short of the cap. In the
/// degenerate case of a cap too small to hold the marker, the result is
/// as much of the marker as fits.
///
/// The output is scanned once, and only as far as the cap: a 50 MiB
/// answer costs a scan of the first quarter-megabyte or so, and nothing
/// at all when six bytes per byte of text would still fit.
pub fn cap_output(output: String, max_bytes: usize) -> String {
    let total = output.len();
    if total.saturating_mul(MAX_JSON_WIDTH) <= max_bytes {
        return output;
    }
    // The marker's widest form. Its two counts are each at most `total`,
    // and a smaller number never has more digits, so the marker written
    // for the kept prefix never takes more room than this.
    let reserve = json_encoded_len(&truncation_marker(total, total, max_bytes));
    let budget = max_bytes.saturating_sub(reserve);
    let Some(kept) = cut_point(&output, budget, max_bytes) else {
        return output;
    };
    let mut capped = output;
    capped.truncate(kept);
    capped.push_str(&truncation_marker(kept, total, max_bytes));
    if reserve > max_bytes
        && let Some(fits) = cut_point(&capped, max_bytes, max_bytes)
    {
        capped.truncate(fits);
    }
    // `truncate` keeps the allocation, and a parallel batch holds every
    // result until its turn ends: without this, each capped read of a
    // large file would go on holding the whole file.
    capped.shrink_to_fit();
    capped
}

/// The bytes `text` takes inside a JSON string as `serde_json` writes
/// it, the surrounding quotes excluded: six for a control character
/// below U+0020 other than the five with a short escape, two for those
/// five and for `"` and `\`, one for every other byte (non-ASCII text is
/// written as it is).
pub fn json_encoded_len(text: &str) -> usize {
    text.bytes().map(json_width).sum()
}

/// The bytes one byte of UTF-8 takes inside a JSON string.
fn json_width(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | 0x08 | b'\t' | b'\n' | 0x0C | b'\r' => 2,
        0x00..=0x1F => MAX_JSON_WIDTH,
        _ => 1,
    }
}

/// One pass over `text`, stopping as soon as its encoded length passes
/// `limit`. `None` when the whole of `text` fits in `limit`; otherwise
/// the length of the longest prefix that ends on a character boundary
/// and whose encoded length is at most `budget` (which is at most
/// `limit`, so that prefix is settled by the time the scan stops).
fn cut_point(text: &str, budget: usize, limit: usize) -> Option<usize> {
    let mut encoded = 0usize;
    let mut within_budget = 0;
    for (index, &byte) in text.as_bytes().iter().enumerate() {
        encoded += json_width(byte);
        if encoded <= budget {
            within_budget = index + 1;
        }
        if encoded > limit {
            return Some(text.floor_char_boundary(within_budget));
        }
    }
    None
}

/// The note appended to a capped result. Its counts are bytes of text,
/// which is what the model can reason about. ASCII, so a cut anywhere
/// inside it is still a character boundary.
fn truncation_marker(kept: usize, total: usize, max_bytes: usize) -> String {
    format!(
        "\n\n(truncated at the {} tool-result cap: kept the first {kept} of {total} bytes; \
         the rest was dropped)",
        human_bytes(max_bytes as u64)
    )
}

/// Human-readable byte size, e.g. `3.4 MiB`, `100.0 KiB`, `512 B`.
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{}.{} MiB", n / MIB, (n % MIB) * 10 / MIB)
    } else if n >= KIB {
        format!("{}.{} KiB", n / KIB, (n % KIB) * 10 / KIB)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests;
