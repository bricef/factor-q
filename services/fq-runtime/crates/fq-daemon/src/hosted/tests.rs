//! What the daemon reports about itself, read off the config it was
//! started with.

use std::sync::Arc;

use fq_runtime::worker::{
    DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome,
};

use super::daemon_facts;

/// The admission's drain probe, and nothing else: the facts run nothing.
struct IdleWorker;

#[async_trait::async_trait]
impl fq_runtime::Worker for IdleWorker {
    async fn run_invocation(
        &self,
        _agent: &fq_runtime::Agent,
        _llm: &dyn fq_runtime::llm::LlmClient,
        _trigger: fq_runtime::trigger::Trigger,
        _delivery_attempt: Option<u32>,
        _durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        unreachable!("reading the daemon's facts runs no invocation")
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        DrainState::Running
    }
}

fn facts_for(toml: &str) -> crate::operator_surface::DaemonFacts {
    let config = fq_runtime::Config::from_toml_str(toml).expect("a valid config");
    daemon_facts(
        &config,
        Default::default(),
        &Arc::new(fq_runtime::llm::ModelThrottle::inert()),
        &fq_runtime::control_plane::dispatcher::RunAdmission::new(Arc::new(IdleWorker), 1),
        &fq_runtime::worker::WorkerId::new("hosted-test").expect("a valid worker id"),
    )
}

/// `fq doctor` says a dead claim's line stays until the claim is as old
/// as its horizon. The retention sweep deletes that claim at
/// `[state] trigger_claim_retention_days`, so that is the horizon — not
/// `retention_days`, the window dead letters age out on. The two are set
/// apart here so a report reading the wrong one shows it.
#[test]
fn the_doctor_horizon_is_the_trigger_claim_window() {
    let facts = facts_for("[state]\nretention_days = 7\ntrigger_claim_retention_days = 45\n");
    assert_eq!(facts.trigger_claim_retention_ms, Some(45 * 86_400_000));

    let facts = facts_for("[state]\nretention_days = 7\ntrigger_claim_retention_days = -1\n");
    assert_eq!(
        facts.trigger_claim_retention_ms, None,
        "claims kept for ever leave the report no horizon"
    );
}
