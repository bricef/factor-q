//! Capturing a child's output, and rendering it for the model.
//!
//! Split from `exec.rs` when that file reached its 800-line cap. The
//! seam is real rather than arithmetic: everything here is about bytes
//! coming off a pipe and the text a model eventually reads, and none of
//! it knows anything about processes, sandboxes or deadlines. `exec.rs`
//! keeps the tool — spawn, enforce, wait, kill — and calls in here for
//! "what did it say".
//!
//! Two bounds live together because they compose: a byte cap that is a
//! safety backstop (dropped bytes are always reported, never silently
//! lost), and a caller-chosen line limit that is the argv-native
//! replacement for `| head` and `| tail`.

use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::watch;

/// How to bound returned output beyond the byte cap.
#[derive(Debug, Clone, Copy)]
pub(super) enum LineLimit {
    /// No line limit — keep the head up to the byte cap.
    None,
    /// Keep only the first N lines (still byte-capped).
    Head(usize),
    /// Keep only the last N lines (still byte-capped).
    Tail(usize),
}

/// Capture a child stream, keeping either the head (default) or the tail
/// (`tail = true`) up to `max_bytes`. Returns the kept bytes, the total
/// number of bytes the stream produced (so the caller can report drops),
/// and whether capture was cut by the drain-grace signal rather than
/// ending at EOF (#176).
pub(super) async fn capture_stream<R>(
    stream: R,
    max_bytes: usize,
    tail: bool,
    stop: watch::Receiver<bool>,
) -> (Vec<u8>, usize, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    if tail {
        read_capped_tail(stream, max_bytes, stop).await
    } else {
        read_capped(stream, max_bytes, stop).await
    }
}

/// Keep at most `max_bytes` from the **front** of a stream, draining and
/// counting the rest (so the child never blocks on a full pipe and the
/// caller learns the true size). Ends at EOF, or when `stop` flips true
/// (the bounded drain, #176). Returns `(kept, total_produced, cut)`.
pub(super) async fn read_capped<R>(
    stream: R,
    max_bytes: usize,
    mut stop: watch::Receiver<bool>,
) -> (Vec<u8>, usize, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut scratch = [0u8; 8 * 1024];
    let mut total = 0usize;
    loop {
        tokio::select! {
            read = reader.read(&mut scratch) => match read {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if buf.len() < max_bytes {
                        let take = (max_bytes - buf.len()).min(n);
                        buf.extend_from_slice(&scratch[..take]);
                    }
                }
                Err(_) => break,
            },
            changed = stop.changed() => match changed {
                Ok(()) if *stop.borrow() => return (buf, total, true),
                Ok(()) => {}
                // The sender only goes away without a cut when the exec
                // future itself was dropped, so there is nobody left to
                // read this stream: stop rather than hold the pipe open
                // for a descendant that outlived the call (#618).
                Err(_) => return (buf, total, true),
            },
        }
    }
    (buf, total, false)
}

/// Keep at most `max_bytes` from the **end** of a stream, reading the whole
/// thing but trimming the retained window so memory stays bounded. Ends at
/// EOF, or when `stop` flips true (the bounded drain, #176). Returns
/// `(kept_tail, total_produced, cut)`.
pub(super) async fn read_capped_tail<R>(
    stream: R,
    max_bytes: usize,
    mut stop: watch::Receiver<bool>,
) -> (Vec<u8>, usize, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut buf: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 8 * 1024];
    let mut total = 0usize;
    let mut cut = false;
    loop {
        tokio::select! {
            read = reader.read(&mut scratch) => match read {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    buf.extend_from_slice(&scratch[..n]);
                    // Amortised trim: only memmove once the window doubles.
                    if buf.len() > 2 * max_bytes {
                        let excess = buf.len() - max_bytes;
                        buf.drain(..excess);
                    }
                }
                Err(_) => break,
            },
            changed = stop.changed() => match changed {
                Ok(()) if *stop.borrow() => {
                    cut = true;
                    break;
                }
                Ok(()) => {}
                // As above (#618): the reader of this result is gone.
                Err(_) => {
                    cut = true;
                    break;
                }
            },
        }
    }
    if buf.len() > max_bytes {
        let excess = buf.len() - max_bytes;
        buf.drain(..excess);
    }
    (buf, total, cut)
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

/// The first `n` lines of `s`, plus whether more lines followed.
pub(super) fn first_lines(s: &str, n: usize) -> (String, bool) {
    let mut lines = s.lines();
    let head: Vec<&str> = lines.by_ref().take(n).collect();
    let more = lines.next().is_some();
    (head.join("\n"), more)
}

/// The last `n` lines of `s`, plus whether earlier lines were dropped.
pub(super) fn last_lines(s: &str, n: usize) -> (String, bool) {
    let all: Vec<&str> = s.lines().collect();
    let dropped = all.len() > n;
    let start = all.len().saturating_sub(n);
    (all[start..].join("\n"), dropped)
}

/// Render one captured stream to display text plus an optional truncation
/// note. `bytes` is what was kept (already byte-capped); `total` is how
/// many bytes the stream actually produced.
pub(super) fn render_stream(
    bytes: &[u8],
    total: usize,
    limit: LineLimit,
) -> (String, Option<String>) {
    let text = String::from_utf8_lossy(bytes);
    let byte_truncated = total > bytes.len();
    match limit {
        LineLimit::None => {
            let note = byte_truncated.then(|| {
                format!(
                    "truncated at the byte cap: kept {} of {} — use max_lines / \
                     tail_lines to choose what you keep",
                    human_bytes(bytes.len() as u64),
                    human_bytes(total as u64),
                )
            });
            (text.into_owned(), note)
        }
        LineLimit::Head(n) => {
            let (shown, more) = first_lines(&text, n);
            let note = (more || byte_truncated)
                .then(|| format!("showing the first {n} line(s); more output followed"));
            (shown, note)
        }
        LineLimit::Tail(n) => {
            let (shown, more) = last_lines(&text, n);
            let note = (more || byte_truncated)
                .then(|| format!("showing the last {n} line(s); earlier output omitted"));
            (shown, note)
        }
    }
}

pub(super) fn format_output(
    stdout: &[u8],
    stdout_total: usize,
    stderr: &[u8],
    stderr_total: usize,
    limit: LineLimit,
) -> String {
    let (out_text, out_note) = render_stream(stdout, stdout_total, limit);
    let (err_text, err_note) = render_stream(stderr, stderr_total, limit);

    let mut out = String::new();
    push_stream(&mut out, "stdout", &out_text, out_note);
    out.push('\n');
    push_stream(&mut out, "stderr", &err_text, err_note);
    out
}

/// Append one `--- <name> ---` section with its optional truncation note.
pub(super) fn push_stream(out: &mut String, name: &str, text: &str, note: Option<String>) {
    out.push_str("--- ");
    out.push_str(name);
    out.push_str(" ---\n");
    if text.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(note) = note {
        out.push('(');
        out.push_str(&note);
        out.push_str(")\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn first_lines_takes_head_and_flags_more() {
        assert_eq!(first_lines("a\nb\nc\nd", 2), ("a\nb".to_string(), true));
        assert_eq!(first_lines("a\nb", 5), ("a\nb".to_string(), false));
    }

    #[test]
    fn last_lines_takes_tail_and_flags_dropped() {
        assert_eq!(last_lines("a\nb\nc\nd", 2), ("c\nd".to_string(), true));
        assert_eq!(last_lines("a\nb", 5), ("a\nb".to_string(), false));
    }

    /// #618: the cut sender is dropped without a send only when the
    /// exec future itself was dropped, so nobody will ever read this
    /// capture. Draining on to EOF there holds the child's pipe open
    /// for a descendant that outlived the call — the capture stops
    /// instead.
    ///
    /// The write half is held open for the whole test, so the stream
    /// never reaches EOF: before the fix this call does not return.
    #[tokio::test]
    async fn capture_stops_when_the_cut_sender_is_dropped() {
        for tail in [false, true] {
            let (_writer, reader) = tokio::io::duplex(64);
            let (stop_tx, stop_rx) = watch::channel(false);
            drop(stop_tx);

            let (kept, total, cut) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                capture_stream(reader, 1024, tail, stop_rx),
            )
            .await
            .expect("capture must stop once nobody can read its result");

            assert!(cut, "the capture was cut, not ended at EOF (tail = {tail})");
            assert!(kept.is_empty() && total == 0);
        }
    }
}
