//! The trigger-to-invocation link as an operator reads it: the judgement
//! on its own, one test per row of its table, then both reads against
//! real stores.

use super::*;
use crate::bus::{DEFAULT_TRIGGER_MAX_AGE, TRIGGER_REDELIVERY_WINDOW, TRIGGER_RETRY_BACKOFF};
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::store::{
    ControlPlaneStore, InvocationArchiveRow, OwnerStatus, TriggerKey,
};
use crate::db::RuntimeDbPaths;
use crate::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS;
use crate::worker::store::{InvocationStateRow, WorkerStore};

const THIS_DAEMON: &str = "daemon-now";
const EXITED: &str = "daemon-before";
const STREAM: &str = "fq-triggers";
/// The incarnation of the stream this daemon consumes.
const EPOCH: i64 = 1_767_323_045_000_000_000;
/// An incarnation the stream had before it was recreated.
const OLD_EPOCH: i64 = 1_767_000_000_000_000_000;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// The trigger-claim retention the daemon passes by default: thirty days.
const HORIZON_MS: i64 = 30 * DAY_MS;
/// A clock far enough from zero that "sixty days ago" is still positive.
const NOW_MS: i64 = 1_000 * DAY_MS;

fn window_ms() -> i64 {
    TRIGGER_REDELIVERY_WINDOW.as_millis() as i64
}

fn max_age_ms() -> i64 {
    DEFAULT_TRIGGER_MAX_AGE.as_millis() as i64
}

fn grace_ms() -> i64 {
    CLAIM_MARK_GRACE.as_millis() as i64
}

/// A `claimed` row on the current incarnation.
fn claim(seq: u64, claimant: &str, invocation_id: &str, claimed_at: i64) -> TriggerClaimRow {
    TriggerClaimRow {
        stream: STREAM.into(),
        stream_epoch: EPOCH,
        stream_seq: seq,
        claimant: claimant.into(),
        durably_started: false,
        invocation_id: Some(invocation_id.into()),
        claimed_at,
    }
}

fn on_old_stream(mut row: TriggerClaimRow) -> TriggerClaimRow {
    row.stream_epoch = OLD_EPOCH;
    row
}

fn without_id(mut row: TriggerClaimRow) -> TriggerClaimRow {
    row.invocation_id = None;
    row
}

/// A daemon that has been dispatching for `for_ms`, judging with the
/// bounds the daemon passes.
fn judgement(for_ms: i64) -> ClaimJudgement<'static> {
    ClaimJudgement {
        claimant: THIS_DAEMON,
        dispatching_since_ms: NOW_MS - for_ms,
        redelivery_window_ms: window_ms(),
        stream_epoch: EPOCH,
        stream_max_age_ms: max_age_ms(),
        horizon_ms: Some(HORIZON_MS),
        now_ms: NOW_MS,
    }
}

fn wal_record(status: &str, started_at_ms: i64) -> Option<StartRecord> {
    Some(StartRecord {
        status: status.into(),
        started_at_ms: Some(started_at_ms),
    })
}

fn coordination_record(status: &str) -> Option<StartRecord> {
    Some(StartRecord {
        status: status.into(),
        started_at_ms: None,
    })
}

fn finding(
    row: TriggerClaimRow,
    record: Option<StartRecord>,
    j: &ClaimJudgement<'_>,
) -> Option<(StrandedClaimFinding, ClaimedMessage)> {
    judge(row, record, j).map(|s| (s.finding, s.message))
}

// ---- The bounds ----

/// The window is the retry ladder end to end, so it can never be shorter
/// than the longest single wait a redelivery can take — which is what
/// keeps an adoptable claim from being called abandoned.
#[test]
fn the_redelivery_window_is_the_whole_retry_ladder() {
    let sum: Duration = TRIGGER_RETRY_BACKOFF.iter().sum();
    assert_eq!(TRIGGER_REDELIVERY_WINDOW, sum);
    assert!(TRIGGER_REDELIVERY_WINDOW >= *TRIGGER_RETRY_BACKOFF.iter().max().unwrap());
    assert_eq!(TRIGGER_REDELIVERY_WINDOW, Duration::from_secs(19 * 60 + 30));
}

/// Deliverable means the current incarnation and claimed more recently
/// than the stream keeps messages; the boundary itself is aged out.
#[test]
fn a_message_is_deliverable_only_on_this_stream_and_inside_its_max_age() {
    let j = judgement(0);
    let at = |age| claim(1, EXITED, "inv", NOW_MS - age);
    assert_eq!(
        j.message(&at(max_age_ms() - 1)),
        ClaimedMessage::Deliverable
    );
    assert_eq!(j.message(&at(max_age_ms())), ClaimedMessage::AgedOut);
    assert_eq!(
        j.message(&on_old_stream(at(HOUR_MS))),
        ClaimedMessage::EarlierIncarnation
    );
}

#[test]
fn only_a_delivery_that_never_started_and_an_unmarked_run_are_issues() {
    assert!(StrandedClaimFinding::NeverStarted.is_issue());
    assert!(StrandedClaimFinding::StartedUnmarked.is_issue());
    assert!(!StrandedClaimFinding::AwaitingRedelivery.is_issue());
    assert!(!StrandedClaimFinding::StartUnknown.is_issue());
}

// ---- The table, one test per row ----

/// Row 1. The dispatcher marks the claim moments after the first WAL
/// write; a report taken in between sees a run on record and a claim that
/// does not say so. Inside the grace that is a healthy start.
#[test]
fn a_run_on_record_inside_the_mark_grace_is_not_listed() {
    for holder in [THIS_DAEMON, EXITED] {
        let starting = claim(4, holder, "inv-starting", NOW_MS - 2 * HOUR_MS);
        let record = wal_record("in_flight", NOW_MS - grace_ms() + 1);
        assert_eq!(judge(starting, record, &judgement(window_ms())), None);
    }
}

/// Row 2, from a WAL row past the grace and from a coordination row,
/// which needs none: it is written once a run is archived, found
/// ambiguous or dropped, long after its claim should have been marked.
#[test]
fn a_run_on_record_with_a_deliverable_message_is_started_unmarked() {
    for (holder, expected) in [
        (THIS_DAEMON, ClaimHolder::ThisDaemon),
        (EXITED, ClaimHolder::ExitedProcess),
    ] {
        for (record, status) in [
            (wal_record("in_flight", NOW_MS - grace_ms()), "in_flight"),
            (coordination_record("failed"), "failed"),
        ] {
            let unmarked = claim(5, holder, "inv-running", NOW_MS - 2 * HOUR_MS);
            let stranded = judge(unmarked, record, &judgement(0)).expect("listed");
            assert_eq!(stranded.finding, StrandedClaimFinding::StartedUnmarked);
            assert_eq!(stranded.message, ClaimedMessage::Deliverable);
            assert_eq!(stranded.holder, expected);
            assert_eq!(stranded.invocation_status.as_deref(), Some(status));
        }
    }
}

/// Row 3. No redelivery of a gone message can follow, so an unmarked
/// claim on one can no longer drop a redelivery or start a second run.
#[test]
fn a_run_on_record_with_a_gone_message_is_not_listed() {
    for holder in [THIS_DAEMON, EXITED] {
        let earlier = on_old_stream(claim(6, holder, "inv-ran", NOW_MS - 2 * HOUR_MS));
        assert_eq!(
            judge(earlier, coordination_record("completed"), &judgement(0)),
            None
        );
        let aged = claim(7, holder, "inv-ran", NOW_MS - max_age_ms());
        assert_eq!(
            judge(aged, coordination_record("completed"), &judgement(0)),
            None
        );
    }
}

/// Row 4. A trigger this daemon holds behind a paused model or a full
/// agent can wait for hours, and nothing on the row says it is not still
/// going to start: never listed, on either incarnation, with or without
/// an id, however old.
#[test]
fn no_run_on_record_held_by_this_daemon_is_never_listed() {
    for age in [
        0,
        grace_ms(),
        window_ms(),
        6 * HOUR_MS,
        2 * DAY_MS,
        60 * DAY_MS,
    ] {
        let held = claim(8, THIS_DAEMON, "inv-held", NOW_MS - age);
        for row in [held.clone(), on_old_stream(held.clone()), without_id(held)] {
            assert_eq!(
                judge(row, None, &judgement(24 * HOUR_MS)),
                None,
                "a held trigger {age} ms old must not be reported"
            );
        }
    }
}

/// Row 5. The report stops listing a dead claim at the age the retention
/// sweep deletes it — whatever else is true of it. With claim retention
/// off, it is listed for as long as it exists.
#[test]
fn no_run_on_record_left_by_an_exited_process_past_the_horizon_is_not_listed() {
    let old = claim(9, EXITED, "inv-lost", NOW_MS - HORIZON_MS);
    for row in [
        old.clone(),
        on_old_stream(old.clone()),
        without_id(old.clone()),
    ] {
        assert_eq!(judge(row, None, &judgement(24 * HOUR_MS)), None);
    }
    let kept = ClaimJudgement {
        horizon_ms: None,
        ..judgement(24 * HOUR_MS)
    };
    assert_eq!(
        finding(old, None, &kept),
        Some((StrandedClaimFinding::NeverStarted, ClaimedMessage::AgedOut))
    );
}

/// Row 6. A claim from before claims named their run cannot be looked up,
/// so the report says it cannot tell and does not call it an issue.
#[test]
fn no_run_on_record_and_no_invocation_id_is_start_unknown() {
    for age in [HOUR_MS, 2 * DAY_MS] {
        let row = without_id(claim(10, EXITED, "unused", NOW_MS - age));
        let stranded = judge(row, None, &judgement(24 * HOUR_MS)).expect("listed");
        assert_eq!(stranded.finding, StrandedClaimFinding::StartUnknown);
        assert!(!stranded.finding.is_issue());
        assert_eq!(stranded.invocation_id, None);
        assert_eq!(stranded.invocation_status, None);
    }
}

/// Row 7. Deliverable, and this daemon has not yet run for the window in
/// which the broker brings an unacked delivery back.
#[test]
fn a_deliverable_claim_an_exited_process_left_awaits_redelivery_inside_the_window() {
    let left = claim(11, EXITED, "inv-left", NOW_MS - 6 * HOUR_MS);
    let stranded = judge(left, None, &judgement(window_ms() - 1)).expect("listed");
    assert_eq!(stranded.finding, StrandedClaimFinding::AwaitingRedelivery);
    assert_eq!(stranded.message, ClaimedMessage::Deliverable);
    assert!(!stranded.finding.is_issue());
    assert_eq!(stranded.holder, ClaimHolder::ExitedProcess);
}

/// Row 8. Deliverable, and not back although the window has passed.
#[test]
fn a_deliverable_claim_an_exited_process_left_never_started_past_the_window() {
    let left = claim(12, EXITED, "inv-lost", NOW_MS - 6 * HOUR_MS);
    let stranded = judge(left, None, &judgement(window_ms())).expect("listed");
    assert_eq!(stranded.finding, StrandedClaimFinding::NeverStarted);
    assert_eq!(stranded.message, ClaimedMessage::Deliverable);
    assert_eq!(stranded.delivery.stream_seq, 12);
    assert_eq!(stranded.delivery.stream_epoch, EPOCH.to_string());
    assert_eq!(stranded.delivery.state, TriggerClaimState::Claimed);
    assert_eq!(stranded.invocation_id.as_deref(), Some("inv-lost"));
}

/// Row 9. A gone message is never coming back, so the window does not
/// matter: a freshly started daemon calls it never started at once.
#[test]
fn a_gone_claim_an_exited_process_left_never_started_whatever_the_window() {
    let j = judgement(0);
    let earlier = on_old_stream(claim(13, EXITED, "inv-a", NOW_MS - HOUR_MS));
    assert_eq!(
        finding(earlier, None, &j),
        Some((
            StrandedClaimFinding::NeverStarted,
            ClaimedMessage::EarlierIncarnation
        ))
    );
    let aged = claim(14, EXITED, "inv-b", NOW_MS - 2 * DAY_MS);
    assert_eq!(
        finding(aged, None, &j),
        Some((StrandedClaimFinding::NeverStarted, ClaimedMessage::AgedOut))
    );
}

// ---- Against the stores ----

struct Seed {
    _dir: tempfile::TempDir,
    paths: RuntimeDbPaths,
    cp: ControlPlaneStore,
    worker: WorkerStore,
    projection: ProjectionStore,
}

impl Seed {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimeDbPaths::under(dir.path());
        let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let worker = WorkerStore::open(&paths.worker).await.unwrap();
        let projection = ProjectionStore::open(&paths.projection).await.unwrap();
        Self {
            _dir: dir,
            paths,
            cp,
            worker,
            projection,
        }
    }

    fn key(seq: u64) -> TriggerKey<'static> {
        TriggerKey {
            stream: STREAM,
            stream_epoch: EPOCH,
            stream_seq: seq,
        }
    }

    async fn claimed(&self, seq: u64, claimant: &str, invocation_id: &str, at: i64) {
        self.cp
            .claim_trigger(Self::key(seq), claimant, invocation_id, at)
            .await
            .unwrap();
    }

    async fn started(&self, seq: u64, claimant: &str, invocation_id: &str, at: i64) {
        self.claimed(seq, claimant, invocation_id, at).await;
        self.cp
            .mark_trigger_started(Self::key(seq), invocation_id)
            .await
            .unwrap();
    }

    /// A claim row with no invocation id, as a build that did not name it
    /// at claim time wrote it — through a raw connection, because no
    /// store method can write one any more.
    async fn claimed_without_id(&self, seq: u64, claimant: &str, at: i64) {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.paths.control_plane),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at) \
             VALUES (?, ?, ?, ?, 'claimed', NULL, ?)",
        )
        .bind(STREAM)
        .bind(EPOCH)
        .bind(seq as i64)
        .bind(claimant)
        .bind(at)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    /// The run's WAL row: in flight, or terminal with `phase`.
    async fn wal(&self, invocation_id: &str, started_at: i64, terminal: Option<&str>) {
        self.worker
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: invocation_id.into(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: terminal.unwrap_or("reducing").into(),
                state_blob: vec![],
                step_index: 1,
                started_at,
                updated_at: started_at,
                terminal_at: terminal.map(|_| started_at + 1_000),
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: Some("subject".into()),
                trigger_subject: None,
                trigger_payload: None,
                terminal_outcome: None,
                terminal_published_at: None,
            })
            .await
            .unwrap();
    }

    /// What the coordination consumer leaves once a run is archived and
    /// its WAL row deleted: the archive row and a terminal owner row.
    async fn archived(&self, invocation_id: &str, at: i64) {
        self.cp
            .insert_archive(&InvocationArchiveRow {
                invocation_id: invocation_id.into(),
                agent_id: "agent-a".into(),
                final_phase: "completed".into(),
                final_state_blob: vec![],
                started_at: at,
                terminal_at: at + 1_000,
                archived_at: at + 2_000,
            })
            .await
            .unwrap();
        self.cp
            .upsert_invocation_ownership(
                invocation_id,
                "worker",
                at + 2_000,
                OwnerStatus::Completed,
            )
            .await
            .unwrap();
    }

    /// A `triggered` event for the run, which is all a refused run leaves.
    async fn triggered(&self, invocation_id: uuid::Uuid) {
        use crate::events::{Event, EventPayload, TriggerSource, TriggeredPayload};
        let agent = crate::agent::Agent::builder()
            .id("agent-a")
            .model("claude-haiku")
            .system_prompt("fixture")
            .build()
            .unwrap();
        let event = Event::new(
            crate::agent::AgentId::new("agent-a").unwrap(),
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_id: None,
                trigger_source: TriggerSource::Subject,
                trigger_subject: None,
                trigger_payload: serde_json::Value::Null,
                config_snapshot: agent.to_snapshot(),
            }),
        );
        self.projection.insert_event(&event, None).await.unwrap();
    }

    async fn views(&self) -> Views {
        Views::open(&self.paths).await.unwrap()
    }
}

async fn detail(views: &Views, invocation_id: &str) -> crate::views::InvocationDetailView {
    find(views, invocation_id)
        .await
        .unwrap_or_else(|| panic!("{invocation_id} has a trace"))
}

async fn find(views: &Views, invocation_id: &str) -> Option<crate::views::InvocationDetailView> {
    views
        .invocation(
            invocation_id,
            NOW_MS,
            30_000,
            DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
        )
        .await
        .unwrap()
}

/// A run a trigger started carries the delivery that started it; a run
/// started any other way carries none; and a run that finished and was
/// archived, its WAL row gone, still carries it — the claim is the one
/// record of the link that outlives both.
#[tokio::test]
async fn the_invocation_view_names_the_delivery_that_started_it() {
    let seed = Seed::new().await;
    seed.started(41, THIS_DAEMON, "inv-live", NOW_MS - 5_000)
        .await;
    seed.wal("inv-live", NOW_MS - 4_000, None).await;
    seed.wal("inv-direct", NOW_MS - 4_000, None).await;
    seed.started(42, EXITED, "inv-archived", NOW_MS - HOUR_MS)
        .await;
    seed.archived("inv-archived", NOW_MS - HOUR_MS).await;
    let views = seed.views().await;

    let live = detail(&views, "inv-live").await;
    assert_eq!(
        live.trigger,
        Some(TriggerDeliveryView {
            stream: STREAM.into(),
            stream_epoch: EPOCH.to_string(),
            stream_seq: 41,
            state: TriggerClaimState::DurablyStarted,
            claimant: THIS_DAEMON.into(),
            claimed_at_ms: NOW_MS - 5_000,
        })
    );

    assert_eq!(detail(&views, "inv-direct").await.trigger, None);

    let archived = detail(&views, "inv-archived").await;
    assert!(archived.live.is_none(), "no WAL row is left");
    assert!(archived.archive.is_some());
    let trigger = archived.trigger.expect("the claim outlives the run");
    assert_eq!(trigger.stream_seq, 42);
    assert_eq!(trigger.claimant, EXITED);
    assert_eq!(trigger.state, TriggerClaimState::DurablyStarted);
}

/// The archive is swept on `retention_days`; the coordination row that
/// makes the run findable is not swept at all, and the claim goes on a
/// window of its own. While the claim is kept, the run shows its trigger
/// without its archive.
#[tokio::test]
async fn a_run_whose_archive_was_swept_still_shows_its_trigger() {
    let seed = Seed::new().await;
    seed.started(43, EXITED, "inv-swept", NOW_MS - 40 * DAY_MS)
        .await;
    seed.archived("inv-swept", NOW_MS - 40 * DAY_MS).await;
    assert_eq!(seed.cp.sweep_archive(NOW_MS - HORIZON_MS).await.unwrap(), 1);
    let views = seed.views().await;

    let swept = detail(&views, "inv-swept").await;
    assert!(swept.archive.is_none(), "the archive row is gone");
    assert!(swept.owner.is_some(), "the coordination row is not");
    assert_eq!(swept.trigger.expect("and nor is the claim").stream_seq, 43);
}

/// A run that ended before its first durable write gives its claim back,
/// yet its events are already published under the claim's id: the view
/// finds the run and finds no claim for it.
#[tokio::test]
async fn a_run_with_events_and_no_claim_carries_no_trigger() {
    let seed = Seed::new().await;
    let refused = uuid::Uuid::now_v7();
    seed.triggered(refused).await;
    let views = seed.views().await;

    let shown = detail(&views, &refused.to_string()).await;
    assert_eq!(shown.agent_id.as_deref(), Some("agent-a"));
    assert_eq!(shown.trigger, None);
}

/// A claim reserves an id; it does not make an invocation exist.
#[tokio::test]
async fn an_id_only_a_claim_names_is_not_an_invocation() {
    let seed = Seed::new().await;
    seed.claimed(44, EXITED, "inv-reserved", NOW_MS - HOUR_MS)
        .await;
    let views = seed.views().await;
    assert!(find(&views, "inv-reserved").await.is_none());
}

/// An adoption writes the adopter's own id and the time of the adoption
/// over the row: the adopter's run shows the delivery under this daemon,
/// and the id the exited process reserved names none.
#[tokio::test]
async fn an_adopted_claim_names_the_adopters_run_and_not_the_old_id() {
    let seed = Seed::new().await;
    seed.claimed(45, EXITED, "inv-old", NOW_MS - 2 * HOUR_MS)
        .await;
    assert!(
        seed.cp
            .take_over_trigger_claim(
                Seed::key(45),
                EXITED,
                THIS_DAEMON,
                "inv-new",
                NOW_MS - HOUR_MS
            )
            .await
            .unwrap()
    );
    seed.wal("inv-old", NOW_MS - 2 * HOUR_MS, Some("failed"))
        .await;
    seed.wal("inv-new", NOW_MS - HOUR_MS, None).await;
    let views = seed.views().await;

    let adopted = detail(&views, "inv-new").await.trigger.expect("adopted");
    assert_eq!(adopted.claimant, THIS_DAEMON);
    assert_eq!(adopted.claimed_at_ms, NOW_MS - HOUR_MS);
    assert_eq!(adopted.stream_seq, 45);
    assert_eq!(detail(&views, "inv-old").await.trigger, None);
}

/// The claim names the invocation from claim time, so a run whose start
/// was never recorded on it shows the claim as it stands: `claimed`.
#[tokio::test]
async fn an_unmarked_claim_shows_on_its_run_as_claimed() {
    let seed = Seed::new().await;
    seed.claimed(7, THIS_DAEMON, "inv-unmarked", NOW_MS - HOUR_MS)
        .await;
    seed.wal("inv-unmarked", NOW_MS - HOUR_MS, None).await;
    let views = seed.views().await;

    let trigger = detail(&views, "inv-unmarked")
        .await
        .trigger
        .expect("named from claim time");
    assert_eq!(trigger.state, TriggerClaimState::Claimed);
    assert_eq!(trigger.stream_seq, 7);
}

/// A table of started triggers and one this daemon is holding is
/// healthy, and says nothing.
#[tokio::test]
async fn a_healthy_table_and_a_held_trigger_report_nothing() {
    let seed = Seed::new().await;
    for seq in 1..=5 {
        let id = format!("inv-{seq}");
        seed.started(seq, EXITED, &id, NOW_MS - 2 * HOUR_MS).await;
        seed.archived(&id, NOW_MS - 2 * HOUR_MS).await;
    }
    seed.claimed(6, THIS_DAEMON, "inv-held", NOW_MS - 3 * HOUR_MS)
        .await;
    let views = seed.views().await;

    let stranded = views
        .stranded_trigger_claims(&judgement(24 * HOUR_MS))
        .await
        .unwrap();
    assert!(stranded.is_empty(), "got {stranded:?}");
}

/// Every source of a run's record at once, against the stores the daemon
/// reads, in the order the report lists them (oldest claim first): a
/// coordination row, a terminal WAL row, an in-flight one, none at all,
/// and a claim with no id.
#[tokio::test]
async fn stranded_claims_are_judged_from_the_stores() {
    let seed = Seed::new().await;
    // Started and marked: never listed.
    seed.started(1, EXITED, "inv-ok", NOW_MS - 9 * HOUR_MS)
        .await;
    // Left by an exited process, nothing recorded of its run.
    seed.claimed(2, EXITED, "inv-never", NOW_MS - 8 * HOUR_MS)
        .await;
    // Left by an exited process whose run finished and was archived: the
    // WAL row is gone, the coordination row says so.
    seed.claimed(3, EXITED, "inv-finished", NOW_MS - 7 * HOUR_MS)
        .await;
    seed.archived("inv-finished", NOW_MS - 7 * HOUR_MS).await;
    // Left by an exited process whose run reached a terminal row that has
    // not been archived yet.
    seed.claimed(4, EXITED, "inv-terminal", NOW_MS - 6 * HOUR_MS)
        .await;
    seed.wal("inv-terminal", NOW_MS - 6 * HOUR_MS, Some("failed"))
        .await;
    // This daemon's, running for an hour with its claim never marked.
    seed.claimed(5, THIS_DAEMON, "inv-running", NOW_MS - 5 * HOUR_MS)
        .await;
    seed.wal("inv-running", NOW_MS - HOUR_MS, None).await;
    // This daemon's, held for hours, never started.
    seed.claimed(6, THIS_DAEMON, "inv-held", NOW_MS - 4 * HOUR_MS)
        .await;
    // Written before claims named their run.
    seed.claimed_without_id(7, EXITED, NOW_MS - 3 * HOUR_MS)
        .await;
    let views = seed.views().await;

    let stranded = views
        .stranded_trigger_claims(&judgement(window_ms()))
        .await
        .unwrap();
    let got: Vec<(u64, ClaimHolder, StrandedClaimFinding, Option<&str>)> = stranded
        .iter()
        .map(|s| {
            (
                s.delivery.stream_seq,
                s.holder,
                s.finding,
                s.invocation_status.as_deref(),
            )
        })
        .collect();
    use ClaimHolder::{ExitedProcess, ThisDaemon};
    use StrandedClaimFinding::{NeverStarted, StartUnknown, StartedUnmarked};
    assert_eq!(
        got,
        vec![
            (2, ExitedProcess, NeverStarted, None),
            (3, ExitedProcess, StartedUnmarked, Some("completed")),
            (4, ExitedProcess, StartedUnmarked, Some("failed")),
            (5, ThisDaemon, StartedUnmarked, Some("in_flight")),
            (7, ExitedProcess, StartUnknown, None),
        ]
    );
    assert_eq!(stranded[4].invocation_id, None, "a NULL id reads as none");

    // The same table, asked by a daemon that has only just started: the
    // claim with nothing recorded is still due its redelivery, and is
    // listed without being an issue.
    let early = views.stranded_trigger_claims(&judgement(0)).await.unwrap();
    assert_eq!(early[0].delivery.stream_seq, 2);
    assert_eq!(early[0].finding, StrandedClaimFinding::AwaitingRedelivery);
}

/// A claim with no invocation id names no run, so no invocation's view
/// can pick it up, however its run is looked for.
#[tokio::test]
async fn a_claim_with_no_invocation_id_names_no_run() {
    let seed = Seed::new().await;
    seed.claimed_without_id(8, EXITED, NOW_MS - HOUR_MS).await;
    seed.wal("inv-other", NOW_MS - HOUR_MS, None).await;
    let views = seed.views().await;
    assert_eq!(detail(&views, "inv-other").await.trigger, None);
    assert_eq!(views.invocation_trigger("").await.unwrap(), None);
}

/// A run's record moves from the WAL to the coordination table when it is
/// archived. A claim read before the archive and a WAL read after it would
/// find neither; the second read of the coordination row finds the run.
#[tokio::test]
async fn a_run_archived_between_the_two_reads_is_still_found() {
    let seed = Seed::new().await;
    seed.claimed(9, EXITED, "inv-moved", NOW_MS - HOUR_MS).await;
    let read_before_the_archive = seed.cp.unsettled_trigger_claims().await.unwrap();
    seed.archived("inv-moved", NOW_MS - HOUR_MS).await;
    let views = seed.views().await;

    let [unsettled] = read_before_the_archive.as_slice() else {
        panic!("one claimed row");
    };
    assert_eq!(unsettled.owner_status, None, "read before the archive");
    assert_eq!(
        views.start_record(unsettled).await.unwrap(),
        coordination_record("completed")
    );
}
