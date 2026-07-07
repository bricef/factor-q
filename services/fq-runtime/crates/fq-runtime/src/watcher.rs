//! GitHub issue watcher (issue #6).
//!
//! Polls a GitHub repository's open issues on a slow interval and, for
//! each issue labelled `ready`, drives a two-step label state machine
//! before triggering an agent:
//!
//! 1. **Relabel** the issue `ready` -> `in-progress`.
//! 2. **Then** publish `fq.trigger.<target_agent>` carrying the issue
//!    number as the payload.
//!
//! Relabelling *out of* `ready` before triggering is what makes the
//! watcher fire exactly once per issue: a re-seen issue is no longer
//! `ready`, so edits, re-polls, and watcher restarts cannot
//! double-trigger. As belt-and-suspenders, an issue already carrying
//! `in-progress` is skipped even if it somehow still shows `ready`.
//!
//! A minimal concurrency guard (`max_triggers_per_poll`) bounds how
//! many issues can be picked up in a single poll so a labelling spree
//! cannot spawn an unbounded fleet. The full cost-control layer is a
//! separate, larger concern (see the backlog).
//!
//! GitHub access is behind the [`IssueSource`] seam so the
//! label-transition + dedup logic is unit-testable without touching the
//! network; production wires [`GhCliIssueSource`], which shells out to
//! the `gh` CLI.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::bus::EventBus;
use crate::config::WatcherConfig;

/// A single open GitHub issue, reduced to the fields the watcher's
/// state machine needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    /// Label names currently on the issue.
    #[serde(default)]
    pub labels: Vec<String>,
}

impl Issue {
    fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// Errors the watcher's GitHub seam can surface.
#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("failed to list issues: {0}")]
    ListIssues(String),
    #[error("failed to relabel issue #{number}: {message}")]
    Relabel { number: u64, message: String },
    #[error("failed to parse issue list output: {0}")]
    Parse(String),
}

/// The narrowest seam over GitHub access. The watcher lists open
/// issues and relabels them through this trait so the pure
/// label-transition logic (and the poll loop) can be exercised
/// against an in-memory fake in tests without hitting the network.
#[async_trait]
pub trait IssueSource: Send + Sync {
    /// List the repo's open issues (with their labels).
    async fn list_open_issues(&self) -> Result<Vec<Issue>, WatcherError>;

    /// Relabel an issue: remove `remove_label`, add `add_label`. This
    /// is the idempotency step — it must complete before the trigger
    /// is published so a re-seen issue is no longer `ready`.
    async fn relabel(
        &self,
        number: u64,
        remove_label: &str,
        add_label: &str,
    ) -> Result<(), WatcherError>;
}

/// The action the watcher decided to take for one issue in a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTrigger {
    pub number: u64,
}

/// Pure: given the current open issues and the watcher config, decide
/// which issues to pick up this poll. This is the heart of the label
/// state machine and the concurrency guard, factored out so it can be
/// tested without any I/O.
///
/// An issue is picked up iff it carries the `ready` label AND does not
/// already carry the `in-progress` label (belt-and-suspenders dedup).
/// At most `max_triggers_per_poll` issues are returned, in ascending
/// issue-number order (oldest first — deterministic and fair).
pub fn plan_triggers(issues: &[Issue], config: &WatcherConfig) -> Vec<PlannedTrigger> {
    let mut candidates: Vec<&Issue> = issues
        .iter()
        .filter(|issue| {
            issue.has_label(&config.ready_label) && !issue.has_label(&config.in_progress_label)
        })
        .collect();
    // Oldest issue first — deterministic ordering so the concurrency
    // cap picks a stable, fair subset rather than whatever order the
    // API returned.
    candidates.sort_by_key(|issue| issue.number);
    candidates
        .into_iter()
        .take(config.max_triggers_per_poll)
        .map(|issue| PlannedTrigger {
            number: issue.number,
        })
        .collect()
}

/// Production [`IssueSource`] backed by the `gh` CLI. Reuses the
/// operator's existing GitHub authentication (`gh auth` / `GH_TOKEN`),
/// as the issue permits ("GitHub access via the `gh` CLI is
/// acceptable"). Shells out rather than talking to the REST API
/// directly so there is no new auth surface to configure.
pub struct GhCliIssueSource {
    repo: String,
    ready_label: String,
}

impl GhCliIssueSource {
    pub fn new(repo: impl Into<String>, ready_label: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            ready_label: ready_label.into(),
        }
    }

    /// Run `gh` with the given args, returning stdout on success.
    async fn run_gh(&self, args: &[String]) -> Result<Vec<u8>, String> {
        let output = tokio::process::Command::new("gh")
            .args(args)
            .output()
            .await
            .map_err(|err| format!("failed to spawn gh: {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "gh exited with {}: {}",
                output.status,
                stderr.trim()
            ));
        }
        Ok(output.stdout)
    }
}

/// A label as `gh issue list --json labels` reports it: an object with
/// a `name`. Used only to normalise the CLI's nested shape onto the
/// flat [`Issue::labels`] the state machine reads.
#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

impl From<GhIssue> for Issue {
    fn from(gh: GhIssue) -> Self {
        Issue {
            number: gh.number,
            title: gh.title,
            labels: gh.labels.into_iter().map(|l| l.name).collect(),
        }
    }
}

/// Parse `gh issue list --json ...` output into [`Issue`]s. Pure and
/// unit-tested — the CLI's JSON shape (labels as `{name}` objects) is
/// normalised here.
fn parse_gh_issue_list(stdout: &[u8]) -> Result<Vec<Issue>, WatcherError> {
    let gh_issues: Vec<GhIssue> =
        serde_json::from_slice(stdout).map_err(|err| WatcherError::Parse(err.to_string()))?;
    Ok(gh_issues.into_iter().map(Issue::from).collect())
}

#[async_trait]
impl IssueSource for GhCliIssueSource {
    async fn list_open_issues(&self) -> Result<Vec<Issue>, WatcherError> {
        // Filter to the ready label at the API to keep the response
        // small; the pure planner re-checks labels so this is an
        // optimisation, not the load-bearing filter.
        let args = vec![
            "issue".to_string(),
            "list".to_string(),
            "--repo".to_string(),
            self.repo.clone(),
            "--state".to_string(),
            "open".to_string(),
            "--label".to_string(),
            self.ready_label.clone(),
            "--json".to_string(),
            "number,title,labels".to_string(),
            "--limit".to_string(),
            "100".to_string(),
        ];
        let stdout = self.run_gh(&args).await.map_err(WatcherError::ListIssues)?;
        parse_gh_issue_list(&stdout)
    }

    async fn relabel(
        &self,
        number: u64,
        remove_label: &str,
        add_label: &str,
    ) -> Result<(), WatcherError> {
        let args = vec![
            "issue".to_string(),
            "edit".to_string(),
            number.to_string(),
            "--repo".to_string(),
            self.repo.clone(),
            "--remove-label".to_string(),
            remove_label.to_string(),
            "--add-label".to_string(),
            add_label.to_string(),
        ];
        self.run_gh(&args)
            .await
            .map_err(|message| WatcherError::Relabel { number, message })?;
        Ok(())
    }
}

/// The GitHub issue watcher. Owns an [`IssueSource`] (the GitHub
/// seam), the [`EventBus`] it publishes triggers on, and the
/// [`WatcherConfig`]. Runs a slow poll loop until cancelled.
pub struct Watcher<S: IssueSource> {
    source: S,
    bus: EventBus,
    config: WatcherConfig,
}

impl<S: IssueSource> Watcher<S> {
    pub fn new(source: S, bus: EventBus, config: WatcherConfig) -> Self {
        Self {
            source,
            bus,
            config,
        }
    }

    /// Run one poll: list open issues, plan which to pick up (label
    /// state machine + concurrency cap), and for each — relabel first,
    /// then publish the trigger. Returns the count of issues actually
    /// triggered.
    ///
    /// Ordering is deliberate: the relabel `ready` -> `in-progress`
    /// commits *before* the trigger publishes. If the relabel fails
    /// the issue is skipped and left `ready` for a later poll (it was
    /// never triggered, so no double-fire). If the trigger publish
    /// fails after a successful relabel, the issue is now
    /// `in-progress` and will not be retried automatically — logged
    /// loudly so an operator can requeue it by re-adding `ready`.
    pub async fn poll_once(&self) -> Result<usize, WatcherError> {
        let issues = self.source.list_open_issues().await?;
        let plan = plan_triggers(&issues, &self.config);
        if plan.is_empty() {
            return Ok(0);
        }
        info!(
            candidates = plan.len(),
            cap = self.config.max_triggers_per_poll,
            "watcher picking up ready issues"
        );
        let mut triggered = 0usize;
        for planned in plan {
            let number = planned.number;
            // Step 1: relabel out of `ready` FIRST. This is the dedup.
            if let Err(err) = self
                .source
                .relabel(
                    number,
                    &self.config.ready_label,
                    &self.config.in_progress_label,
                )
                .await
            {
                warn!(
                    issue = number,
                    error = %err,
                    "failed to relabel issue; leaving it `ready` for a later poll"
                );
                continue;
            }
            // Step 2: publish the trigger. The payload is the issue
            // number so the target agent knows which issue to work.
            let payload = serde_json::json!({ "issue": number });
            if let Err(err) = self
                .bus
                .publish_trigger(&self.config.target_agent, &payload)
                .await
            {
                error!(
                    issue = number,
                    agent = %self.config.target_agent,
                    error = %err,
                    "relabelled issue but FAILED to publish trigger; issue is now \
                     `in-progress` and will not auto-retry — re-add `ready` to requeue"
                );
                continue;
            }
            info!(
                issue = number,
                agent = %self.config.target_agent,
                "triggered agent for issue"
            );
            triggered += 1;
        }
        Ok(triggered)
    }

    /// Run the poll loop until `shutdown` fires. Polls immediately on
    /// start, then every `effective_poll_interval_secs`. A failed poll
    /// is logged and the loop continues — a transient GitHub outage
    /// must not kill the watcher.
    pub async fn run(
        self,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), WatcherError> {
        let interval = Duration::from_secs(self.config.effective_poll_interval_secs());
        info!(
            interval_secs = interval.as_secs(),
            target_agent = %self.config.target_agent,
            ready_label = %self.config.ready_label,
            in_progress_label = %self.config.in_progress_label,
            max_per_poll = self.config.max_triggers_per_poll,
            "issue watcher started"
        );
        loop {
            match self.poll_once().await {
                Ok(0) => {}
                Ok(n) => info!(triggered = n, "watcher poll triggered issues"),
                Err(err) => warn!(error = %err, "watcher poll failed; will retry next tick"),
            }
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("issue watcher received shutdown signal");
                    return Ok(());
                }
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            title: format!("issue {number}"),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn config() -> WatcherConfig {
        WatcherConfig {
            repo: Some("owner/repo".to_string()),
            ..WatcherConfig::default()
        }
    }

    #[test]
    fn plan_picks_ready_issue() {
        let issues = vec![issue(1, &["ready"])];
        let plan = plan_triggers(&issues, &config());
        assert_eq!(plan, vec![PlannedTrigger { number: 1 }]);
    }

    #[test]
    fn plan_skips_issue_without_ready_label() {
        let issues = vec![issue(1, &["bug"]), issue(2, &[])];
        let plan = plan_triggers(&issues, &config());
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_skips_issue_already_in_progress() {
        // Belt-and-suspenders: even if an issue somehow still shows
        // `ready`, carrying `in-progress` means it's already been
        // picked up — never re-trigger it.
        let issues = vec![issue(1, &["ready", "in-progress"])];
        let plan = plan_triggers(&issues, &config());
        assert!(plan.is_empty(), "in-progress issue must be skipped");
    }

    #[test]
    fn plan_respects_concurrency_cap() {
        let issues = vec![
            issue(3, &["ready"]),
            issue(1, &["ready"]),
            issue(2, &["ready"]),
        ];
        let mut cfg = config();
        cfg.max_triggers_per_poll = 2;
        let plan = plan_triggers(&issues, &cfg);
        // Cap of 2, oldest first (deterministic): #1 and #2.
        assert_eq!(
            plan,
            vec![PlannedTrigger { number: 1 }, PlannedTrigger { number: 2 }]
        );
    }

    #[test]
    fn plan_honours_custom_label_names() {
        let mut cfg = config();
        cfg.ready_label = "go".to_string();
        cfg.in_progress_label = "wip".to_string();
        let issues = vec![
            issue(1, &["go"]),
            issue(2, &["go", "wip"]),
            issue(3, &["ready"]),
        ];
        let plan = plan_triggers(&issues, &cfg);
        assert_eq!(plan, vec![PlannedTrigger { number: 1 }]);
    }

    #[test]
    fn parse_gh_issue_list_normalises_label_objects() {
        let json = br#"[
            {"number": 7, "title": "hi", "labels": [{"name": "ready"}, {"name": "bug"}]},
            {"number": 8, "title": "yo", "labels": []}
        ]"#;
        let issues = parse_gh_issue_list(json).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 7);
        assert_eq!(issues[0].labels, vec!["ready", "bug"]);
        assert!(issues[1].labels.is_empty());
    }

    #[test]
    fn parse_gh_issue_list_rejects_garbage() {
        let err = parse_gh_issue_list(b"not json").unwrap_err();
        assert!(matches!(err, WatcherError::Parse(_)));
    }

    /// In-memory fake GitHub, so the poll loop's relabel-before-trigger
    /// ordering and the dedup can be exercised without the network.
    /// Records every relabel call and lets a test flip an issue's
    /// labels to model the state machine advancing.
    struct FakeSource {
        issues: Mutex<Vec<Issue>>,
        relabels: Mutex<Vec<(u64, String, String)>>,
        fail_relabel_for: Option<u64>,
    }

    impl FakeSource {
        fn new(issues: Vec<Issue>) -> Self {
            Self {
                issues: Mutex::new(issues),
                relabels: Mutex::new(Vec::new()),
                fail_relabel_for: None,
            }
        }
    }

    #[async_trait]
    impl IssueSource for FakeSource {
        async fn list_open_issues(&self) -> Result<Vec<Issue>, WatcherError> {
            Ok(self.issues.lock().unwrap().clone())
        }

        async fn relabel(
            &self,
            number: u64,
            remove_label: &str,
            add_label: &str,
        ) -> Result<(), WatcherError> {
            if self.fail_relabel_for == Some(number) {
                return Err(WatcherError::Relabel {
                    number,
                    message: "boom".to_string(),
                });
            }
            self.relabels.lock().unwrap().push((
                number,
                remove_label.to_string(),
                add_label.to_string(),
            ));
            // Advance the state machine: move the label on the stored
            // issue so a re-poll no longer sees it as ready.
            let mut issues = self.issues.lock().unwrap();
            if let Some(issue) = issues.iter_mut().find(|i| i.number == number) {
                issue.labels.retain(|l| l != remove_label);
                issue.labels.push(add_label.to_string());
            }
            Ok(())
        }
    }

    /// NATS-gated end-to-end: relabel commits before the trigger
    /// publishes, the trigger carries the issue number, and a second
    /// poll does not re-fire (the relabel is the dedup).
    #[tokio::test]
    async fn poll_relabels_then_triggers_and_dedups() {
        use futures::StreamExt;

        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        // Unique agent so parallel test runs don't cross-talk on the
        // shared trigger stream.
        let agent = format!("watch-test-{}", uuid::Uuid::now_v7().simple());
        let mut cfg = config();
        cfg.target_agent = agent.clone();

        let source = FakeSource::new(vec![issue(42, &["ready"]), issue(43, &["bug"])]);
        let consumer = bus
            .trigger_consumer_with_filter(
                &format!("watch-test-c-{}", uuid::Uuid::now_v7().simple()),
                &crate::bus::trigger_subject(&agent),
            )
            .await
            .expect("consumer");

        let watcher = Watcher::new(source, bus, cfg);
        let triggered = watcher.poll_once().await.expect("poll");
        assert_eq!(triggered, 1, "only the ready issue triggers");

        // The relabel happened, ready -> in-progress, before the trigger.
        {
            let relabels = watcher.source.relabels.lock().unwrap();
            assert_eq!(relabels.len(), 1);
            assert_eq!(
                relabels[0],
                (42, "ready".to_string(), "in-progress".to_string())
            );
        }

        // The published trigger carries the issue number.
        let mut messages = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(Duration::from_secs(2), messages.next())
            .await
            .expect("trigger timeout")
            .expect("stream closed")
            .expect("message");
        let payload: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
        assert_eq!(payload["issue"], 42);
        msg.ack().await.ok();

        // A second poll sees #42 as in-progress now — no re-fire.
        let again = watcher.poll_once().await.expect("second poll");
        assert_eq!(again, 0, "relabel is the dedup: no double-trigger");
    }

    /// A relabel failure leaves the issue `ready` and does not trigger
    /// — the ordering guarantees no publish happens without a
    /// successful relabel.
    #[tokio::test]
    async fn poll_skips_issue_when_relabel_fails() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        let mut source = FakeSource::new(vec![issue(9, &["ready"])]);
        source.fail_relabel_for = Some(9);
        let cfg = config();
        let watcher = Watcher::new(source, bus, cfg);

        let triggered = watcher.poll_once().await.expect("poll");
        assert_eq!(triggered, 0, "relabel failure means no trigger");
        // Issue is still ready (state machine never advanced).
        let issues = watcher.source.list_open_issues().await.unwrap();
        assert!(issues[0].has_label("ready"));
    }
}
