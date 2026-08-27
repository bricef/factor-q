//! `fq doctor`: one aggregated durable-execution health report — worker
//! liveness, in-flight/stuck work, ambiguous invocations, permanent
//! failures and dead letters.
//!
//! The client half of `control.doctor` (plan Phase 4, verb 15): one
//! edge call, then rendering. The report itself — its shape, its
//! checks, and the reads behind them — is `fq_daemon::doctor_report`,
//! in the other binary.
//!
//! **This verb used to work with the daemon stopped, and no longer
//! does.** It read the projection directly, so it answered from
//! whatever the last daemon left behind. That is the trade every
//! migrated read has made (`fq events query` made it first), and it
//! lands harder here than anywhere else, because a diagnostic that
//! needs the thing it diagnoses is a diagnostic with a blind spot
//! exactly where an operator reaches for it. Two things make it the
//! right trade anyway: what the offline read returned was a *stale*
//! verdict presented as a current one — worker liveness and stuck-work
//! ages computed against a fold nobody was advancing — and the case it
//! could not cover was never the one it appeared to. So the answer
//! when no daemon answers is not a connection error but the finding
//! itself: nothing is running for these checks to be about
//! ([`doctor_client`]).

use crate::cli::GlobalArgs;
use fq_ops::surface::DoctorReport;

use crate::edge_call::edge_client_for;
use fq_ops::surface::DOCTOR_STUCK_THRESHOLD_MS;

/// Pure: render the human-readable `fq doctor` report, mirroring
/// `render_recovery_guidance` — an overall verdict, then per-failing-
/// check the count plus the copy-paste next-step command. Returns
/// `All clear.` when every check is green (the dead-letter line is
/// always shown as pending #49 — it is informational, not a problem).
fn render_doctor_report_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str("factor-q doctor\n\n");

    // Verdict line.
    if report.has_issues() {
        out.push_str("Verdict: issues found — see below.\n\n");
    } else {
        out.push_str("Verdict: All clear.\n\n");
    }

    // Workers.
    out.push_str(&format!(
        "Workers: {} alive, {} stale, {} shutdown\n",
        report.workers.alive, report.workers.stale, report.workers.shutdown
    ));
    if report.workers.stale > 0 {
        out.push_str("  -> `fq workers list --stale-only` to inspect\n");
    }

    // Executions.
    out.push_str(&format!(
        "Current executions: {} in-flight ({} working, {} stuck)\n",
        report.executions.in_flight, report.executions.working, report.executions.stuck
    ));
    if report.executions.stuck > 0 {
        out.push_str(&format!(
            "  -> {} not advanced in >{}s: {}\n",
            report.executions.stuck,
            DOCTOR_STUCK_THRESHOLD_MS / 1000,
            report.executions.stuck_ids.join(", ")
        ));
        out.push_str(
            "  -> `fq invocation show <id>` to inspect, `fq invocation drop <id>` to triage\n",
        );
    }

    // Ambiguous.
    out.push_str(&format!("Ambiguous invocations: {}\n", report.ambiguous));
    if report.ambiguous > 0 {
        out.push_str("  -> `fq invocation list --status=ambiguous` to inspect\n");
        out.push_str("  -> `fq invocation drop <id>` to triage individually\n");
    }

    // Permanent failures.
    let failure_total = report.failure_total();
    out.push_str(&format!("Permanent failures: {failure_total}\n"));
    if failure_total > 0 {
        for f in &report.failures {
            out.push_str(&format!("  {}: {}\n", f.error_kind, f.count));
        }
        out.push_str("  -> `fq invocation list --status=failed` to inspect\n");
    }

    // Dead-letters (#49): exhausted triggers the dispatcher consumed.
    if report.dead_letters.exhausted_triggers > 0 {
        out.push_str(&format!(
            "Dead-letters: {} exhausted trigger(s)\n",
            report.dead_letters.exhausted_triggers
        ));
        out.push_str(
            "  -> `fq dead-letters list` to inspect; `fq dead-letters requeue <agent>` to re-run\n",
        );
    } else {
        out.push_str("Dead-letters: none\n");
    }

    out
}

/// Dial the daemon for a report that is *about* the daemon.
///
/// A failure to connect here is not an incidental transport problem
/// that happens to be in the way of the answer — for this one verb it
/// may **be** the answer, and the most consequential one the checks
/// below could have returned. Saying so is the difference between an
/// operator reading `Connection refused` as a bug in `fq doctor` and
/// reading it as the first thing to fix.
///
/// It is stated conditionally on purpose. Two different failures reach
/// here — an unreachable edge and a client that was never paired — and
/// only the first is evidence about the runtime. Asserting "nothing is
/// running" when the truth is "you have not run `fq connect`" would
/// send an operator to restart a healthy daemon, which is the same
/// class of misdirection this message exists to prevent.
async fn doctor_client(global: &GlobalArgs) -> anyhow::Result<fq_edge::EdgeClient> {
    edge_client_for(global).await.map_err(|e| {
        anyhow::anyhow!(
            "{e:#}\n\
             `fq doctor` reports on a running daemon, so it cannot answer without \
             reaching one — the line above says what stopped it. If the daemon is simply \
             not running, that is the finding rather than a missing report: there is no \
             runtime for these checks to be about."
        )
    })
}

/// `fq doctor`: ask the daemon for the durable-execution health
/// composite and render it.
pub(crate) async fn doctor(
    global: &GlobalArgs,
    json: bool,
    fail_on_issues: bool,
) -> anyhow::Result<()> {
    let client = doctor_client(global).await?;
    let output = client
        .invoke(
            fq_ops::OpId::Report(fq_ops::ReportId::Control(fq_ops::ControlReport::Doctor)),
            serde_json::json!({}),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let report: DoctorReport = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_doctor_report_human(&report));
    }

    if fail_on_issues && report.has_issues() {
        // Opt-in non-zero exit for `&&` health-gates and cron. The
        // anyhow error path already maps to ExitCode::FAILURE in main.
        anyhow::bail!("doctor found issues (see report above)");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
