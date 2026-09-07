//! `fq projection rebuild`: the client half of
//! `control.projection_rebuild`.
//!
//! One command and one report. The command drops the daemon's
//! projection tables, recreates them and restarts the replay; it
//! answers with an empty receipt, because nothing was appended to any
//! log. What the operator wants to see — when it started, what the
//! replay has to reach, whether it is still going — is the rebuild
//! record `control.status` carries, so the confirmation reads that
//! back, the way `fq dead-letters requeue` reads back the trigger its
//! receipt names.
//!
//! `--yes` is checked here first so a bare `fq projection rebuild`
//! stops with the explanation before it dials; the daemon checks
//! `confirm` again, so a client that skipped this is refused there.

use crate::cli::GlobalArgs;
use crate::edge_call::edge_client_for;
use fq_ops::surface::{ProjectionRebuild, StatusParams, StatusReport};

pub(crate) async fn rebuild_projection(
    global: &GlobalArgs,
    yes: bool,
    reason: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    if !yes {
        anyhow::bail!(
            "refusing without --yes: a rebuild drops the daemon's projection tables and \
             re-derives them from the event stream. Cost-bearing rows, invocation summaries \
             and trigger records are carried across, so no spend is lost; everything else is \
             empty until the replay catches up, and reads answer over a partial fold until \
             then. Run `fq projection rebuild --yes` to proceed."
        );
    }

    let client = edge_client_for(global).await?;
    client
        .invoke(
            fq_ops::OpId::Verb(fq_ops::VerbId::Control(fq_ops::Control::ProjectionRebuild)),
            serde_json::json!({ "confirm": true, "reason": reason }),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // The rebuild has happened by the time the command answers; the
    // record is what says where the replay has to get to.
    let report = client
        .invoke(
            fq_ops::OpId::Report(fq_ops::ReportId::Control(fq_ops::ControlReport::Status)),
            serde_json::to_value(StatusParams {})?,
        )
        .await?
        .map_err(|e| {
            anyhow::anyhow!("rebuilt, but reading the daemon's status back failed: {e}")
        })?;
    let report: StatusReport = serde_json::from_value(report)?;
    let rebuild = report.projection_rebuild;

    if json {
        println!("{}", serde_json::to_string_pretty(&rebuild)?);
        return Ok(());
    }
    print!("{}", render_rebuild_human(rebuild.as_ref()));
    Ok(())
}

/// Pure: the confirmation, so the wording is testable without a
/// daemon.
fn render_rebuild_human(rebuild: Option<&ProjectionRebuild>) -> String {
    let mut out = String::from("Rebuilt the daemon's projection.\n");
    match rebuild {
        Some(rebuild) => {
            out.push_str(&format!("  schema version:   {}\n", rebuild.schema_version));
            out.push_str(&format!("  started:          {}\n", rebuild.started_at));
            out.push_str(&format!("  reason:           {}\n", rebuild.reason));
            match rebuild.target_seq {
                Some(seq) => out.push_str(&format!(
                    "  replay target:    stream sequence {seq} (everything the stream still holds)\n"
                )),
                None => out.push_str("  replay target:    not yet set (the consumer is resetting)\n"),
            }
        }
        None => out.push_str("  (the daemon reported no rebuild record — check `fq status`)\n"),
    }
    out.push_str(
        "Cost-bearing rows, invocation summaries and trigger records were carried across; \
         everything else is re-derived as the replay runs.\n",
    );
    out.push_str("`fq status` reports the replay's progress under `projection rebuild`.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebuild(target_seq: Option<u64>) -> ProjectionRebuild {
        ProjectionRebuild {
            started_at: "2026-09-07T10:00:00+00:00".to_string(),
            reason: "operator request: backfill reasoning tokens".to_string(),
            from_version: None,
            schema_version: 1,
            target_seq,
            consumer_reset_pending: target_seq.is_none(),
            in_progress: true,
        }
    }

    /// The confirmation names the replay's target and where to watch
    /// it, and says what was kept — the fact an operator worried about
    /// dropping tables needs stated.
    #[test]
    fn the_confirmation_names_the_target_and_what_was_kept() {
        let out = render_rebuild_human(Some(&rebuild(Some(4_242))));
        assert!(
            out.starts_with("Rebuilt the daemon's projection.\n"),
            "{out}"
        );
        assert!(out.contains("stream sequence 4242"), "{out}");
        assert!(out.contains("schema version:   1"), "{out}");
        assert!(out.contains("backfill reasoning tokens"), "{out}");
        assert!(out.contains("Cost-bearing rows"), "{out}");
        assert!(out.contains("`fq status`"), "{out}");
    }

    /// A record with no target yet is reported as such rather than as
    /// a sequence of nothing.
    #[test]
    fn a_pending_reset_is_said_to_be_pending() {
        let out = render_rebuild_human(Some(&rebuild(None)));
        assert!(out.contains("not yet set"), "{out}");
        assert!(!out.contains("stream sequence"), "{out}");
    }

    /// No record at all is still a confirmation — the command answered
    /// — with the gap named.
    #[test]
    fn a_missing_record_is_named_not_invented() {
        let out = render_rebuild_human(None);
        assert!(out.contains("no rebuild record"), "{out}");
    }
}
