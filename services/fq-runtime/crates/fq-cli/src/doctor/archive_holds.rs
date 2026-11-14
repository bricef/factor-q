//! The archive-holds block of `fq doctor`: finished runs whose rows the
//! worker's archive sweeper is holding rather than archiving.
//!
//! Its own module, like the trigger-claims block, because each line has to
//! carry its own way out. A held row lifts only when an operator acts, and
//! what to do differs by cause, so the reason and the remedy the daemon
//! reported are printed under every row rather than summarised once.

use fq_ops::health::{ArchiveHold, ArchiveSweep};

/// Pure: the block. A header with the count, then per row its invocation
/// and agent, the reason, and the remedy. "none" only once the sweeper has
/// finished a pass this start; before that the holds are not known, and
/// the block says so rather than "none", which would be a finding the
/// daemon has not made. The block is always on the page, so its absence
/// never has to be read as "not checked".
pub(super) fn render_archive_holds(sweep: &ArchiveSweep) -> String {
    if sweep.holds.is_empty() {
        return if sweep.has_swept() {
            "Archive holds: none\n".to_string()
        } else {
            format!("Archive holds: {NOT_YET_SWEPT}\n")
        };
    }
    let mut out = format!(
        "Archive holds: {} — finished runs the archive sweeper will not archive until an \
         operator acts; the control plane sees each as not archived\n",
        sweep.holds.len()
    );
    out.push_str(&hold_lines(&sweep.holds));
    if !sweep.has_swept() {
        out.push_str(&format!("  ({NOT_YET_SWEPT}, so there may be more)\n"));
    }
    out
}

/// What both verbs say before the sweeper has finished a pass this start.
pub(crate) const NOT_YET_SWEPT: &str = "not yet known — the archive sweeper has not finished a \
     pass since this daemon started (its first comes one `[worker] archive_retry_interval_ms` \
     after start)";

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
