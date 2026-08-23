//! The `fq workers` verbs (plan Phase 4, verbs 21–22): the roster and
//! one worker's fold, both read over the authenticated edge from the
//! daemon's Worker view.
//!
//! `fq workers` is read-only. It used to carry a third verb, `prune`,
//! which deleted stale registration rows straight out of the
//! control-plane store — the one write in the tree that bypassed every
//! boundary, and the only thing reclaiming a table that gains a row per
//! daemon restart. That made an unbounded table an operator's job to
//! remember, and *the system should not depend on operator remediations
//! to work normally*, so the reclamation became a daemon retention
//! sweep ([`fq_runtime::control_plane::retention`]) and the verb was
//! retired rather than transplanted onto the edge.
//!
//! Split out of `lib.rs` (#189) rather than grown in place: the
//! transplant onto `worker.get`/`worker.list` is what pushed that file
//! past its budget, and a subcommand's rendering is exactly the kind
//! of thing that belongs in its own module.

use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;

use crate::cli::GlobalArgs;
use crate::edge_call::edge_invoke;
use fq_ops::surface::{WorkerListFilter, WorkerViewKey};

/// Human-readable heartbeat age. Stays in step with the
/// stale-worker sweep threshold so the operator can eyeball
/// what's about to go stale: anything past the threshold
/// (default 30s) is rendered as `"stale"` regardless of the
/// exact age — agrees with `coordination_worker.status`.
fn format_heartbeat_age_human(age_ms: i64, stale_threshold_ms: i64) -> String {
    if age_ms < 0 {
        return "future".to_string();
    }
    if age_ms >= stale_threshold_ms {
        return "stale".to_string();
    }
    let secs = age_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn format_worker_list_row_human(
    item: &fq_ops::views::WorkerView,
    now_ms: i64,
    stale_threshold_ms: i64,
) -> String {
    let age = format_heartbeat_age_human(now_ms - item.last_heartbeat_ms, stale_threshold_ms);
    format!(
        "{:<28} {:<8} {:<10} {:<8} {}",
        item.worker_id, item.status, age, item.in_flight_count, item.host
    )
}

pub(crate) async fn workers_list(
    global: &GlobalArgs,
    stale_only: bool,
    alive_only: bool,
    json: bool,
) -> anyhow::Result<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    // The threshold the CP uses to flip a worker from alive
    // to stale; this is the same DEFAULT_STALE_THRESHOLD_MS
    // used by the coordination consumer.
    let stale_threshold_ms = DEFAULT_STALE_THRESHOLD_MS;

    // The flip (plan Phase 4, verb 21): the roster comes from the
    // daemon over the authenticated edge, and the selection travels
    // with the request — the two flags are clap-exclusive, so at most
    // one status is ever asked for and the view's index does the
    // narrowing that used to happen after the rows had crossed.
    let status = if stale_only {
        Some("stale")
    } else if alive_only {
        Some("alive")
    } else {
        None
    };
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::Worker),
        serde_json::to_value(WorkerListFilter {
            status: status.map(str::to_string),
        })?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let items: Vec<fq_ops::views::WorkerView> = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("0 workers — nothing to list.");
    } else {
        println!(
            "{:<28} {:<8} {:<10} {:<8} host",
            "worker", "status", "hb-age", "in-flight"
        );
        for item in &items {
            println!(
                "{}",
                format_worker_list_row_human(item, now_ms, stale_threshold_ms)
            );
        }
    }
    Ok(())
}

pub(crate) async fn workers_show(global: &GlobalArgs, id: &str, json: bool) -> anyhow::Result<()> {
    let stale_threshold_ms = DEFAULT_STALE_THRESHOLD_MS;
    // The flip (plan Phase 4, verb 22): the fold — roster row plus
    // owned invocations — is the daemon's to compute; not-found stays
    // the operator-facing exit-1 it always was.
    let output = edge_invoke(
        global,
        fq_ops::OpId::Get(fq_ops::Domain::Worker),
        serde_json::to_value(WorkerViewKey {
            worker_id: id.to_string(),
        })?,
    )
    .await?;
    let detail: fq_ops::views::WorkerDetailView = match output {
        Ok(value) => serde_json::from_value(value)?,
        Err(fq_edge::wire::WireError::NotFound { .. }) => {
            eprintln!("no worker found with id={id}");
            std::process::exit(1);
        }
        Err(e) => anyhow::bail!("{e}"),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let w = &detail.worker;
        println!("Worker: {}", w.worker_id);
        println!("  host:      {}", w.host);
        println!("  status:    {}", w.status);
        println!(
            "  hb-age:    {}",
            format_heartbeat_age_human(now_ms - w.last_heartbeat_ms, stale_threshold_ms)
        );
        println!("  in-flight: {}", w.in_flight_count);
        if !detail.owned.is_empty() {
            println!("\nInvocations owned:");
            for o in detail.owned.iter().take(20) {
                let inv: String = o.invocation_id.chars().take(11).collect();
                println!("  {inv}  {}", o.status);
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_heartbeat_age_human_under_threshold_shows_seconds() {
        assert_eq!(format_heartbeat_age_human(500, 30_000), "0s");
        assert_eq!(format_heartbeat_age_human(12_345, 30_000), "12s");
        assert_eq!(format_heartbeat_age_human(59_999, 30_000), "stale");
    }

    #[test]
    fn format_heartbeat_age_human_minutes_and_hours() {
        // Stale threshold widened so the larger ages don't get
        // clobbered to "stale".
        assert_eq!(format_heartbeat_age_human(150_000, 1_000_000), "2m");
        assert_eq!(format_heartbeat_age_human(3_700_000, 10_000_000), "1h");
    }

    #[test]
    fn format_heartbeat_age_human_past_threshold_is_stale() {
        // 60s with threshold 30s.
        assert_eq!(format_heartbeat_age_human(60_000, 30_000), "stale");
    }

    #[test]
    fn format_heartbeat_age_human_handles_clock_skew() {
        // Negative age = worker's clock is ahead. Render
        // explicitly rather than displaying a nonsense
        // negative second count.
        assert_eq!(format_heartbeat_age_human(-1000, 30_000), "future");
    }
    /// The `--json` worker shape after the swap to `views::WorkerView`
    /// (#105 layer 1). Deliberate change from the old CLI-local item:
    /// gains `registered_at_ms` and `in_flight_count`, drops the
    /// now-dependent `heartbeat_age_ms` (consumers derive age from
    /// `last_heartbeat_ms`; the view stays wall-clock-free).
    #[test]
    fn worker_view_serialises_to_stable_json_shape() {
        let item = fq_ops::views::WorkerView {
            worker_id: "w-1".to_string(),
            host: "host-1".to_string(),
            registered_at_ms: 1_600_000_000_000,
            last_heartbeat_ms: 1_700_000_000_000,
            status: "alive".to_string(),
            in_flight_count: 3,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["worker_id"], "w-1");
        assert_eq!(v["host"], "host-1");
        assert_eq!(v["status"], "alive");
        assert_eq!(v["registered_at_ms"], 1_600_000_000_000_i64);
        assert_eq!(v["last_heartbeat_ms"], 1_700_000_000_000_i64);
        assert_eq!(v["in_flight_count"], 3);
        assert!(v.get("heartbeat_age_ms").is_none());
    }
}
