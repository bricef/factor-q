//! Manual reducer stepper for tests that need fine-grained
//! control over the reducer's step boundary.
//!
//! Most tests should use the [`crate::ReducerRunner`] which drives
//! a reducer to completion in one call. The stepper exists for
//! the cases where the runner isn't enough:
//!
//! - **Suspend / resume tests.** Drive a few steps, snapshot the
//!   state, drop the reducer, restore from the snapshot, continue.
//! - **Mid-flight crash simulation.** Stop after a specific step
//!   and assert what's persisted; resume in a fresh stepper.
//!   This is the building block the data-architecture-v1 plan
//!   calls "the crash-simulation helper" — it doesn't yet target
//!   specific WAL states (the WAL doesn't exist), but it gives us
//!   the deterministic stop-and-resume primitive.
//! - **State-shape tests.** Assert what the state blob looks like
//!   after every step.
//!
//! The stepper is *purely the host side of the reducer protocol*.
//! It does not run a real LLM or tools. The test supplies the
//! [`CapabilityResult`] for each step.

use crate::worker::reducer::types::{
    AgentConfig, CapabilityResult, NextAction, Reducer, StepInput, StepOutput, TriggerPayload,
};

/// Drive a [`Reducer`] step-by-step under test control.
pub struct ManualStepper<R: Reducer> {
    reducer: R,
    config: AgentConfig,
    trigger: TriggerPayload,
    state: Vec<u8>,
    step_index: u32,
    last_action: Option<NextAction>,
}

impl<R: Reducer> ManualStepper<R> {
    /// Construct a stepper bound to the given reducer, config, and
    /// trigger. The first call to [`Self::step`] should pass
    /// `last_result = None` (matching the reducer's `Phase::Initial`
    /// expectation).
    pub fn new(reducer: R, config: AgentConfig, trigger: TriggerPayload) -> Self {
        Self {
            reducer,
            config,
            trigger,
            state: Vec::new(),
            step_index: 0,
            last_action: None,
        }
    }

    /// Advance one step. The output's persisted state replaces the
    /// stepper's internal state for the next call.
    ///
    /// `now_ms` and `random_seed` are derived deterministically
    /// from the step index so repeated runs are reproducible. Tests
    /// that need control over those inputs can use [`Self::step_with_clock`].
    pub fn step(&mut self, last_result: Option<CapabilityResult>) -> StepOutput {
        let now_ms = 1_000_000 + self.step_index as u64;
        let random_seed = self.step_index as u64;
        self.step_with_clock(last_result, now_ms, random_seed)
    }

    /// Like [`Self::step`] but with explicit `now_ms` and
    /// `random_seed`. Use this for time-sensitive reducer logic
    /// (retry backoff, scheduled wakeups) once those land.
    pub fn step_with_clock(
        &mut self,
        last_result: Option<CapabilityResult>,
        now_ms: u64,
        random_seed: u64,
    ) -> StepOutput {
        let input = StepInput {
            config: self.config.clone(),
            trigger: self.trigger.clone(),
            state: self.state.clone(),
            last_result,
            now_ms,
            random_seed,
            step_index: self.step_index,
        };
        let output = self
            .reducer
            .step(input)
            .expect("reducer step in ManualStepper");
        self.state = output.state.clone();
        self.last_action = Some(output.next_action.clone());
        self.step_index += 1;
        output
    }

    /// Return the current persisted state blob (copy).
    pub fn snapshot_state(&self) -> Vec<u8> {
        self.state.clone()
    }

    /// Replace the persisted state, simulating a resume from a
    /// snapshot taken by another stepper instance.
    ///
    /// The step index is preserved — restoring state on a fresh
    /// stepper requires the caller to also restore the step index
    /// if reproducibility of `now_ms` / `random_seed` matters.
    pub fn restore_state(&mut self, state: Vec<u8>) {
        self.state = state;
    }

    /// Set the step index explicitly. Used together with
    /// [`Self::restore_state`] when resuming on a fresh stepper.
    pub fn set_step_index(&mut self, step_index: u32) {
        self.step_index = step_index;
    }

    /// Return the most recent [`NextAction`], if any.
    pub fn last_action(&self) -> Option<&NextAction> {
        self.last_action.as_ref()
    }

    /// Current step index (the value that will be used for the
    /// *next* step).
    pub fn step_index(&self) -> u32 {
        self.step_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Harness;
    use crate::agent::AgentId;
    use crate::events::{MessageToolCall, StopReason, TokenUsage, ToolSchema};
    use crate::worker::reducer::types::{
        CapabilityResult, ModelResponse, ToolCallResult, TriggerSourceKind,
    };
    use serde_json::{Value, json};

    fn config() -> AgentConfig {
        AgentConfig {
            agent_id: AgentId::new("stepper-test").unwrap(),
            model: "claude-haiku".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools_available: vec![ToolSchema {
                name: "echo".to_string(),
                description: "echo".to_string(),
                parameters_schema: json!({"type": "object"}),
            }],
            allowed_tool_names: vec!["echo".to_string()],
            max_iterations: 5,
        }
    }

    fn trigger(payload: Value) -> TriggerPayload {
        TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload,
        }
    }

    fn end_turn(text: &str) -> ModelResponse {
        ModelResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    fn tool_use(name: &str, call_id: &str) -> ModelResponse {
        ModelResponse {
            content: None,
            tool_calls: vec![MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
                tool_name: name.to_string(),
                parameters: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }

    fn tool_result(call_id: &str, output: &str) -> ToolCallResult {
        ToolCallResult {
            tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
            output: output.to_string(),
            is_error: false,
            error_kind: None,
            duration_ms: 1,
        }
    }

    #[test]
    fn step_zero_advances_phase_and_returns_call_model() {
        let mut stepper = ManualStepper::new(Harness::new(), config(), trigger(json!("hi")));
        let out = stepper.step(None);
        assert!(matches!(out.next_action, NextAction::CallModel(_)));
        assert_eq!(stepper.step_index(), 1);
        assert!(!stepper.snapshot_state().is_empty());
    }

    #[test]
    fn end_turn_response_completes_invocation() {
        let mut stepper = ManualStepper::new(Harness::new(), config(), trigger(json!("hi")));
        let _ = stepper.step(None);
        let s1 = stepper.step(Some(CapabilityResult::ModelResult(end_turn("done."))));
        match s1.next_action {
            NextAction::Complete(text) => assert_eq!(text, "done."),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_then_restore_resumes_to_completion() {
        // Drive a few steps, snapshot, drop the stepper, restore
        // in a fresh one, continue. End-state must match what an
        // uninterrupted run would produce.
        let mut s1 = ManualStepper::new(Harness::new(), config(), trigger(json!("hi")));
        let _ = s1.step(None);
        let _ = s1.step(Some(CapabilityResult::ModelResult(tool_use(
            "echo", "call_1",
        ))));

        let snapshot = s1.snapshot_state();
        let saved_index = s1.step_index();
        drop(s1);

        let mut s2 = ManualStepper::new(Harness::new(), config(), trigger(json!("hi")));
        s2.restore_state(snapshot);
        s2.set_step_index(saved_index);

        let _ = s2.step(Some(CapabilityResult::ToolResult(tool_result(
            "call_1", "echoed",
        ))));
        let final_step = s2.step(Some(CapabilityResult::ModelResult(end_turn(
            "after-resume.",
        ))));

        match final_step.next_action {
            NextAction::Complete(text) => assert_eq!(text, "after-resume."),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn last_action_reflects_most_recent_step() {
        let mut stepper = ManualStepper::new(Harness::new(), config(), trigger(json!("hi")));
        assert!(stepper.last_action().is_none());

        let _ = stepper.step(None);
        assert!(matches!(
            stepper.last_action(),
            Some(NextAction::CallModel(_))
        ));

        let _ = stepper.step(Some(CapabilityResult::ModelResult(end_turn("done."))));
        assert!(matches!(
            stepper.last_action(),
            Some(NextAction::Complete(_))
        ));
    }

    #[test]
    fn step_with_clock_threads_now_and_seed_to_reducer() {
        // Verify the supplied `now_ms` and `random_seed` reach the
        // reducer by constructing a capturing reducer.
        struct Capturing {
            seen: std::sync::Arc<std::sync::Mutex<Option<(u64, u64, u32)>>>,
        }
        impl Reducer for Capturing {
            fn step(
                &self,
                input: StepInput,
            ) -> Result<StepOutput, crate::worker::reducer::types::HarnessError> {
                *self.seen.lock().unwrap() =
                    Some((input.now_ms, input.random_seed, input.step_index));
                Ok(StepOutput {
                    next_action: NextAction::Complete("ok".to_string()),
                    state: vec![],
                    logs: vec![],
                    events: vec![],
                })
            }
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(None::<(u64, u64, u32)>));
        let cap = Capturing { seen: seen.clone() };
        let mut stepper = ManualStepper::new(cap, config(), trigger(json!("hi")));
        let _ = stepper.step_with_clock(None, 42, 99);
        assert_eq!(seen.lock().unwrap().unwrap(), (42, 99, 0));
    }
}
