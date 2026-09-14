//! The health page (#549, #278): the stream/consumer table, the runtime
//! block and the throttled-models row. Its own module because
//! `render.rs` may only shrink and the page grew a row when the
//! provider throttle arrived — the ratchet's answer to a file that
//! grows is to split it, not to raise its budget.

use fq_ops::health::StreamHealth;
use fq_ops::surface::{DoctorReport, OperatorSignalCounts, StatusReport};

use super::consumers::consumer_row;
use super::{esc, inv_link};

/// ": <link>, <link>" suffix for a count that carries ids; empty when
/// there are none.
fn linked_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    format!(
        ": {}",
        ids.iter()
            .map(|id| inv_link(id))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The home page's one-line count: how many notifications landed in the
/// last day, and how many alerts are still open.
///
/// "Open" rather than "unread" is deliberate and the line says so in
/// its hover: an alert closes when a later signal resolves it — the
/// component saying the condition has passed — and not when a person
/// looks at it. There is no acknowledgement in this build, and a number
/// that claimed to be unread would be worse than one that says what it
/// is.
///
/// `None` is **unknown** — the counts call did not answer. It renders
/// amber and says so, because the alternative is the one answer this
/// row must never give: a green `0 open alerts` that an operator reads
/// as "nothing needs me" when what actually happened is that the page
/// could not ask. Amber rather than red: the daemon may be perfectly
/// well and the dashboard's own credential short of a grant, which is
/// a fault in the deployment rather than in the fleet.
fn counts_row(counts: Option<&OperatorSignalCounts>) -> String {
    let Some(counts) = counts else {
        return r#"<tr><th>notifications</th><td class="warn"><a href="/notifications">unknown</a> — the daemon did not answer <code>operator_signal.counts</code>; this is not a count of zero</td></tr>"#
            .to_string();
    };
    let class = if counts.open_alerts > 0 { "bad" } else { "ok" };
    format!(
        r#"<tr><th>notifications</th><td class="{class}"><a href="/notifications">{} in the last 24h</a> · <a href="/notifications?severity=alert" title="alerts no later signal has resolved — alerts are never swept, and this build has no acknowledgement, so this falls when the condition passes and not when you read it">{} open alert{}</a></td></tr>"#,
        counts.notifications,
        counts.open_alerts,
        if counts.open_alerts == 1 { "" } else { "s" },
    )
}

/// The health page body.
///
/// `signals` is the notifications pane's two counts, rendered as one
/// row in the Runtime block. It is a third read rather than a field on
/// either report because the counts belong to their own resource — and
/// it degrades to zeros against a daemon that has no pane, which is
/// what keeps the whole health view rendering across a build skew.
/// `None` is that read having failed for any other reason, which the
/// row reports as unknown rather than as zero — see [`counts_row`].
pub fn health(
    status: &StatusReport,
    doctor: &DoctorReport,
    signals: Option<&OperatorSignalCounts>,
) -> String {
    let mut b = String::new();

    b.push_str(&format!(
        r#"<p>daemon <span class="ok">reachable</span> — version {}</p>"#,
        esc(&status.version)
    ));

    b.push_str("<h2>Streams</h2><table><tr><th>stream</th><th>messages</th><th>consumer</th><th>state</th><th>pending</th><th>in flight</th></tr>");
    for s in &status.streams {
        match s {
            StreamHealth::Unavailable { stream, error } => {
                b.push_str(&format!(
                    r#"<tr><td>{}</td><td colspan="5" class="bad">✗ {}</td></tr>"#,
                    esc(stream),
                    esc(error)
                ));
            }
            StreamHealth::Available {
                stream,
                messages,
                consumers,
                ..
            } => {
                // One row per durable, because a stream carries several
                // and the row that matters during an incident is the
                // one for the consumer that wedged (#549).
                for consumer in consumers {
                    let (cname, cstate, pending, in_flight) = consumer_row(consumer);
                    b.push_str(&format!(
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        esc(stream),
                        messages,
                        cname,
                        cstate,
                        pending,
                        in_flight
                    ));
                }
            }
        }
    }
    b.push_str("</table>");

    b.push_str("<h2>Runtime</h2><table>");
    b.push_str(&format!(
        "<tr><th>projection events</th><td>{}</td></tr>",
        status.projection_rows
    ));
    let exec_class = if doctor.executions.stuck > 0 {
        "bad"
    } else {
        "ok"
    };
    b.push_str(&format!(
        r#"<tr><th>executions</th><td class="{}">{} in-flight ({} working{}, {} stuck{})</td></tr>"#,
        exec_class,
        doctor.executions.in_flight,
        doctor.executions.working,
        linked_ids(&doctor.executions.working_ids),
        doctor.executions.stuck,
        linked_ids(&doctor.executions.stuck_ids)
    ));
    let rec_class = if status.recovery.ambiguous > 0 || status.recovery.stale_workers > 0 {
        "warn"
    } else {
        "ok"
    };
    b.push_str(&format!(
        r#"<tr><th>recovery</th><td class="{}">{} ambiguous, {} stale workers</td></tr>"#,
        rec_class, status.recovery.ambiguous, status.recovery.stale_workers
    ));
    // The provider throttle (#278): what the worker is holding back,
    // and why the fleet is quiet when it is. Amber, not red — a
    // throttled model is the runtime absorbing a provider's
    // backpressure, not a fault.
    if status.throttled_models.is_empty() {
        b.push_str(r#"<tr><th>throttled models</th><td class="ok">none</td></tr>"#);
    } else {
        let lines: Vec<String> = status
            .throttled_models
            .iter()
            .map(throttled_model_cell)
            .collect();
        b.push_str(&format!(
            r#"<tr><th>throttled models</th><td class="warn">{}</td></tr>"#,
            lines.join("<br>")
        ));
    }
    // What has asked for a person's attention, and the way into the
    // pane that shows it. Red only when an alert stands: a
    // notification is normal-hours work, and a home page that shouted
    // about every one of them would train the operator to ignore the
    // row that matters.
    b.push_str(&counts_row(signals));
    b.push_str("</table>");

    if !doctor.failures.is_empty() {
        b.push_str("<h2>Permanent failures</h2><table><tr><th>kind</th><th>count</th></tr>");
        for f in &doctor.failures {
            b.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                esc(&f.error_kind),
                f.count
            ));
        }
        b.push_str("</table>");
    }

    b
}

/// One throttled model as an escaped phrase: the pause's end on the
/// daemon's clock, the permits against the ceiling, the 429s behind it.
fn throttled_model_cell(model: &fq_ops::health::ThrottledModel) -> String {
    let pause = match model.paused_until_ms {
        Some(until) => format!(
            "paused until {}",
            chrono::DateTime::from_timestamp_millis(until)
                .map(|t| t.format("%H:%M:%SZ").to_string())
                .unwrap_or_else(|| format!("{until}ms"))
        ),
        None => "not paused".to_string(),
    };
    format!(
        "{} — {pause}; {}",
        esc(&model.model),
        esc(&model.permit_summary())
    )
}
