//! The archive-holds block of `fq doctor`: finished runs whose rows the
//! worker's archive sweeper is holding rather than archiving.
//!
//! Its own module, like the trigger-claims block, because each line has to
//! carry its own way out. A held row lifts only when an operator acts, and
//! what to do differs by cause, so the reason and the remedy the daemon
//! reported are printed under every row rather than summarised once.

use fq_ops::health::ArchiveHold;

/// Pure: the block. A header with the count, then per row its invocation
/// and agent, the reason, and the remedy. "none" when nothing is held, so
/// the block is always on the page and its absence never has to be read as
/// "not checked".
pub(super) fn render_archive_holds(holds: &[ArchiveHold]) -> String {
    if holds.is_empty() {
        return "Archive holds: none\n".to_string();
    }
    let mut out = format!(
        "Archive holds: {} — finished runs the archive sweeper will not archive until an \
         operator acts; the control plane sees each as not archived\n",
        holds.len()
    );
    out.push_str(&hold_lines(holds));
    out
}

/// Each held row as two lines: what it is and why, then the remedy. Shared
/// with `fq status`, which lists the same rows without a verdict.
pub(crate) fn hold_lines(holds: &[ArchiveHold]) -> String {
    holds
        .iter()
        .map(|hold| {
            format!(
                "  {} ({}): {}\n  -> {}\n",
                hold.invocation_id,
                hold.agent_id,
                hold.reason.summary(),
                hold.remedy
            )
        })
        .collect()
}
