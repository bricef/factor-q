//! The health page (#549, #278): the stream/consumer table, the runtime
//! block and the throttled-models row. Its own module because
//! `render.rs` may only shrink and the page grew a row when the
//! provider throttle arrived — the ratchet's answer to a file that
//! grows is to split it, not to raise its budget.

use fq_ops::health::StreamHealth;
use fq_ops::surface::{DoctorReport, StatusReport};

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

/// The health page body.
pub fn health(status: &StatusReport, doctor: &DoctorReport) -> String {
    let mut b = String::new();

    b.push_str(&format!(
        r#"<p>daemon <span class="ok">reachable</span> — version {}</p>"#,
        esc(&status.version)
    ));

    b.push_str("<h2>Streams</h2><table><tr><th>stream</th><th>messages</th><th>consumer</th><th>state</th><th>lag</th><th>pending</th></tr>");
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
                    let (cname, cstate, lag, pending) = consumer_row(consumer);
                    b.push_str(&format!(
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        esc(stream),
                        messages,
                        cname,
                        cstate,
                        lag,
                        pending
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
