use crate::*;

// ============================================================
// fq workers subcommand
// ============================================================

/// Human-readable heartbeat age. Stays in step with the
/// stale-worker sweep threshold so the operator can eyeball
/// what's about to go stale: anything past the threshold
/// (default 30s) is rendered as `"stale"` regardless of the
/// exact age — agrees with `coordination_worker.status`.
pub(crate) fn format_heartbeat_age_human(age_ms: i64, stale_threshold_ms: i64) -> String {
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

pub(crate) fn format_worker_list_row_human(
    item: &fq_runtime::views::WorkerView,
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
    let stale_threshold_ms = 30_000_i64;

    let views = open_views(global).await?;
    let items: Vec<_> = views
        .workers()
        .await?
        .into_iter()
        .filter(|w| !(stale_only && w.status != "stale"))
        .filter(|w| !(alive_only && w.status != "alive"))
        .collect();

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

pub(crate) async fn workers_prune(global: &GlobalArgs, dry_run: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    // Prune deletes control-plane rows, so this path runs the
    // legacy split if one is pending.
    let db_paths = ensure_split_dbs(&config).await?;
    // allow-direct-store-open: operator write path (`fq workers prune` deletes rows).
    let store = ControlPlaneStore::open(&db_paths.control_plane).await?;
    let stale: Vec<String> = store
        .list_workers()
        .await?
        .into_iter()
        .filter(|worker| worker.status == WorkerStatus::Stale)
        .map(|worker| worker.worker_id)
        .collect();
    if dry_run {
        println!(
            "Would remove {} stale worker(s): {}",
            stale.len(),
            stale.join(", ")
        );
    } else if stale.is_empty() {
        println!("0 stale workers removed.");
    } else {
        let removed = store.prune_stale_workers().await?;
        println!(
            "Removed {} stale worker(s): {}",
            removed.len(),
            removed.join(", ")
        );
    }
    Ok(())
}

pub(crate) async fn workers_show(global: &GlobalArgs, id: &str, json: bool) -> anyhow::Result<()> {
    let stale_threshold_ms = 30_000_i64;
    let views = open_views(global).await?;
    let Some(detail) = views.worker(id).await? else {
        eprintln!("no worker found with id={id}");
        std::process::exit(1);
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
mod workers_tests {
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

    #[test]
    fn render_recovery_guidance_all_clear() {
        let out = render_recovery_guidance(0, 0);
        assert!(out.contains("All clear"), "got: {out:?}");
        // No command hints when nothing's pending.
        assert!(
            !out.contains("fq invocation"),
            "should not hint commands: {out:?}"
        );
        assert!(
            !out.contains("fq workers"),
            "should not hint commands: {out:?}"
        );
    }

    #[test]
    fn render_recovery_guidance_for_ambiguous_only() {
        let out = render_recovery_guidance(3, 0);
        assert!(out.contains("Ambiguous invocations: 3"));
        assert!(out.contains("fq invocation list --status=ambiguous"));
        assert!(out.contains("fq invocation drop"));
        assert!(!out.contains("Stale workers"), "got: {out:?}");
        assert!(!out.contains("All clear"));
    }

    #[test]
    fn render_recovery_guidance_for_stale_only() {
        let out = render_recovery_guidance(0, 2);
        assert!(out.contains("Stale workers: 2"));
        assert!(out.contains("fq workers list --stale-only"));
        assert!(out.contains("fq workers prune"));
        assert!(!out.contains("Ambiguous"), "got: {out:?}");
        assert!(!out.contains("All clear"));
    }

    #[test]
    fn render_recovery_guidance_for_both() {
        let out = render_recovery_guidance(1, 1);
        assert!(out.contains("Ambiguous invocations: 1"));
        assert!(out.contains("Stale workers: 1"));
        assert!(out.contains("fq invocation drop"));
        assert!(out.contains("fq workers list --stale-only"));
        assert!(out.contains("fq workers prune"));
    }

    /// The `--json` worker shape after the swap to `views::WorkerView`
    /// (#105 layer 1). Deliberate change from the old CLI-local item:
    /// gains `registered_at_ms` and `in_flight_count`, drops the
    /// now-dependent `heartbeat_age_ms` (consumers derive age from
    /// `last_heartbeat_ms`; the view stays wall-clock-free).
    #[test]
    fn worker_view_serialises_to_stable_json_shape() {
        let item = fq_runtime::views::WorkerView {
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

#[cfg(test)]
mod ambiguous_once_tests {
    use super::*;
    use std::time::Duration;

    /// The #64 idempotency AC: a persistently-failing recovery
    /// (re-classified or re-failed on every daemon restart) emits
    /// `invocation.ambiguous` exactly once, not once per restart.
    #[tokio::test]
    async fn publish_ambiguous_once_fires_exactly_once_per_invocation() {
        let server = fq_test_support::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempfile::tempdir().unwrap();
        let wstore = fq_runtime::WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .expect("open worker store");

        let inv_id = Uuid::now_v7();
        let agent = format!("amb-once-{}", Uuid::now_v7().simple());
        wstore
            .upsert_invocation_state(&fq_runtime::worker::InvocationStateRow {
                invocation_id: inv_id.to_string(),
                agent_id: agent.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: b"{}".to_vec(),
                step_index: 0,
                started_at: 1_000,
                updated_at: 1_000,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            })
            .await
            .unwrap();

        let mut sub = bus
            .subscribe(format!("fq.agent.{agent}.invocation.ambiguous"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let payload = || fq_runtime::events::InvocationAmbiguousPayload {
            stuck_entity: "recovery".to_string(),
            stuck_call_id: inv_id.to_string(),
            note: "resume failed (test)".to_string(),
        };
        let agent_id = AgentId::new(agent.clone()).unwrap();

        // First failure publishes…
        publish_ambiguous_once(
            &wstore,
            &bus,
            agent_id.clone(),
            inv_id,
            &inv_id.to_string(),
            payload(),
        )
        .await;
        let event = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("invocation.ambiguous within 5s")
            .expect("stream open")
            .expect("event deserialises");
        assert!(matches!(
            event.payload,
            EventPayload::InvocationAmbiguous(_)
        ));

        // …the second (same invocation, "next restart") is stamped out.
        publish_ambiguous_once(
            &wstore,
            &bus,
            agent_id,
            inv_id,
            &inv_id.to_string(),
            payload(),
        )
        .await;
        let quiet = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
        assert!(
            quiet.is_err(),
            "second failure must not re-publish invocation.ambiguous"
        );
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use tempfile::tempdir;

    fn write_agent(dir: &Path, name: &str) {
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!("---\nname: {name}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."),
        )
        .unwrap();
    }

    /// A reload re-reads the agents directory and swaps the shared
    /// handle in place: the same `SharedRegistry` the dispatcher
    /// holds now points at the freshly-loaded registry, so the next
    /// trigger sees the new agent set.
    #[tokio::test]
    async fn reload_agents_swaps_in_new_definitions() {
        let dir = tempdir().unwrap();
        write_agent(dir.path(), "first");

        let initial = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
        assert_eq!(initial.len(), 1);
        let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

        // Add a second agent on disk, then reload.
        write_agent(dir.path(), "second");
        reload_agents(&shared, dir.path(), None).await;

        let after = shared.read().await.clone();
        assert_eq!(after.len(), 2, "reload should pick up the new agent");
        assert!(after.get(&AgentId::new("second").unwrap()).is_some());
    }

    /// A reload against a directory that has gone missing keeps the
    /// current registry rather than blanking it — a bad edit can't
    /// knock out a running daemon.
    #[tokio::test]
    async fn reload_agents_keeps_current_registry_on_load_error() {
        let dir = tempdir().unwrap();
        write_agent(dir.path(), "keep");
        let initial = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
        assert_eq!(initial.len(), 1);
        let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

        // Point the reload at a directory that does not exist.
        let missing = dir.path().join("does-not-exist");
        reload_agents(&shared, &missing, None).await;

        let after = shared.read().await.clone();
        assert_eq!(after.len(), 1, "failed reload must keep the old registry");
        assert!(after.get(&AgentId::new("keep").unwrap()).is_some());
    }
}

#[cfg(test)]
mod log_format_tests {
    use super::*;
    use clap::Parser;

    /// The default (no flag, no env) is `text`, preserving the
    /// existing human-readable output.
    #[test]
    fn log_format_defaults_to_text() {
        let cli = Cli::parse_from(["fq", "run"]);
        assert_eq!(cli.global.log_format, LogFormat::Text);
    }

    /// `--log-format json` parses to the JSON renderer.
    #[test]
    fn log_format_json_flag_parses() {
        let cli = Cli::parse_from(["fq", "--log-format", "json", "run"]);
        assert_eq!(cli.global.log_format, LogFormat::Json);
    }

    /// `--log-format text` parses to the text renderer.
    #[test]
    fn log_format_text_flag_parses() {
        let cli = Cli::parse_from(["fq", "--log-format", "text", "run"]);
        assert_eq!(cli.global.log_format, LogFormat::Text);
    }

    /// The flag is global — it can follow the subcommand too.
    #[test]
    fn log_format_flag_is_global() {
        let cli = Cli::parse_from(["fq", "status", "--log-format", "json"]);
        assert_eq!(cli.global.log_format, LogFormat::Json);
    }

    /// An unknown value is rejected rather than silently defaulting.
    #[test]
    fn log_format_rejects_unknown_value() {
        let result = Cli::try_parse_from(["fq", "--log-format", "yaml", "run"]);
        let err = match result {
            Ok(_) => panic!("unknown log-format value should be rejected"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("yaml") || msg.contains("possible values"),
            "got: {msg}"
        );
    }

    /// The JSON formatter layer builds and renders a structured event
    /// as parseable JSON with the fields intact. Uses a
    /// `tracing_subscriber::fmt` layer with a captured writer rather
    /// than the process-global subscriber (which can only be set once),
    /// but exercises the same `.json()` renderer `init_tracing` wires up.
    #[test]
    fn json_layer_emits_parseable_json_with_fields() {
        use std::sync::{Arc, Mutex};
        use tracing::subscriber;
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for SharedBuf {
            type Writer = SharedBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .json()
            .with_writer(buf.clone())
            .finish();

        subscriber::with_default(subscriber, || {
            tracing::warn!(
                invocation_id = "inv-42",
                worker_id = "w-1",
                "structured event"
            );
        });

        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = raw.lines().next().expect("expected at least one log line");
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("each log line must be a JSON object");
        assert_eq!(parsed["level"], "WARN");
        assert_eq!(parsed["fields"]["message"], "structured event");
        assert_eq!(parsed["fields"]["invocation_id"], "inv-42");
        assert_eq!(parsed["fields"]["worker_id"], "w-1");
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;
    use fq_runtime::views::{ExecutionsView, FailureView, WorkerView};

    fn worker(id: &str, status: &str, last_heartbeat: i64) -> WorkerView {
        WorkerView {
            worker_id: id.to_string(),
            host: "h".to_string(),
            registered_at_ms: 0,
            last_heartbeat_ms: last_heartbeat,
            status: status.to_string(),
            in_flight_count: 0,
        }
    }

    /// The in-flight/stuck determination itself (threshold, clock skew)
    /// is `views::Views::executions`' job and is covered by its tests;
    /// doctor receives the finished counts.
    fn executions(in_flight: i64, stuck_ids: &[&str]) -> ExecutionsView {
        ExecutionsView {
            in_flight,
            working: 0,
            working_ids: vec![],
            stuck: stuck_ids.len() as i64,
            stuck_ids: stuck_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    const NOW: i64 = 1_000_000;

    #[test]
    fn all_clear_when_everything_healthy() {
        let workers = vec![worker("w1", "alive", NOW)];
        let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

        assert!(!report.has_issues());
        assert_eq!(report.workers.alive, 1);
        assert_eq!(report.workers.stale, 0);
        assert_eq!(report.executions.in_flight, 0);
        assert_eq!(report.failure_total(), 0);
        assert_eq!(
            report.dead_letters,
            DoctorDeadLetters {
                exhausted_triggers: 0
            }
        );

        let out = render_doctor_report_human(&report);
        assert!(out.contains("All clear."), "got: {out}");
        // Dead-letter section is always shown.
        assert!(out.contains("Dead-letters: none"), "got: {out}");
    }

    #[test]
    fn running_in_flight_work_is_not_an_issue() {
        // In-flight but not stuck is healthy.
        let report = build_doctor_report(&[], &executions(1, &[]), 0, &[]);
        assert_eq!(report.executions.in_flight, 1);
        assert_eq!(report.executions.stuck, 0);
        assert!(!report.has_issues());
    }

    #[test]
    fn stale_workers_flagged_with_ids() {
        let workers = vec![
            worker("alive-1", "alive", NOW),
            worker("stale-1", "stale", NOW - 60_000),
            worker("gone-1", "shutdown", 0),
        ];
        let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

        assert_eq!(report.workers.alive, 1);
        assert_eq!(report.workers.stale, 1);
        assert_eq!(report.workers.shutdown, 1);
        assert_eq!(report.workers.stale_ids, vec!["stale-1".to_string()]);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(out.contains("1 alive, 1 stale, 1 shutdown"), "got: {out}");
        assert!(out.contains("fq workers list --stale-only"), "got: {out}");
        assert!(!out.contains("All clear."), "got: {out}");
    }

    #[test]
    fn stuck_in_flight_flagged() {
        let report = build_doctor_report(&[], &executions(2, &["stuck-abcdef01"]), 0, &[]);

        assert_eq!(report.executions.in_flight, 2);
        assert_eq!(report.executions.stuck, 1);
        // Short id (8 chars) recorded for triage.
        assert_eq!(report.executions.stuck_ids, vec!["stuck-ab".to_string()]);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(
            out.contains("2 in-flight (0 working, 1 stuck)"),
            "got: {out}"
        );
        assert!(out.contains("fq invocation drop"), "got: {out}");
    }

    /// Working invocations (fresh open dispatch, #130) surface in the human
    /// report but are healthy — no issue, no remediation hint.
    #[test]
    fn working_in_flight_shown_but_not_an_issue() {
        let ex = ExecutionsView {
            in_flight: 2,
            working: 1,
            working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
            stuck: 0,
            stuck_ids: vec![],
        };
        let report = build_doctor_report(&[], &ex, 0, &[]);

        assert!(!report.has_issues());
        // Short id (8 chars), same convention as stuck_ids.
        assert_eq!(report.executions.working_ids, vec!["019f5b3f".to_string()]);

        let out = render_doctor_report_human(&report);
        assert!(
            out.contains("2 in-flight (1 working, 0 stuck)"),
            "got: {out}"
        );
        assert!(!out.contains("fq invocation drop"), "got: {out}");
    }

    /// #49: dead-lettered triggers surface as their own doctor line,
    /// counted from the `trigger_exhausted` failures bucket.
    #[test]
    fn dead_lettered_triggers_are_counted_and_rendered() {
        let failures = vec![
            FailureView {
                error_kind: "trigger_exhausted".to_string(),
                count: 2,
            },
            FailureView {
                error_kind: "tool_error".to_string(),
                count: 1,
            },
        ];
        let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);
        assert_eq!(
            report.dead_letters,
            DoctorDeadLetters {
                exhausted_triggers: 2
            }
        );
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(
            out.contains("Dead-letters: 2 exhausted trigger(s)"),
            "got: {out}"
        );
        assert!(out.contains("fq dead-letters list"), "got: {out}");
        assert!(out.contains("fq dead-letters requeue"), "got: {out}");
    }

    #[test]
    fn ambiguous_flagged() {
        let report = build_doctor_report(&[], &ExecutionsView::default(), 3, &[]);
        assert_eq!(report.ambiguous, 3);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(out.contains("Ambiguous invocations: 3"), "got: {out}");
        assert!(
            out.contains("fq invocation list --status=ambiguous"),
            "got: {out}"
        );
    }

    #[test]
    fn permanent_failures_grouped_by_kind() {
        let failures = vec![
            FailureView {
                error_kind: "budget_exceeded".to_string(),
                count: 2,
            },
            FailureView {
                error_kind: "tool_error".to_string(),
                count: 1,
            },
        ];
        let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);

        assert_eq!(report.failure_total(), 3);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(out.contains("Permanent failures: 3"), "got: {out}");
        assert!(out.contains("budget_exceeded: 2"), "got: {out}");
        assert!(out.contains("tool_error: 1"), "got: {out}");
        assert!(
            out.contains("fq invocation list --status=failed"),
            "got: {out}"
        );
    }

    #[test]
    fn report_serialises_to_stable_json_shape() {
        let report = build_doctor_report(
            &[worker("w1", "alive", NOW)],
            &executions(1, &[]),
            1,
            &[FailureView {
                error_kind: "runtimeerror".to_string(),
                count: 4,
            }],
        );
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["workers"]["alive"], 1);
        assert_eq!(v["executions"]["in_flight"], 1);
        assert_eq!(v["ambiguous"], 1);
        assert_eq!(v["failures"][0]["error_kind"], "runtimeerror");
        assert_eq!(v["failures"][0]["count"], 4);
        assert_eq!(v["dead_letters"]["exhausted_triggers"], 0);
    }

    /// The count derives only from the `triggerexhausted` bucket —
    /// other failure kinds never inflate it.
    #[test]
    fn dead_letters_never_fabricates_a_count() {
        let report = build_doctor_report(
            &[],
            &ExecutionsView::default(),
            0,
            &[FailureView {
                error_kind: "runtimeerror".to_string(),
                count: 7,
            }],
        );
        assert_eq!(report.dead_letters.exhausted_triggers, 0);
        let out = render_doctor_report_human(&report);
        assert!(out.contains("Dead-letters: none"), "got: {out}");
    }
}
