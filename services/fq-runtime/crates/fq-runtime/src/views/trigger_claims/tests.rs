//! The trigger-to-invocation link as an operator reads it: the judgement
//! on its own, then both reads against real stores.

use super::*;
use crate::control_plane::store::{
    ControlPlaneStore, InvocationArchiveRow, OwnerStatus, TriggerKey,
};
use crate::control_plane::projection::ProjectionStore;
use crate::db::RuntimeDbPaths;
use crate::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS;
use crate::worker::store::{InvocationStateRow, WorkerStore};

const THIS_DAEMON: &str = "daemon-now";
const EXITED: &str = "daemon-before";
const STREAM: &str = "fq-triggers";
const EPOCH: i64 = 1_767_323_045_000_000_000;

const HOUR_MS: i64 = 3_600_000;
/// A clock far enough from zero that "six hours ago" is still positive.
const NOW_MS: i64 = 1_000 * HOUR_MS;

fn window_ms() -> i64 {
    TRIGGER_REDELIVERY_WINDOW.as_millis() as i64
}

fn grace_ms() -> i64 {
    CLAIM_MARK_GRACE.as_millis() as i64
}

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

/// A daemon that has been dispatching for `for_ms`.
fn judgement(for_ms: i64) -> ClaimJudgement<'static> {
    ClaimJudgement {
        claimant: THIS_DAEMON,
        dispatching_since_ms: NOW_MS - for_ms,
        now_ms: NOW_MS,
    }
}

fn wal_record(status: &str, started_at_ms: i64) -> Option<StartRecord> {
    Some(StartRecord {
        status: status.into(),
        started_at_ms: Some(started_at_ms),
    })
}

// ---- The judgement, pure ----

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

/// A trigger this daemon holds behind a paused model or a full agent can
/// wait for hours, and nothing on the row says it is not still going to
/// start. Six hours old, still not reported.
#[test]
fn a_claim_this_daemon_holds_with_no_start_is_never_reported_however_old() {
    for age in [0, grace_ms(), window_ms(), 6 * HOUR_MS] {
        let held = claim(1, THIS_DAEMON, "inv-held", NOW_MS - age);
        assert_eq!(
            judge(held, None, &judgement(24 * HOUR_MS)),
            None,
            "a held trigger {age} ms old must not be reported"
        );
    }
}

#[test]
fn a_claim_an_exited_process_left_awaits_redelivery_inside_the_window() {
    let left = claim(2, EXITED, "inv-left", NOW_MS - 6 * HOUR_MS);
    let stranded = judge(left, None, &judgement(window_ms() - 1)).expect("listed");
    assert_eq!(stranded.finding, StrandedClaimFinding::AwaitingRedelivery);
    assert!(!stranded.finding.is_issue());
    assert_eq!(stranded.holder, ClaimHolder::ExitedProcess);
    assert_eq!(stranded.invocation_status, None);
}

#[test]
fn a_claim_an_exited_process_left_never_started_once_the_window_has_passed() {
    let left = claim(3, EXITED, "inv-lost", NOW_MS - 6 * HOUR_MS);
    let stranded = judge(left, None, &judgement(window_ms())).expect("listed");
    assert_eq!(stranded.finding, StrandedClaimFinding::NeverStarted);
    assert!(stranded.finding.is_issue());
    assert_eq!(stranded.delivery.stream_seq, 3);
    assert_eq!(stranded.delivery.stream_epoch, EPOCH);
    assert_eq!(stranded.delivery.state, TriggerClaimState::Claimed);
    assert_eq!(stranded.invocation_id.as_deref(), Some("inv-lost"));
}

/// The dispatcher marks the claim two awaits after the first WAL write,
/// so a report taken in between sees a start on record and a claim that
/// does not say so. Inside the grace that is a healthy start.
#[test]
fn a_start_inside_the_mark_grace_is_not_reported() {
    for holder in [THIS_DAEMON, EXITED] {
        let starting = claim(4, holder, "inv-starting", NOW_MS - 2 * HOUR_MS);
        let record = wal_record("in_flight", NOW_MS - grace_ms() + 1);
        assert_eq!(judge(starting, record, &judgement(window_ms())), None);
    }
}

#[test]
fn a_started_run_whose_claim_was_never_marked_is_reported_for_either_holder() {
    for (holder, expected) in [
        (THIS_DAEMON, ClaimHolder::ThisDaemon),
        (EXITED, ClaimHolder::ExitedProcess),
    ] {
        let unmarked = claim(5, holder, "inv-running", NOW_MS - 2 * HOUR_MS);
        let record = wal_record("in_flight", NOW_MS - grace_ms());
        let stranded = judge(unmarked, record, &judgement(0)).expect("listed");
        assert_eq!(stranded.finding, StrandedClaimFinding::StartedUnmarked);
        assert!(stranded.finding.is_issue());
        assert_eq!(stranded.holder, expected);
        assert_eq!(stranded.invocation_status.as_deref(), Some("in_flight"));
    }
}

/// A coordination row is written once a run is archived, found ambiguous
/// or dropped — long after its claim should have been marked — so it
/// needs no grace, and an exited holder's needs no window.
#[test]
fn a_coordination_record_is_a_start_without_a_grace() {
    let finished = claim(6, EXITED, "inv-finished", NOW_MS);
    let record = Some(StartRecord {
        status: "failed".into(),
        started_at_ms: None,
    });
    let stranded = judge(finished, record, &judgement(0)).expect("listed");
    assert_eq!(stranded.finding, StrandedClaimFinding::StartedUnmarked);
    assert_eq!(stranded.invocation_status.as_deref(), Some("failed"));
}

// ---- Against the stores ----

struct Seed {
    _dir: tempfile::TempDir,
    paths: RuntimeDbPaths,
    cp: ControlPlaneStore,
    worker: WorkerStore,
    _projection: ProjectionStore,
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
            _projection: projection,
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

    async fn wal(&self, invocation_id: &str, started_at: i64, terminal_at: Option<i64>) {
        self.worker
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: invocation_id.into(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: if terminal_at.is_some() {
                    "completed".into()
                } else {
                    "reducing".into()
                },
                state_blob: vec![],
                step_index: 1,
                started_at,
                updated_at: started_at,
                terminal_at,
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
            .upsert_invocation_ownership(invocation_id, "worker", at + 2_000, OwnerStatus::Completed)
            .await
            .unwrap();
    }

    async fn views(&self) -> Views {
        Views::open(&self.paths).await.unwrap()
    }
}

async fn detail(views: &Views, invocation_id: &str) -> crate::views::InvocationDetailView {
    views
        .invocation(
            invocation_id,
            NOW_MS,
            30_000,
            DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
        )
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{invocation_id} has a trace"))
}

/// A run a trigger started carries the delivery that started it; a run
/// started any other way carries none; and a run that finished and was
/// archived, its WAL row gone, still carries it — the claim is the one
/// record of the link that outlives both.
#[tokio::test]
async fn the_invocation_view_names_the_delivery_that_started_it() {
    let seed = Seed::new().await;
    seed.started(41, THIS_DAEMON, "inv-live", NOW_MS - 5_000).await;
    seed.wal("inv-live", NOW_MS - 4_000, None).await;
    seed.wal("inv-direct", NOW_MS - 4_000, None).await;
    seed.started(42, EXITED, "inv-archived", NOW_MS - HOUR_MS).await;
    seed.archived("inv-archived", NOW_MS - HOUR_MS).await;
    let views = seed.views().await;

    let live = detail(&views, "inv-live").await;
    assert_eq!(
        live.trigger,
        Some(TriggerDeliveryView {
            stream: STREAM.into(),
            stream_epoch: EPOCH,
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

/// Every case at once, against the stores the daemon reads, in the order
/// the report lists them (oldest claim first).
#[tokio::test]
async fn stranded_claims_are_judged_from_the_stores() {
    let seed = Seed::new().await;
    // Started and marked: never listed.
    seed.started(1, EXITED, "inv-ok", NOW_MS - 9 * HOUR_MS).await;
    // Left by an exited process, nothing recorded of its run.
    seed.claimed(2, EXITED, "inv-never", NOW_MS - 8 * HOUR_MS)
        .await;
    // Left by an exited process whose run finished and was archived:
    // the WAL row is gone, the coordination row says so.
    seed.claimed(3, EXITED, "inv-finished", NOW_MS - 7 * HOUR_MS)
        .await;
    seed.archived("inv-finished", NOW_MS - 7 * HOUR_MS).await;
    // This daemon's, running for an hour with its claim never marked.
    seed.claimed(4, THIS_DAEMON, "inv-running", NOW_MS - 6 * HOUR_MS)
        .await;
    seed.wal("inv-running", NOW_MS - HOUR_MS, None).await;
    // This daemon's, held for hours, never started.
    seed.claimed(5, THIS_DAEMON, "inv-held", NOW_MS - 5 * HOUR_MS)
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
    assert_eq!(
        got,
        vec![
            (
                2,
                ClaimHolder::ExitedProcess,
                StrandedClaimFinding::NeverStarted,
                None
            ),
            (
                3,
                ClaimHolder::ExitedProcess,
                StrandedClaimFinding::StartedUnmarked,
                Some("completed")
            ),
            (
                4,
                ClaimHolder::ThisDaemon,
                StrandedClaimFinding::StartedUnmarked,
                Some("in_flight")
            ),
        ]
    );

    // The same table, asked by a daemon that has only just started: the
    // claim with nothing recorded is still due its redelivery, and is
    // listed without being an issue.
    let early = views
        .stranded_trigger_claims(&judgement(0))
        .await
        .unwrap();
    assert_eq!(early[0].delivery.stream_seq, 2);
    assert_eq!(early[0].finding, StrandedClaimFinding::AwaitingRedelivery);
}
