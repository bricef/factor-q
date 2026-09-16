//! The reports over the edge (plan Phase 4, verbs 13, 14 and 15) —
//! every one the surface declares: `cost.summary`, `cost.by_agent`,
//! `control.doctor`, `control.status`.
//!
//! What is proved here is what only the wire can prove. The
//! aggregation itself is `Views`' and is covered by its tests; the
//! doctor composite's arithmetic is covered in `doctor_report/tests.rs`.
//! What neither can reach is the three properties a *declared* report
//! has and a function call does not:
//!
//! * **A report is a privilege boundary.** Authority is Read on the
//!   report's own scope, never on its inputs — which is much of the
//!   point of putting aggregates on the surface. A token holding
//!   `read:cost` and nothing else must be able to read fleet spend
//!   while being refused the event log that spend is computed from,
//!   and must be refused `control.doctor`, which is a different scope
//!   entirely. That claim is untestable anywhere but here.
//! * **A parameter is validated, not absorbed.** An unparseable
//!   `since` is a verdict on the request, not a reason to answer over
//!   the whole history — the failure mode where a narrowing silently
//!   widens.
//! * **Absence has a shape.** `cost.by_agent` on an agent with no
//!   spend is NotFound, not an empty breakdown that a caller would
//!   render as "this agent cost nothing".

#![cfg(unix)]

use std::process::Stdio;

use fq_test_support::TestChild;
use std::time::Duration;

use fq_ops::{ControlReport, CostReport, Domain, OpId, ReportId};
use fq_runtime::events::{
    CostMetadata, Event, EventPayload, LlmCallOrigin, StopReason, TokenUsage,
};
use fq_runtime::{AgentId, ProjectionStore};
use serde_json::json;
use uuid::Uuid;

/// Epoch for the seeded rows: fixed, far enough in the past that
/// nothing here depends on wall-clock drift.
const BASE_MS: i64 = 1_767_323_045_000;
const INVOCATION: &str = "1c000000-0000-7000-8000-000000000001";
const AGENT: &str = "researcher";

/// The two figures the allocation rule (#466) relates: what an
/// invocation cost, and what the engine spent on its behalf.
const INVOCATION_COST: f64 = 0.0125;
const FRAMEWORK_COST: f64 = 0.0009;

struct ScratchDir(Option<tempfile::TempDir>);

impl ScratchDir {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("edge-reports-")
            .tempdir_in(std::env::temp_dir())
            .expect("create scratch directory");
        std::fs::create_dir_all(dir.path().join("cache")).unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(
            dir.path().join("fq.toml"),
            "[edge]\nbind = \"127.0.0.1:0\"\n",
        )
        .unwrap();
        Self(Some(dir))
    }

    fn join(&self, path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
        self.0
            .as_ref()
            .expect("scratch guard present")
            .path()
            .join(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let path = self.0.take().expect("scratch guard present").keep();
            eprintln!("test failed; scratch preserved at {}", path.display());
        }
    }
}

fn unique_scratch() -> ScratchDir {
    ScratchDir::new()
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

fn fixed_uuid(n: u32) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-7000-8000-0000000010{n:02}")).unwrap()
}

fn stamp(mut event: Event, seq: u32, at_ms: i64) -> Event {
    event.envelope.event_id = fixed_uuid(seq);
    event.envelope.timestamp = chrono::DateTime::from_timestamp_millis(at_ms).unwrap();
    event
}

fn cost(seq: u32, total_cost: f64, model: &str) -> CostMetadata {
    CostMetadata {
        call_id: fixed_uuid(seq),
        model: model.into(),
        input_tokens: 1_200,
        output_tokens: 340,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: total_cost * 0.8,
        output_cost: total_cost * 0.2,
        total_cost,
        cumulative_invocation_cost: total_cost,
        cumulative_agent_cost: total_cost,
        origin: LlmCallOrigin::AgentTurn,
        reasoning_tokens: None,
        reported_cost: None,
        pricing_table: None,
    }
}

/// One priced agent turn and one priced invocation summary against the
/// same invocation — the smallest world in which the allocation rule
/// says something: the agent's row carries only its own spend, the
/// reserved `summary` row carries only the engine's, and the fleet
/// total is both.
async fn seed_costs(cache: &std::path::Path) {
    let paths = fq_runtime::db::RuntimeDbPaths::under(cache);
    let proj = ProjectionStore::open(&paths.projection)
        .await
        .expect("open projection");

    let invocation = Uuid::parse_str(INVOCATION).unwrap();
    let response = stamp(
        Event::new(
            AgentId::new(AGENT).unwrap(),
            invocation,
            EventPayload::LlmResponse(fq_runtime::events::LlmResponsePayload {
                parts: fq_runtime::events::assistant_parts(Some("Probe reply.".into()), Vec::new()),
                round: 0,
                call_id: fixed_uuid(2),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 1_200,
                    output_tokens: 340,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                },
                origin: LlmCallOrigin::AgentTurn,
            }),
        ),
        2,
        BASE_MS + 1_000,
    )
    .with_cost(cost(2, INVOCATION_COST, "claude-haiku"));

    let summary = stamp(
        Event::new(
            AgentId::summary(),
            invocation,
            EventPayload::InvocationSummary(fq_runtime::events::InvocationSummaryPayload {
                kind: fq_runtime::events::SummaryKind::Outcome,
                summary: "Probe run finished clean.".into(),
            }),
        ),
        3,
        BASE_MS + 2_000,
    )
    .with_cost(cost(3, FRAMEWORK_COST, "cheap-model"));

    for event in [response, summary] {
        proj.insert_event(&event, None).await.expect("insert event");
    }
}

/// A running daemon with the costs above already projected, plus the
/// credentials to talk to it.
struct Daemon {
    /// Held, never read: dropping it is what stops the daemon (#630),
    /// so the field is load-bearing exactly where the lint cannot see.
    #[allow(dead_code)]
    process: TestChild,
    _scratch: ScratchDir,
    addr: String,
    fingerprint: [u8; 32],
    admin_token: String,
}

async fn start_daemon(server: &fq_test_support::NatsServer) -> Daemon {
    let scratch = unique_scratch();
    seed_costs(&scratch.join("cache")).await;

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut process = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("RUST_LOG", "off")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Some(status) = process.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if text.contains("Runtime ready") {
            break text;
        }
        assert!(tokio::time::Instant::now() < deadline, "fqd never ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    Daemon {
        addr: suffix_of(&text, "- edge is listening on ").to_string(),
        fingerprint: parse_fingerprint(suffix_of(
            &text,
            "edge: certificate fingerprint (clients pin this): ",
        )),
        admin_token: fq_test_support::admin_token(&scratch.join("state")),
        process,
        _scratch: scratch,
    }
}

async fn invoke(
    client: &fq_edge::EdgeClient,
    op: OpId,
    input: serde_json::Value,
) -> Result<serde_json::Value, fq_edge::wire::WireError> {
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op,
                version: 1,
                input,
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .map(|r| r.output)
}

fn cost_summary() -> OpId {
    OpId::Report(ReportId::Cost(CostReport::Summary))
}

fn cost_by_agent() -> OpId {
    OpId::Report(ReportId::Cost(CostReport::ByAgent))
}

fn control_doctor() -> OpId {
    OpId::Report(ReportId::Control(ControlReport::Doctor))
}

fn control_status() -> OpId {
    OpId::Report(ReportId::Control(ControlReport::Status))
}

const EXPECTED_DURABLES: [&str; 6] = [
    "fq-projector",
    "fq-coordination",
    "fq-heartbeat",
    "fq-dispatcher",
    "fq-advisory-watch",
    "fq-maintenance",
];

fn durables_ready(consumers: &[serde_json::Value], expected: &[&str]) -> bool {
    let mut names = Vec::with_capacity(consumers.len());
    for consumer in consumers {
        let active = &consumer["active"];
        if active.is_null() {
            return false;
        }
        let Some(name) = active["name"].as_str().filter(|name| !name.is_empty()) else {
            return false;
        };
        names.push(name);
    }
    names.sort_unstable();

    let mut expected = expected.to_vec();
    expected.sort_unstable();
    names == expected
}

/// `control.doctor` reports the durables as one flat array, whatever
/// stream each of them reads.
fn doctor_consumers(report: &serde_json::Value) -> Vec<serde_json::Value> {
    report["consumers"].as_array().cloned().unwrap_or_default()
}

/// `control.status` reports them per stream, inside the stream's
/// `available` block — a stream the probe could not read carries none,
/// which is an incomplete roster and so not yet ready.
fn status_consumers(report: &serde_json::Value) -> Vec<serde_json::Value> {
    report["streams"]
        .as_array()
        .map(|streams| {
            streams
                .iter()
                .filter_map(|stream| stream["available"]["consumers"].as_array())
                .flatten()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until every durable this daemon expects has been created and is
/// readable, and hand back **the report that proved it**.
///
/// The consumers are made by the hosted tasks *after* the edge starts
/// serving, so a report taken the instant the daemon is connectable can
/// legitimately catch one that does not exist yet and report it
/// `Missing` — which is the probe telling the truth, and the two tests
/// below asserting about a moment rather than about the daemon.
/// Polling until the exact roster is active keeps the assertions exact
/// instead of loosening them to accept an absence.
///
/// Returning the satisfying report is the other half: the caller
/// asserts about the state it actually observed, so a transient probe
/// failure in the gap between the poll and a second, fresh call cannot
/// put the consumer assertions back on a report nobody checked.
///
/// `op` and `consumers` let both report shapes share this loop — each
/// test waits on the very report it goes on to assert about, rather
/// than on a sibling that merely starts at the same time.
async fn wait_for_durables(
    client: &fq_edge::EdgeClient,
    op: fn() -> OpId,
    expected: &[&str],
    consumers: fn(&serde_json::Value) -> Vec<serde_json::Value>,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let report = invoke(client, op(), json!({})).await.expect("report");
        if durables_ready(&consumers(&report), expected) {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a durable never appeared: {report}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The predicate, against the shapes the daemon actually emits.
///
/// `ConsumerHealth` is externally tagged, so every entry is a one-key
/// object naming its state — `{"active": {…}}`, `{"missing": {…}}` —
/// and the cases here are serialised from the enum itself rather than
/// hand-written. A rename or a re-tagging in `fq-ops` then fails this
/// test, instead of silently leaving a predicate that recognises a
/// wire format nobody speaks any more.
#[test]
fn durable_wait_requires_the_complete_active_roster() {
    use fq_ops::health::{ConsumerHealth, UnsupportedEvent};

    fn wire(consumer: ConsumerHealth) -> serde_json::Value {
        serde_json::to_value(consumer).expect("a consumer health serialises")
    }
    fn active(name: &str) -> serde_json::Value {
        wire(ConsumerHealth::Active {
            name: name.to_string(),
            delivered: 0,
            ack_pending: 0,
            num_pending: 0,
            num_redelivered: 0,
            redeliveries: 0,
            stuck: false,
            malformed_acked: 0,
        })
    }
    let roster = |consumers: &[serde_json::Value]| durables_ready(consumers, &EXPECTED_DURABLES);
    let mut consumers: Vec<_> = EXPECTED_DURABLES.iter().map(|n| active(n)).collect();
    assert!(roster(&consumers), "the settled roster is ready");

    // The startup shape this wait exists for: `probe_core_consumers`
    // walks the expected roster and emits `Missing` for a durable no
    // daemon has created yet, so an incomplete startup is a full-length
    // list with a `missing` entry in it, never a short one.
    let mut with_missing = consumers.clone();
    with_missing[0] = wire(ConsumerHealth::Missing {
        name: EXPECTED_DURABLES[0].to_string(),
    });
    assert!(!roster(&with_missing), "a missing durable is not ready");

    // The probe read the consumer and JetStream refused: named, but
    // nothing is known about its progress.
    let mut with_error = consumers.clone();
    with_error[1] = wire(ConsumerHealth::Error {
        name: EXPECTED_DURABLES[1].to_string(),
        error: "consumer info: timeout".to_string(),
    });
    assert!(!roster(&with_error), "an unreadable durable is not ready");

    // Halted: it exists and is named, but it is parked on an event it
    // cannot read and is making no progress — not a daemon to assert
    // a healthy roster about.
    let mut with_halted = consumers.clone();
    with_halted[2] = wire(ConsumerHealth::Halted {
        name: EXPECTED_DURABLES[2].to_string(),
        halted_on: UnsupportedEvent {
            schema_version: 99,
            supported: vec![1],
            event_id: None,
            subject: "fq.events.v99".to_string(),
            stream_seq: Some(7),
        },
        malformed_acked: 0,
    });
    assert!(!roster(&with_halted), "a halted durable is not ready");

    // A seventh durable is as wrong as a missing one: the roster is
    // compared, not contained, so a consumer this fixture does not
    // configure fails the wait rather than passing it early.
    let mut superset = consumers.clone();
    superset.push(active("fq-summariser"));
    assert!(
        !roster(&superset),
        "an unexpected durable is not the roster"
    );

    // A named surface that names nothing: `expect("a named consumer")`
    // downstream would take an empty string, so the wait refuses it
    // here.
    let mut unnamed = consumers.clone();
    unnamed[3] = active("");
    assert!(!roster(&unnamed), "an empty name is not a name");

    // Defensive only: `ConsumerHealth` is externally tagged, so a JSON
    // null under `active` is a shape the daemon cannot emit. Kept
    // because the predicate indexes into `active` and the cost of the
    // guard is one branch — if the encoding ever becomes internally
    // tagged or optional, this is the case that notices.
    let mut null_active = consumers.clone();
    null_active[0]["active"] = serde_json::Value::Null;
    assert!(!roster(&null_active), "a null active block is not ready");

    // A short list: whatever produced it, it is not the roster.
    consumers.truncate(5);
    assert!(!roster(&consumers), "five of six is not the roster");
}

/// Close enough for money: the figures cross a JSON wire as f64, so
/// exact equality would be asserting about IEEE-754 rather than about
/// the report.
fn about(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The fleet report, and the identity its declaration promises:
/// `total_cost = <per-invocation costs> + framework_cost`, with the
/// remainder named rather than merely missing.
#[tokio::test]
async fn cost_summary_reports_spend_and_names_the_unallocated_remainder() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    let report: fq_runtime::views::CostReport = serde_json::from_value(
        invoke(&client, cost_summary(), json!({}))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");

    assert!(
        about(report.total_cost, INVOCATION_COST + FRAMEWORK_COST),
        "the fleet total is both kinds of spend: {report:?}"
    );
    assert!(
        about(report.framework_cost, FRAMEWORK_COST),
        "the engine's own spend is named on the report: {report:?}"
    );
    assert!(
        about(report.total_cost - report.framework_cost, INVOCATION_COST),
        "the remainder must reconcile, not merely be absent: {report:?}"
    );

    // The rule holds per agent too, which is what makes the drill-down
    // self-explaining: the agent carries none of it, the reserved
    // `summary` row carries all of it.
    let by_id = |id: &str| {
        report
            .agents
            .iter()
            .find(|a| a.agent_id == id)
            .unwrap_or_else(|| panic!("no `{id}` row in {report:?}"))
            .clone()
    };
    assert!(about(by_id(AGENT).framework_cost, 0.0));
    assert!(about(by_id(AGENT).total_cost, INVOCATION_COST));
    let summary_row = by_id("summary");
    assert!(about(summary_row.framework_cost, summary_row.total_cost));
}

/// The drill-down, including the case that looks like missing data and
/// is not: the reserved `summary` agent has spend and no invocations
/// under it, by construction.
#[tokio::test]
async fn cost_by_agent_drills_down_and_is_not_found_when_there_is_nothing_to_drill() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    let detail: fq_runtime::views::AgentCostDetailView = serde_json::from_value(
        invoke(&client, cost_by_agent(), json!({ "agent": AGENT }))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");
    assert_eq!(detail.agent_id, AGENT);
    assert!(about(detail.totals.total_cost, INVOCATION_COST));
    assert_eq!(
        detail.invocations.len(),
        1,
        "the agent's spend is allocated to its invocation: {detail:?}"
    );

    let framework: fq_runtime::views::AgentCostDetailView = serde_json::from_value(
        invoke(&client, cost_by_agent(), json!({ "agent": "summary" }))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");
    assert!(
        about(framework.totals.framework_cost, framework.totals.total_cost),
        "engine spend is the whole of the summary row: {framework:?}"
    );
    assert!(
        framework.invocations.is_empty(),
        "engine spend is charged to no invocation — an empty list here is the rule \
         showing through, not missing data: {framework:?}"
    );

    // An agent with no spend has no row of the summary to drill into,
    // so it is not found rather than a breakdown of zero — which a
    // caller would render as "this agent cost nothing".
    let err = invoke(&client, cost_by_agent(), json!({ "agent": "never-ran" }))
        .await
        .expect_err("an agent with no spend is not found");
    assert!(
        matches!(err, fq_edge::wire::WireError::NotFound { .. }),
        "got: {err:?}"
    );
}

/// A narrowing that cannot be understood must not silently widen. The
/// grammar is `views::since`'s, and the refusal quotes both the
/// spelling and the accepted set.
#[tokio::test]
async fn a_since_that_names_no_instant_is_refused_rather_than_ignored() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    for op in [cost_summary(), cost_by_agent()] {
        let err = invoke(
            &client,
            op.clone(),
            json!({ "agent": AGENT, "since": "last tuesday" }),
        )
        .await
        .expect_err("an unparseable since is a verdict on the request");
        match err {
            fq_edge::wire::WireError::InvalidInput { op: named, message } => {
                assert_eq!(named, op.to_string());
                assert!(message.contains("last tuesday"), "got: {message}");
            }
            other => panic!("{op} should refuse, got: {other:?}"),
        }
    }

    // And a spelling it *does* understand narrows rather than refuses:
    // a bound after the seeded rows leaves nothing behind.
    let report: fq_runtime::views::CostReport = serde_json::from_value(
        invoke(&client, cost_summary(), json!({ "since": "2099-01-01" }))
            .await
            .expect("report"),
    )
    .expect("the declared output shape");
    assert!(report.agents.is_empty(), "got: {report:?}");
}

/// The composite answers, and it answers about the daemon that served
/// it — its own worker row is in the count, which is the sense in
/// which this report needs the thing it reports on.
#[tokio::test]
async fn control_doctor_answers_about_the_daemon_that_serves_it() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    // Everything below asserts about *this* report — the one whose
    // roster satisfied the wait. Taking a fresh one instead would leave
    // a window in which a transient `consumer.info()` failure turns an
    // entry into `{"error": …}`, which carries no `active` key and
    // would fail the consumer assertion on a daemon that is in fact
    // healthy.
    let report = wait_for_durables(
        &client,
        control_doctor,
        &EXPECTED_DURABLES,
        doctor_consumers,
    )
    .await;

    assert!(
        report["workers"]["alive"].as_i64().expect("alive count") >= 1,
        "the serving daemon registers itself, so it is in its own roster: {report}"
    );
    // The sections are all present, including the ones that are empty
    // in a healthy fixture — a report that omitted them would read as
    // "not checked" rather than "nothing found".
    for section in [
        "workers",
        "executions",
        "ambiguous",
        "failures",
        "dead_letters",
        "consumers",
    ] {
        assert!(!report[section].is_null(), "missing {section}: {report}");
    }
    // The Phase 1 exit criterion, verbatim: "`fq doctor` reports every
    // consumer" (#549). Against a live daemon, not a fixture — the
    // probe is a JetStream read only the serving process can make.
    let consumers: Vec<&str> = report["consumers"]
        .as_array()
        .expect("consumers")
        .iter()
        .map(|c| c["active"]["name"].as_str().expect("a named consumer"))
        .collect();
    assert_eq!(
        consumers, EXPECTED_DURABLES,
        "every durable this daemon runs is reported: {report}"
    );
    assert_eq!(report["dead_letters"]["exhausted_triggers"], 0);
    assert_eq!(
        report["failures"].as_array().expect("failures").len(),
        0,
        "nothing failed in this fixture: {report}"
    );
}

/// The machinery report answers with things only a running daemon
/// has: which build it is, the JetStream probe over the connection it
/// holds, and its own live registry. Every section is present even
/// when empty — a report that omitted them would read as "not
/// checked" rather than "nothing found".
#[tokio::test]
async fn control_status_answers_with_what_only_a_running_daemon_has() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;
    let client =
        fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &daemon.admin_token)
            .await
            .expect("connect edge");

    // Waiting on `control.doctor` here would prove a different report
    // ready than the one asserted about; this polls `control.status`
    // itself, over the same roster predicate, and asserts about the
    // report that satisfied it.
    let report = wait_for_durables(
        &client,
        control_status,
        &EXPECTED_DURABLES,
        status_consumers,
    )
    .await;

    for section in [
        "version",
        "streams",
        "registry",
        "projection_rows",
        "recovery",
    ] {
        assert!(!report[section].is_null(), "missing {section}: {report}");
    }
    assert!(
        report["version"]
            .as_str()
            .expect("a version string")
            .contains('+'),
        "the build is semver plus the commit it was built from: {report}"
    );
    // The probe reached the daemon's own streams — the client never
    // connects to the broker, so this could not be here otherwise.
    let streams = report["streams"].as_array().expect("streams");
    assert_eq!(
        streams.len(),
        4,
        "all four core streams are probed — events, triggers, advisories, \
         maintenance: {report}"
    );
    // Every durable this daemon expects, named, and none of them stuck
    // on a freshly-started fixture (#549). This used to be one
    // "primary" consumer per stream, which is how a wedged coordination
    // or advisory consumer stayed invisible to `control.status`.
    let consumers: Vec<&str> = streams
        .iter()
        .flat_map(|s| {
            s["available"]["consumers"]
                .as_array()
                .expect("a live daemon's streams carry their durable consumers")
        })
        .map(|c| c["active"]["name"].as_str().expect("a named consumer"))
        .collect();
    assert_eq!(
        consumers, EXPECTED_DURABLES,
        "this fixture configures no summariser, so it is not expected; \
         maintenance is on by default, so it is: {report}"
    );
    assert!(
        streams
            .iter()
            .flat_map(|s| s["available"]["consumers"].as_array().expect("consumers"))
            .all(|c| c["active"]["stuck"] == serde_json::json!(false)),
        "nothing is wedged on a daemon that just started: {report}"
    );
    // This fixture's agents directory is empty, and an empty registry
    // is a zero rather than an omission.
    assert_eq!(report["registry"]["agents"], 0);
    assert_eq!(
        report["registry"]["load_errors"]
            .as_array()
            .expect("load errors")
            .len(),
        0
    );
    // The projection holds the seeded cost events, read by the daemon
    // that owns the store.
    assert!(
        report["projection_rows"].as_i64().expect("row count") >= 2,
        "the seeded events are folded: {report}"
    );
}

/// **The point of putting an aggregate on the surface.** A report's
/// authority is Read on its own scope and never on its inputs, so
/// spend is grantable without granting the event log it is computed
/// from — and `control.doctor`, being a different scope, is not
/// carried along with it.
///
/// Both directions are asserted, because a boundary that only lets
/// things through is not a boundary.
#[tokio::test]
async fn a_report_is_a_privilege_boundary_over_its_scope_not_its_inputs() {
    let server = fq_test_support::NatsServer::start();
    let daemon = start_daemon(&server).await;

    let cost_only = fq_edge::attenuate(
        &daemon.admin_token,
        &[("read".to_string(), "cost".to_string())],
    )
    .expect("attenuate to cost");
    let control_only = fq_edge::attenuate(
        &daemon.admin_token,
        &[("read".to_string(), "control".to_string())],
    )
    .expect("attenuate to control");

    let accountant = fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &cost_only)
        .await
        .expect("connect edge");
    invoke(&accountant, cost_summary(), json!({}))
        .await
        .expect("read:cost reads fleet spend");
    let denied = invoke(&accountant, OpId::List(Domain::Event), json!({}))
        .await
        .expect_err("read:cost must not carry the event log the spend is computed from");
    assert!(
        matches!(denied, fq_edge::wire::WireError::Denied { .. }),
        "got: {denied:?}"
    );
    let denied = invoke(&accountant, control_doctor(), json!({}))
        .await
        .expect_err("read:cost must not carry another domain's report");
    assert!(
        matches!(denied, fq_edge::wire::WireError::Denied { .. }),
        "got: {denied:?}"
    );

    let operator = fq_edge::EdgeClient::connect(&daemon.addr, daemon.fingerprint, &control_only)
        .await
        .expect("connect edge");
    invoke(&operator, control_doctor(), json!({}))
        .await
        .expect("read:control reads the health composite");
    let denied = invoke(&operator, cost_summary(), json!({}))
        .await
        .expect_err("read:control must not carry spend");
    assert!(
        matches!(denied, fq_edge::wire::WireError::Denied { .. }),
        "got: {denied:?}"
    );
}
