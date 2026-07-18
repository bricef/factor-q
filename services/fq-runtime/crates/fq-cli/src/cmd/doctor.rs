// ============================================================
// fq doctor — one-shot durable-execution health report
// ============================================================

/// Stuck-work threshold: an in-flight invocation whose
/// `invocation_state.updated_at` is older than this many ms is
/// flagged "stuck" by `fq doctor`. Reuses the control-plane's
/// stale-worker value (`DEFAULT_STALE_THRESHOLD_MS = 30_000`,
/// `coordination_consumer.rs:66`) rather than inventing a third
/// hard-coded constant — an invocation that has not touched its
/// WAL row in as long as a worker has not heartbeated is the same
/// order of "not making progress" signal.
const DOCTOR_STUCK_THRESHOLD_MS: i64 = 30_000;

/// Worker liveness counts plus the ids of any stale workers so
/// the operator can act without a second `fq workers list` call.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq, Default)]
struct DoctorWorkers {
    alive: i64,
    stale: i64,
    shutdown: i64,
    /// Worker ids currently past the stale threshold.
    stale_ids: Vec<String>,
}

/// In-flight / current-execution view, read from the worker-local
/// `invocation_state` table (the reliable live view — the CP owner
/// table's `in_flight` status is not populated by trigger dispatch
/// yet; see issue #50).
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq, Default)]
struct DoctorExecutions {
    in_flight: i64,
    /// In-flight invocations with a fresh open dispatch (tool or LLM) —
    /// actively working, however silent their WAL row (#130).
    working: i64,
    /// Short ids of the working invocations, same convention as
    /// `stuck_ids`.
    working_ids: Vec<String>,
    /// In-flight invocations whose `updated_at` is older than
    /// [`DOCTOR_STUCK_THRESHOLD_MS`].
    stuck: i64,
    /// Short ids of the stuck invocations, for triage.
    stuck_ids: Vec<String>,
}

/// Availability of the dead-letter section. Gated on issue #49:
/// Dead-lettered triggers (#49): transient pre-WAL failures that
/// exhausted the trigger consumer's delivery bound. The dispatcher
/// consumes the exhausted trigger and emits a terminal `failed` event
/// with kind [`DEAD_LETTER_KIND`]; this counts that bucket, so the
/// report needs no extra query. The event's annotations carry the
/// trigger subject and payload for requeue/diagnosis.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
struct DoctorDeadLetters {
    exhausted_triggers: i64,
}

/// The projection's failure-kind string for a dead-lettered trigger —
/// `FailureKind::TriggerExhausted` serialized with the wire vocabulary.
const DEAD_LETTER_KIND: &str = "trigger_exhausted";

/// The full doctor report. Serialisable for `--json`; built by the
/// pure [`build_doctor_report`] so the checks are unit-testable
/// without a live DB.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
struct DoctorReport {
    workers: DoctorWorkers,
    executions: DoctorExecutions,
    /// Ambiguous invocations needing operator triage (CP owner
    /// table, `status='ambiguous'`).
    ambiguous: i64,
    /// Terminal failures grouped by `FailureKind` (from the
    /// projection `events` table, `event_type='failed'`).
    failures: Vec<DoctorFailure>,
    dead_letters: DoctorDeadLetters,
}

/// One failure-kind bucket in the report. Mirrors
/// [`fq_runtime::views::FailureView`] but owns its data so the report
/// is a self-contained serialisable value.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
struct DoctorFailure {
    error_kind: String,
    count: i64,
}

impl DoctorReport {
    /// Total terminal failures across all kinds.
    fn failure_total(&self) -> i64 {
        self.failures.iter().map(|f| f.count).sum()
    }

    /// True when any check reports a problem worth an operator's
    /// attention: stale workers, stuck in-flight work, ambiguous
    /// invocations, or permanent failures. In-flight work that is
    /// merely running (not stuck) is healthy, not an issue.
    fn has_issues(&self) -> bool {
        self.workers.stale > 0
            || self.executions.stuck > 0
            || self.ambiguous > 0
            || self.failure_total() > 0
    }
}

/// Pure: assemble a [`DoctorReport`] from the already-fetched read
/// views, so it can be unit-tested without a database. The stuck
/// determination (threshold + clock-skew handling) lives in
/// [`fq_runtime::views::Views::executions`]; this builder only
/// aggregates and shortens ids for triage.
fn build_doctor_report(
    workers: &[fq_runtime::views::WorkerView],
    executions: &fq_runtime::views::ExecutionsView,
    ambiguous: i64,
    failures: &[fq_runtime::views::FailureView],
) -> DoctorReport {
    let mut w = DoctorWorkers::default();
    for row in workers {
        match row.status.as_str() {
            "alive" => w.alive += 1,
            "stale" => {
                w.stale += 1;
                w.stale_ids.push(row.worker_id.clone());
            }
            "shutdown" => w.shutdown += 1,
            // The control-plane only records the three statuses above;
            // an unknown value would mean a store/view drift — count it
            // as stale so it surfaces as an issue rather than vanishing.
            _ => {
                w.stale += 1;
                w.stale_ids.push(row.worker_id.clone());
            }
        }
    }

    // Short ids (8 chars) for triage, matching the human report.
    let short = |ids: &[String]| -> Vec<String> {
        ids.iter().map(|id| id.chars().take(8).collect()).collect()
    };
    let ex = DoctorExecutions {
        in_flight: executions.in_flight,
        working: executions.working,
        working_ids: short(&executions.working_ids),
        stuck: executions.stuck,
        stuck_ids: short(&executions.stuck_ids),
    };

    let failures: Vec<DoctorFailure> = failures
        .iter()
        .map(|f| DoctorFailure {
            error_kind: f.error_kind.clone(),
            count: f.count,
        })
        .collect();

    let dead_letters = DoctorDeadLetters {
        exhausted_triggers: failures
            .iter()
            .filter(|f| f.error_kind == DEAD_LETTER_KIND)
            .map(|f| f.count)
            .sum(),
    };

    DoctorReport {
        workers: w,
        executions: ex,
        ambiguous,
        failures,
        dead_letters,
    }
}

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

/// `fq doctor`: aggregate the DB-backed durable-execution health
/// signals into one report. Read-only against the SQLite projection
/// DB — no NATS round-trip — so it works with `fq run` stopped.
///
/// Opens the three read-only stores against the single projection DB
/// file, reads each check's source, then hands the raw rows to the
/// pure [`build_doctor_report`] / [`render_doctor_report_human`] so
/// the aggregation and formatting stay testable.
async fn doctor(global: &GlobalArgs, json: bool, fail_on_issues: bool) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let now_ms = chrono::Utc::now().timestamp_millis();

    let workers = views.workers().await?;
    let executions = views
        .executions(
            now_ms,
            DOCTOR_STUCK_THRESHOLD_MS,
            fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
        )
        .await?;
    let ambiguous = views
        .recovery(now_ms, DOCTOR_STUCK_THRESHOLD_MS)
        .await?
        .ambiguous;
    let failures = views.failures().await?;

    let report = build_doctor_report(&workers, &executions, ambiguous, &failures);

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

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Publish `invocation.ambiguous` at most once per invocation (#64).
///
/// Claims the worker store's one-shot stamp (`ambiguous_reported_at`)
/// before publishing, so a restart that re-classifies the same
/// invocation as ambiguous — or re-fails the same resume — does not
/// re-fire the event. `stamp_key` is the store's invocation-id string
/// (normally equal to `invocation_id`; the scan path passes the raw
/// row id so a malformed stored uuid still stamps its own row).
///
/// Claim-then-publish is deliberately at-most-once: a publish failure
/// after a successful claim is logged and not retried. A claim *error*
/// (store unavailable) publishes anyway — it doesn't prove the event
/// was already sent, and a possible duplicate beats re-silencing the
/// failure mode #64 exists to make loud.
async fn publish_ambiguous_once(
    worker_store: &fq_runtime::WorkerStore,
    bus: &EventBus,
    agent_id: AgentId,
    invocation_id: Uuid,
    stamp_key: &str,
    payload: fq_runtime::events::InvocationAmbiguousPayload,
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    match worker_store
        .mark_ambiguous_reported(stamp_key, now_ms)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                invocation_id = %invocation_id,
                "invocation.ambiguous already reported for this invocation; not re-firing"
            );
            return;
        }
        Err(err) => {
            tracing::error!(
                invocation_id = %invocation_id,
                error = %err,
                "failed to claim ambiguous-report stamp; publishing anyway (may duplicate)"
            );
        }
    }
    let event = Event::new(
        agent_id,
        invocation_id,
        EventPayload::InvocationAmbiguous(payload),
    );
    if let Err(err) = bus.publish(&event).await {
        tracing::error!(
            invocation_id = %invocation_id,
            error = %err,
            "failed to publish invocation.ambiguous (stamp already claimed; will not retry)"
        );
    }
}

