//! Bounding the text a tool result carries, and saying what was cut.
//!
//! A tool's output is written to the worker's WAL, published as a
//! `tool.result` event, and appended to the conversation that is sent
//! to the model again on every later turn. The runtime bounds it once,
//! where every tool's answer comes back, with [`cap_output`] (`[tools]
//! max_result_bytes`, <https://github.com/bricef/factor-q/issues/827>).
//!
//! `exec` keeps its own, tighter per-stream cap (`builtin/exec/output.rs`),
//! and the two are deliberately not one function. `exec`'s bounds
//! *memory while a pipe is read*: it works on raw bytes that need not be
//! UTF-8, can keep a head or a tail, and its note points at `exec`'s own
//! `max_lines` / `tail_lines` parameters. This one cuts a finished
//! string to a byte budget, and no tool has a parameter it could point
//! the model at. What they share is how a size is said, [`human_bytes`],
//! and the shape of the note: the cap, how much was kept, how much there
//! was.

/// Cut `output` to at most `max_bytes` bytes, **the marker included**.
///
/// Output of `max_bytes` or fewer is returned untouched. Longer output
/// keeps its longest prefix that ends on a character boundary and still
/// leaves room for the marker, then the marker, which names the cap and
/// the exact number of bytes kept and produced. The marker says the rest
/// was dropped and nothing more: no tool can return the rest yet, so it
/// does not suggest asking for it.
///
/// The cap counts the marker because the number is meant to bound what
/// gets recorded and published, not what the tool produced. A result
/// therefore never exceeds `max_bytes`. In the degenerate case of a cap
/// too small to hold the marker, the result is the marker cut to the
/// cap.
pub fn cap_output(output: String, max_bytes: usize) -> String {
    let total = output.len();
    if total <= max_bytes {
        return output;
    }
    // The marker's longest form. Its two counts are each at most
    // `total`, and a smaller number never has more digits, so the
    // marker written for the kept prefix is never longer than this.
    let reserve = truncation_marker(total, total, max_bytes).len();
    let kept = output.floor_char_boundary(max_bytes.saturating_sub(reserve));
    let mut capped = output;
    capped.truncate(kept);
    capped.push_str(&truncation_marker(kept, total, max_bytes));
    capped.truncate(capped.floor_char_boundary(max_bytes));
    capped
}

/// The note appended to a capped result. ASCII only, so a cut anywhere
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
