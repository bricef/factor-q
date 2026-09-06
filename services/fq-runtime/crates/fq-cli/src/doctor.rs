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

    // Executions. The threshold comes off the report rather than out of
    // a constant here: the daemon derives it from its own call
    // deadlines (#37), so the number a client quoted from its own build
    // could be the wrong one for the daemon it is talking to.
    out.push_str(&format!(
        "Current executions: {} in-flight ({} working, {} stuck after {}s)\n",
        report.executions.in_flight,
        report.executions.working,
        report.executions.stuck,
        report.executions.stuck_after_ms / 1000,
    ));
    if report.executions.stuck > 0 {
        out.push_str(&format!(
            "  -> {} not advanced in >{}s: {}\n",
            report.executions.stuck,
            report.executions.stuck_after_ms / 1000,
            report.executions.stuck_ids.join(", ")
        ));
        out.push_str(
            "  -> `fq invocation show <id>` to inspect, `fq invocation drop <id>` to triage\n",
        );
        out.push_str(
            "  -> `fq events query --event-type invocation_stuck` for when each one was flagged\n",
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

    // Consumers (#549): every durable this daemon expects, named. The
    // Phase 1 exit criterion is "`fq doctor` reports every consumer",
    // so the healthy ones are listed too — an operator reading this
    // during an incident needs to know which consumers were *checked*,
    // not only which ones complained.
    out.push_str(&render_consumers(&report.consumers));

    // MCP servers (#548): every shared server the daemon declares.
    // Boot no longer stops for one that will not start, so this line is
    // where "the daemon is up but half its tools are not" becomes
    // visible — and the agents that need an unavailable server are
    // being refused at dispatch with the same reason.
    out.push_str(&render_mcp_servers(&report.mcp_servers));

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

/// Pure: the consumer block of `fq doctor`. One line per durable, its
/// name first, and a next-step line under any that is not doing its
/// job. A stuck consumer is the wedge finding B4 describes: unlimited
/// redelivery means the event is not lost, the escalating NAK delay
/// means the daemon is not spinning, and this line is the part that
/// makes it something an operator can find.
fn render_consumers(consumers: &[fq_ops::health::ConsumerHealth]) -> String {
    use fq_ops::health::ConsumerHealth;

    if consumers.is_empty() {
        return "Consumers: none expected\n".to_string();
    }
    let faulty = consumers.iter().filter(|c| c.is_fault()).count();
    let mut out = format!(
        "Consumers: {} checked, {faulty} unhealthy\n",
        consumers.len()
    );
    for consumer in consumers {
        match consumer {
            ConsumerHealth::Active {
                name,
                lag,
                redeliveries,
                stuck: true,
                ..
            } => {
                out.push_str(&format!(
                    "  {name}: ✗ stuck — {redeliveries} redeliveries past its acked floor, \
                     lag {lag}\n"
                ));
                out.push_str(&format!(
                    "  -> its handler keeps failing; check the daemon log for `consumer={name}` \
                     and free whatever it is blocked on (disk, store, broker)\n"
                ));
            }
            ConsumerHealth::Active { name, lag, .. } => {
                out.push_str(&format!("  {name}: ok (lag {lag})\n"));
            }
            ConsumerHealth::Missing { name } => {
                out.push_str(&format!("  {name}: ✗ missing\n"));
                out.push_str(
                    "  -> the durable does not exist; the task that creates it never started\n",
                );
            }
            ConsumerHealth::Error { name, error } => {
                out.push_str(&format!("  {name}: ✗ unreadable: {error}\n"));
            }
        }
    }
    out
}

/// Pure: the MCP block of `fq doctor`. Silent when no agent declares a
/// shared server — a daemon that runs none has nothing to report, and a
/// line saying so on every install would be noise. Otherwise one line
/// per server, named, with the reason and the next retry under any that
/// is down.
fn render_mcp_servers(servers: &[fq_ops::health::McpServerHealth]) -> String {
    use fq_ops::health::McpServerHealth;

    if servers.is_empty() {
        return String::new();
    }
    let down = servers.iter().filter(|s| s.is_fault()).count();
    let mut out = format!("MCP servers: {} declared, {down} unavailable\n", servers.len());
    for server in servers {
        match server {
            McpServerHealth::Ready { name, tools } => {
                out.push_str(&format!("  {name}: ok ({tools} tools)\n"));
            }
            McpServerHealth::Starting { name } => {
                out.push_str(&format!("  {name}: starting\n"));
            }
            McpServerHealth::Unavailable {
                name,
                reason,
                attempts,
                next_retry_at_ms,
            } => {
                out.push_str(&format!(
                    "  {name}: ✗ unavailable after {attempts} attempt(s) — {reason}\n"
                ));
                out.push_str(&match next_retry_at_ms {
                    Some(at) => format!(
                        "  -> agents declaring it are refused at dispatch; next retry {}\n",
                        render_when(*at)
                    ),
                    None => "  -> agents declaring it are refused at dispatch; retrying is \
                             disabled ([mcp] retry_initial_secs = 0), so this needs a restart \
                             once the server is back\n"
                        .to_string(),
                },);
            }
        }
    }
    out
}

/// An epoch-millisecond instant as a relative phrase. Relative because
/// the reader is deciding whether to wait: "in 4m" answers that and a
/// timestamp in the daemon's timezone does not.
fn render_when(at_ms: i64) -> String {
    let delta = (at_ms - chrono::Utc::now().timestamp_millis()) / 1000;
    match delta {
        ..=0 => "due now".to_string(),
        1..=90 => format!("in {delta}s"),
        _ => format!("in {}m", delta / 60),
    }
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
