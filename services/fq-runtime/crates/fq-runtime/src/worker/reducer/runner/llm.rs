//! The invocation's own model calls: the shared dispatch core, the
//! agent-turn path over it, and the retrying structured-completion
//! primitive the ADR-0018 servicing builds on.
//!
//! Extracted from `runner.rs` (#78). Every call the runtime makes to a
//! model goes through `dispatch_llm` — agent turns, sampling,
//! elicitation and evaluators alike — which is why cost accounting,
//! the event pair and the failure classification are written once,
//! here, rather than at each caller.
//!
//! `ModelOutcome` stays in `runner.rs` even though `run_model_with_llm`
//! returns it: the host loop matches on it directly to decide whether a
//! budget stop ends the invocation.

use super::*;

/// Internal: factor out the LLM dispatch path so the loop body
/// stays readable.
impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Agent-turn LLM path: dispatch through the shared core, then
    /// apply agent-turn failure semantics — an LLM error fails the
    /// invocation, and exceeding the budget terminates it.
    pub(super) async fn run_model_with_llm(
        &self,
        ctx: &mut InvocationCtx<'_>,
        budget: Option<f64>,
        request: ModelRequest,
        origin: LlmCallOrigin,
        start: Instant,
        context: &mut ContextTracker,
    ) -> Result<ModelOutcome, ExecutorError> {
        let response = match self
            .dispatch_llm(ctx, request, origin, Some(context))
            .await?
        {
            Ok((response, _cost)) => response,
            Err(err) => {
                ctx.totals.total_duration_ms = start.elapsed().as_millis() as u64;
                self.emit_failed(
                    ctx.agent_id,
                    ctx.invocation_id,
                    FailureKind::LlmError,
                    err.to_string(),
                    FailurePhase::LlmRequest,
                    *ctx.totals,
                    ctx.cursor,
                )
                .await?;
                return Err(ExecutorError::Llm(err));
            }
        };

        if let Some(budget) = budget
            && ctx.totals.total_cost > budget
        {
            ctx.totals.total_duration_ms = start.elapsed().as_millis() as u64;
            self.emit_failed(
                ctx.agent_id,
                ctx.invocation_id,
                FailureKind::BudgetExceeded,
                format!(
                    "cost ${:.6} exceeded budget ${budget:.2}",
                    ctx.totals.total_cost
                ),
                FailurePhase::LlmResponse,
                *ctx.totals,
                ctx.cursor,
            )
            .await?;
            return Ok(ModelOutcome::BudgetExceeded(ctx.totals.total_cost));
        }

        Ok(ModelOutcome::Response(response))
    }

    /// At-use pricing backstop (ADR-0004): when enabled, refuse a model
    /// with no pricing rather than dispatch and track its cost as $0 —
    /// which would silently defeat the budget check.
    ///
    /// Called before any WAL write, so a refused call leaves no trace.
    /// Both agent turns and sampling flow through `dispatch_llm`; each
    /// applies its own semantics to the returned inner `Err` (a turn
    /// fails the invocation, a sampling request declines). Unreachable
    /// when the startup pricing guarantee holds — defence in depth.
    fn unpriced_model_refusal(&self, model: &str) -> Option<crate::llm::LlmError> {
        (self.config.enforce_pricing && self.config.pricing.lookup(model).is_none())
            .then(|| crate::llm::LlmError::UnpricedModel(model.to_string()))
    }

    /// Shared LLM dispatch core (ADR-0018 §2): the single WAL'd /
    /// evented / budgeted path every model call flows through — agent
    /// turns and sampling alike. Writes the §5.5 WAL
    /// (intent → dispatched → completed), publishes
    /// `ctx.llm.request` / `ctx.llm.dispatched` / `ctx.llm.response` + cost (the
    /// cost tagged with `origin` for attribution), and folds cost into
    /// `ctx.totals`. Returns the inner `Err` on an LLM-call failure (the
    /// WAL is already closed `is_error`) so each caller applies its
    /// own semantics — an agent turn fails the invocation, a sampling
    /// request merely declines. The outer `Err` is infrastructure
    /// (store / bus).
    pub(super) async fn dispatch_llm(
        &self,
        ctx: &mut InvocationCtx<'_>,
        request: ModelRequest,
        origin: LlmCallOrigin,
        // Agent turns pass their invocation-scoped context tracker so
        // occupancy/history are recorded and the one-shot context-
        // pressure warning can be latched and injected here (issue #76).
        // Sampling / elicitation / evaluator calls pass `None` — those
        // are server-initiated and do not drive the agent's own context
        // signal.
        context: Option<&mut ContextTracker>,
    ) -> Result<Result<(ModelResponse, f64), crate::llm::LlmError>, ExecutorError> {
        let call_id = Uuid::now_v7();
        let inv_str = ctx.invocation_id.to_string();
        let req_str = call_id.to_string();
        let chat_request = ChatRequest {
            model: request.model.clone(),
            messages: request.messages.clone(),
            tools: request.tools.clone(),
            params: request.params.clone(),
        };

        if let Some(refusal) = self.unpriced_model_refusal(&chat_request.model) {
            return Ok(Err(refusal));
        }

        // §5.5 write order applied to LLM calls: SQL first, then
        // NATS publish, then the LLM call, then dispatched, then
        // completed, then response/cost events.
        let request_payload_json =
            serde_json::to_string(&chat_request).unwrap_or_else(|_| "{}".to_string());
        self.config
            .store
            .write_llm_intent(
                &inv_str,
                &req_str,
                &chat_request.model,
                &request_payload_json,
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        self.publish_chained(
            ctx.cursor,
            Event::new(
                ctx.agent_id.clone(),
                ctx.invocation_id,
                EventPayload::LlmRequest(LlmRequestPayload {
                    call_id,
                    model: chat_request.model.clone(),
                    messages: chat_request.messages.clone(),
                    tools_available: chat_request.tools.clone(),
                    request_params: chat_request.params.clone(),
                    origin: origin.clone(),
                }),
            ),
        )
        .await?;

        let call_started = Instant::now();
        let response = match ctx.llm.chat(chat_request).await {
            Ok(r) => r,
            Err(err) => {
                // The provider errored. Nothing was parsed, so there is
                // no usage to recover: the spend, if the request billed
                // server-side before we lost the response, is real but
                // unobservable, and `None` says exactly that.
                self.fail_llm_call(
                    FailedCall {
                        agent_id: ctx.agent_id,
                        invocation_id: ctx.invocation_id,
                        call_id,
                        model: &request.model,
                        error_kind: (&err).into(),
                        error_message: err.to_string(),
                        duration_ms: call_started.elapsed().as_millis() as u64,
                        usage: None,
                        origin: &origin,
                    },
                    ctx.totals,
                    ctx.cursor,
                )
                .await?;
                // Hand the LLM error back to the caller; the WAL is
                // already closed `is_error`, so this is a final state.
                return Ok(Err(err));
            }
        };

        if response.tool_calls.is_empty()
            && response
                .content
                .as_deref()
                .is_none_or(|content| content.trim().is_empty())
        {
            // A 200 with nothing in it. Skips `ctx.totals.total_llm_calls`
            // — no outcome to count — but *not* the spend: the provider
            // did the prefill and `response.usage` says what it billed,
            // so it is priced and recorded like any other call.
            let err = crate::llm::LlmError::RequestFailed(
                "model returned an empty response (no content, no tool calls)".to_string(),
            );
            self.fail_llm_call(
                FailedCall {
                    agent_id: ctx.agent_id,
                    invocation_id: ctx.invocation_id,
                    call_id,
                    model: &request.model,
                    error_kind: crate::events::LlmErrorKind::EmptyResponse,
                    error_message: err.to_string(),
                    duration_ms: call_started.elapsed().as_millis() as u64,
                    usage: Some(response.usage),
                    origin: &origin,
                },
                ctx.totals,
                ctx.cursor,
            )
            .await?;
            return Ok(Err(err));
        }

        ctx.totals.total_llm_calls += 1;

        // LLM returned control. Mark dispatched (ambiguous
        // window), publish the dispatched event, then transition
        // to completed before the response/cost events go out.
        self.config
            .store
            .write_llm_dispatched(&inv_str, &req_str, self.config.clock.unix_now_ms())
            .await
            .map_err(map_store_err)?;
        self.publish_chained(
            ctx.cursor,
            Event::new(
                ctx.agent_id.clone(),
                ctx.invocation_id,
                EventPayload::LlmDispatched(events::LlmDispatchedPayload {
                    call_id,
                    model: request.model.clone(),
                }),
            ),
        )
        .await?;
        // Cost is computed before the WAL completed-write so the row
        // carries the call's real cost — resume() reconstitutes the
        // budget accumulator from exactly this column, so a 0.0 here
        // silently forgets pre-crash spend on every resume (finding 4,
        // caught by the slice-6 budget-across-resume property; the
        // old comment claimed the cost was "filled in below", which
        // never happened).
        let pricing = self.config.pricing.lookup(&request.model);
        if pricing.is_none() {
            warn!(
                model = %request.model,
                "no pricing known for model; cost will be reported as $0"
            );
        }
        let (input_cost, output_cost, total_cost) = pricing
            .map(|p| p.calculate(&response.usage))
            .unwrap_or((0.0, 0.0, 0.0));
        ctx.totals.total_cost += total_cost;

        let response_json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        self.config
            .store
            .write_llm_completed(
                &inv_str,
                &req_str,
                &response_json,
                false,
                total_cost,
                self.config.clock.unix_now_ms(),
            )
            .await
            .map_err(map_store_err)?;

        // Context-pressure tracking + one-shot soft warning (issue
        // #76). Only agent turns carry a tracker; sampling/elicitation
        // pass `None`. We record the turn's occupancy and history, and
        // — the first time occupancy crosses the soft threshold — latch
        // and annotate this `ctx.llm.response` event so the warning is
        // visible in the event trail exactly once (annotations ride on
        // the envelope and are stripped only from downstream consumer
        // prompts, so this does not perturb the canonical trace).
        let mut context_warning: Option<String> = None;
        if let Some(tracker) = context {
            tracker.tokens_in_use = Some(response.usage.input_tokens);
            tracker.messages_in_history = Some(request.messages.len() as u32);
            let window = self.config.pricing.context_window(&request.model);
            if crate::worker::introspection::context_pressure(
                Some(response.usage.input_tokens),
                window,
            )
            .is_some()
                && !tracker.warning_emitted
            {
                tracker.warning_emitted = true;
                warn!(
                    ctx.agent_id = %ctx.agent_id,
                    ctx.invocation_id = %ctx.invocation_id,
                    tokens_in_use = response.usage.input_tokens,
                    context_window = ?window,
                    "{}",
                    crate::worker::introspection::CONTEXT_PRESSURE_WARNING
                );
                context_warning =
                    Some(crate::worker::introspection::CONTEXT_PRESSURE_WARNING.to_string());
            }
        }

        let mut response_event = crate::worker::reducer::emit::llm_response_event(
            self.rounds.next(ctx.invocation_id),
            ctx.agent_id,
            ctx.invocation_id,
            call_id,
            &response,
            origin,
            request.model.clone(),
            input_cost,
            output_cost,
            total_cost,
            ctx.totals.total_cost,
        );
        if let Some(message) = context_warning {
            response_event = response_event.annotate(
                crate::events::annotation_keys::FLAGS,
                serde_json::json!({ "context_pressure": message }),
            );
        }
        self.publish_chained(ctx.cursor, response_event).await?;

        Ok(Ok((
            ModelResponse {
                content: response.content,
                tool_calls: response.tool_calls,
                stop_reason: response.stop_reason,
                usage: response.usage,
            },
            total_cost,
        )))
    }

    /// Run a schema-constrained structured completion — the reusable
    /// primitive behind elicitation (ADR-0018 §4), shaped so the future
    /// sampling evaluator-validator and spawn-deliverable typing reuse
    /// it. Build a request, dispatch it on the agent's model, parse the
    /// response, and validate the parsed value — retrying up to
    /// `max_retries` times. Returns the first value that parses, passes
    /// `validate`, *and* survives the `outbound` seam
    /// (`Ok(Some(value))`); a model failure, exhausted retries, or an
    /// outbound denial all yield `Ok(None)` so the caller can decline.
    /// `record_cost` attributes each dispatched call's cost to the
    /// caller's sub-budget. The outer `Err` is infrastructure
    /// (store / bus).
    // 9/7 even with the context bundled: the rest are this primitive's
    // four injected behaviours (build / parse / validate / attribute),
    // which are what make it reusable rather than invocation state.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_structured_completion(
        &self,
        ctx: &mut InvocationCtx<'_>,
        origin: LlmCallOrigin,
        max_retries: u32,
        build_request: impl Fn() -> ModelRequest,
        parse: impl Fn(Option<&str>) -> Option<Value>,
        validate: impl Fn(&Value) -> Result<(), String>,
        outbound: &ValidatorChain<Value>,
        mut record_cost: impl FnMut(&mut InvocationTotals, f64),
    ) -> Result<Option<Value>, ExecutorError> {
        for _ in 0..max_retries {
            let response = match self
                .dispatch_llm(ctx, build_request(), origin.clone(), None)
                .await?
            {
                Ok((response, cost)) => {
                    record_cost(ctx.totals, cost);
                    response
                }
                // A model failure resolves to "no value"; the caller
                // declines and the agent turn is untouched.
                Err(_) => return Ok(None),
            };

            let Some(value) = parse(response.content.as_deref()) else {
                continue; // unparseable → retry
            };
            if validate(&value).is_err() {
                continue; // invalid → retry
            }

            // Outbound seam: a denial censors the whole result.
            return match outbound.run(value) {
                Ok(value) => Ok(Some(value)),
                Err(_) => Ok(None),
            };
        }
        // Retries exhausted without a valid result.
        Ok(None)
    }
}
