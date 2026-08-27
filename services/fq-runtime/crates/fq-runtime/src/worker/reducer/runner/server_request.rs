//! Servicing the requests an MCP server initiates back at the runtime
//! (ADR-0018): sampling, elicitation, and the evaluators that gate
//! both.
//!
//! Extracted from `runner.rs` (#78). The runner is the sole arbiter
//! here — the MCP handler is a thin bridge that forwards a request and
//! waits on a oneshot — so the gate/run/validate logic is the runtime's
//! and lives together, away from the host loop that merely pumps it.
//!
//! This group is entered from exactly one place, `run_tool`'s select
//! over the server-request channel, and everything it needs about the
//! invocation arrives as one [`InvocationCtx`]. That is what made the
//! move possible: before the context was bundled, these four methods
//! took nine to thirteen arguments each and the boundary would have
//! been worse than the file.
//!
//! What deliberately stayed behind in `runner.rs`: `ContextTracker` and
//! `ModelOutcome`, which read as part of this region but are used by
//! the host loop; and the tool-naming, grant, preamble and
//! workspace-binding helpers that were interleaved with the evaluator
//! block by accident of insertion order rather than by kinship.

use super::*;

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Service one inbound server-initiated request (ADR-0018). The
    /// runner is the sole arbiter; the handler is a thin bridge, so
    /// the gate/run/validate logic lives here and replies on the
    /// request's oneshot. A dropped reply (the tool finished and the
    /// bridge went away) is ignored.
    pub(super) async fn handle_server_request(
        &self,
        ctx: &mut InvocationCtx<'_>,
        agent: &Agent,
        server: &str,
        request: ServerRequest,
    ) -> Result<(), ExecutorError> {
        match request {
            ServerRequest::Sampling { params, reply } => {
                let result = self.handle_sampling(ctx, agent, server, params).await?;
                let _ = reply.send(result);
                Ok(())
            }
            ServerRequest::Elicitation { params, reply } => {
                let result = self.handle_elicitation(ctx, agent, server, params).await?;
                let _ = reply.send(result);
                Ok(())
            }
        }
    }

    /// Answer a `sampling/createMessage` request (ADR-0018 §2):
    /// **gate** (granted? within the sampling sub-budget and the
    /// invocation total?) → **run** through the shared LLM path tagged
    /// `origin = sampling{server}` → **validate** the result through
    /// the outbound seam → reply. A policy refusal or a model failure
    /// returns a structured decline to the *server* and leaves the
    /// agent invocation untouched — sampling spends the agent's
    /// budget but never fails its turn. The outer `Err` is
    /// infrastructure (store / bus) and does propagate.
    pub(super) async fn handle_sampling(
        &self,
        ctx: &mut InvocationCtx<'_>,
        agent: &Agent,
        server: &str,
        params: CreateMessageRequestParams,
    ) -> Result<Result<CreateMessageResult, rmcp::ErrorData>, ExecutorError> {
        // --- gate (no model call on refusal) ---
        let Some(grant) = agent.sampling_grant() else {
            return Ok(Err(sampling_decline(
                "sampling is not granted for this agent",
            )));
        };
        if !grant.permits(server) {
            return Ok(Err(sampling_decline(&format!(
                "sampling is not granted for server '{server}'"
            ))));
        }
        if let Some(max) = grant.max_cost
            && ctx.totals.sampling_cost >= max
        {
            return Ok(Err(sampling_decline(
                "sampling sub-budget exhausted for this invocation",
            )));
        }
        if let Some(budget) = agent.budget()
            && ctx.totals.total_cost >= budget
        {
            return Ok(Err(sampling_decline(
                "invocation budget exhausted; sampling refused",
            )));
        }

        // --- run through the shared LLM path, tagged as sampling ---
        // (Inbound `includeContext` is forced to `none`: we do not
        // inject agent/MCP context into a server's prompt yet, so a
        // server cannot pull context it was not granted. The inbound
        // redact seam lands with context injection.)
        let origin = LlmCallOrigin::Sampling {
            server: server.to_string(),
        };

        // --- input evaluator gates (may decline before any model call) ---
        if let EvaluatorOutcome::Denied(reason) = self
            .run_evaluators(
                ctx,
                &agent.sampling_validation().input_validation,
                "forwarding a sampling request to the agent's model",
                &serde_json::to_string(&params).unwrap_or_default(),
                agent.model(),
                origin.clone(),
                |t, c| t.sampling_cost += c,
            )
            .await?
        {
            return Ok(Err(sampling_decline(&format!(
                "sampling request denied by evaluator: {reason}"
            ))));
        }

        let model_request = sampling_to_model_request(agent.model(), &params);
        let (response, call_cost) = match self
            .dispatch_llm(ctx, model_request, origin.clone(), None)
            .await?
        {
            Ok(pair) => pair,
            // A sampling model failure declines the request; the agent
            // invocation continues (ADR-0018: the failure is the
            // server's, not the agent's).
            Err(err) => {
                return Ok(Err(sampling_decline(&format!(
                    "sampling model call failed: {err}"
                ))));
            }
        };
        ctx.totals.sampling_cost += call_cost;

        // --- outbound validation seam: the pluggable context chain
        // (empty by default) then the agent's declarative config
        // (redaction when `redact_secrets`). ---
        let result = model_response_to_create_message(agent.model(), response);
        let result = match self.context.sampling_validators.run(result) {
            Ok(result) => result,
            Err(reason) => {
                return Ok(Err(sampling_decline(&format!(
                    "sampling result rejected by policy: {reason}"
                ))));
            }
        };
        let result =
            match crate::policy::sampling_output_chain(agent.sampling_validation()).run(result) {
                Ok(validated) => validated,
                Err(reason) => {
                    return Ok(Err(sampling_decline(&format!(
                        "sampling result rejected by policy: {reason}"
                    ))));
                }
            };

        // --- output evaluator gates (judge the result before reply) ---
        if let EvaluatorOutcome::Denied(reason) = self
            .run_evaluators(
                ctx,
                &agent.sampling_validation().output_validation,
                "returning a sampling result to the requesting MCP server",
                &sampling_message_text(&result.message.content),
                agent.model(),
                origin,
                |t, c| t.sampling_cost += c,
            )
            .await?
        {
            return Ok(Err(sampling_decline(&format!(
                "sampling result denied by evaluator: {reason}"
            ))));
        }

        Ok(Ok(result))
    }

    /// Answer an `elicitation/create` request (ADR-0018 §4). Same
    /// gate / shared-LLM-path / cost attribution as sampling, but the
    /// answer is a **schema-constrained structured completion**: the
    /// model is asked for JSON matching the requested schema, validated
    /// against it, and retried up to [`ELICITATION_MAX_RETRIES`] times;
    /// a still-invalid result, a refusal (ungranted / over-budget), or
    /// a model failure all resolve to a `decline` *result* (not an
    /// error) so the server continues without the input. The outer
    /// `Err` is infrastructure (store / bus).
    async fn handle_elicitation(
        &self,
        ctx: &mut InvocationCtx<'_>,
        agent: &Agent,
        server: &str,
        params: CreateElicitationRequestParams,
    ) -> Result<Result<CreateElicitationResult, rmcp::ErrorData>, ExecutorError> {
        let decline = || Ok(Ok(elicitation_decline()));

        // --- gate (no model call on refusal) ---
        let Some(grant) = agent.elicitation_grant() else {
            return decline();
        };
        if !grant.permits(server) {
            return decline();
        }
        if let Some(max) = grant.max_cost
            && ctx.totals.elicitation_cost >= max
        {
            return decline();
        }
        if let Some(budget) = agent.budget()
            && ctx.totals.total_cost >= budget
        {
            return decline();
        }

        // --- inbound validation seam: the pluggable context chain
        // (empty by default) then the agent's declarative request policy
        // (sensitive-field rejection when `reject_sensitive_fields`). ---
        let params = match self.context.elicitation_inbound_validators.run(params) {
            Ok(params) => params,
            Err(_) => return decline(),
        };
        let params = match crate::policy::elicitation_input_chain(agent.elicitation_validation())
            .run(params)
        {
            Ok(params) => params,
            Err(_) => return decline(),
        };

        // --- input evaluator gates (judge the request before answering) ---
        let origin = LlmCallOrigin::Elicitation {
            server: server.to_string(),
        };
        if let EvaluatorOutcome::Denied(_) = self
            .run_evaluators(
                ctx,
                &agent.elicitation_validation().input_validation,
                "answering an elicitation request from an MCP server",
                &serde_json::to_string(&params).unwrap_or_default(),
                agent.model(),
                origin.clone(),
                |t, c| t.elicitation_cost += c,
            )
            .await?
        {
            return decline();
        }

        // Only form-mode elicitation is supported; URL mode declines.
        let CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = params
        else {
            return decline();
        };

        // --- schema-constrained structured completion (bounded retry) ---
        // Delegates to the reusable `run_structured_completion` primitive;
        // only the request builder, schema validation, and sub-budget
        // attribution are elicitation-specific.
        let model = agent.model();
        let value = self
            .run_structured_completion(
                ctx,
                origin.clone(),
                ELICITATION_MAX_RETRIES,
                || elicitation_to_model_request(model, &message, &requested_schema),
                parse_elicitation_value,
                |value| validate_against_elicitation_schema(value, &requested_schema),
                &self.context.elicitation_outbound_validators,
                |totals, cost| totals.elicitation_cost += cost,
            )
            .await?;

        let Some(value) = value else {
            return decline();
        };
        // Declarative outbound redaction on the accepted value (the
        // pluggable context outbound seam already ran inside the
        // structured-completion primitive).
        let value = match crate::policy::elicitation_output_chain(agent.elicitation_validation())
            .run(value)
        {
            Ok(value) => value,
            Err(_) => return decline(),
        };

        // --- output evaluator gates (judge the elicited value) ---
        if let EvaluatorOutcome::Denied(_) = self
            .run_evaluators(
                ctx,
                &agent.elicitation_validation().output_validation,
                "returning an elicited value to the requesting MCP server",
                &serde_json::to_string(&value).unwrap_or_default(),
                agent.model(),
                origin,
                |t, c| t.elicitation_cost += c,
            )
            .await?
        {
            return decline();
        }

        Ok(Ok(CreateElicitationResult {
            action: ElicitationAction::Accept,
            content: Some(value),
            meta: None,
        }))
    }

    /// Run an ordered evaluator sequence (A1c) against `subject` with AND
    /// semantics: the first deny short-circuits and the rest do not run;
    /// an empty sequence — or all-approve — passes. `ApproveAll` /
    /// `DenyAll` are deterministic; `Llm` runs a model judge via the
    /// structured-completion primitive on the agent's model (or a
    /// configured cheaper one), asking for a
    /// `{ "approved": bool, "reason": string }` verdict. A judge that
    /// returns no parseable verdict fails closed (denies). Each judge
    /// call's cost is attributed through `record_cost`.
    // 8/7 with the context bundled: the remainder describe the
    // evaluation itself (specs, subject, model) rather than the
    // invocation it runs inside.
    #[allow(clippy::too_many_arguments)]
    async fn run_evaluators(
        &self,
        ctx: &mut InvocationCtx<'_>,
        evaluators: &[EvaluatorSpec],
        context: &str,
        subject: &str,
        default_model: &str,
        origin: LlmCallOrigin,
        mut record_cost: impl FnMut(&mut InvocationTotals, f64),
    ) -> Result<EvaluatorOutcome, ExecutorError> {
        let empty_outbound = ValidatorChain::new();
        for evaluator in evaluators {
            match evaluator {
                EvaluatorSpec::ApproveAll => {}
                EvaluatorSpec::DenyAll => {
                    return Ok(EvaluatorOutcome::Denied("denied by deny_all".to_string()));
                }
                EvaluatorSpec::Llm { model } => {
                    let model = model.as_deref().unwrap_or(default_model).to_string();
                    let value = self
                        .run_structured_completion(
                            ctx,
                            origin.clone(),
                            EVALUATOR_MAX_RETRIES,
                            || evaluator_to_model_request(&model, context, subject),
                            parse_elicitation_value,
                            validate_evaluator_verdict,
                            &empty_outbound,
                            &mut record_cost,
                        )
                        .await?;
                    if let EvaluatorOutcome::Denied(reason) = evaluator_verdict(value) {
                        return Ok(EvaluatorOutcome::Denied(reason));
                    }
                }
            }
        }
        Ok(EvaluatorOutcome::Approved)
    }
}

/// The outcome of running an evaluator sequence (A1c).
enum EvaluatorOutcome {
    Approved,
    Denied(String),
}

/// Attempts for an LLM evaluator's verdict before failing closed.
const EVALUATOR_MAX_RETRIES: u32 = 1;

/// System instruction prefixed to every LLM evaluator judge call.
const EVALUATOR_SYSTEM_PREAMBLE: &str = "You are a safety evaluator gating an automated MCP exchange. Decide whether the content below is safe and \
     appropriate for the stated action. Respond with ONLY a single JSON object \
     {\"approved\": <true|false>, \"reason\": <string>} — no prose, no code fences.";

/// Build the judge request for an LLM evaluator: the preamble + action
/// context as the system message, the subject as the user turn. Run on
/// the configured (or agent) model; no tools.
fn evaluator_to_model_request(model: &str, context: &str, subject: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages: vec![
            Message::System {
                text: format!("{EVALUATOR_SYSTEM_PREAMBLE}\n\nAction: {context}"),
            },
            Message::User {
                text: subject.to_string(),
            },
        ],
        tools: Vec::new(),
        params: RequestParams {
            effort: None,
            temperature: None,
            max_tokens: None,
        },
    }
}

/// Validate that an evaluator response carries a boolean `approved`.
fn validate_evaluator_verdict(value: &Value) -> Result<(), String> {
    if value.get("approved").and_then(Value::as_bool).is_some() {
        Ok(())
    } else {
        Err("evaluator response missing boolean `approved`".to_string())
    }
}

/// Map a parsed evaluator verdict to an outcome. A missing verdict (a
/// model failure or unparseable response after retries) fails closed:
/// denied.
fn evaluator_verdict(value: Option<Value>) -> EvaluatorOutcome {
    match value {
        Some(value) if value.get("approved").and_then(Value::as_bool) == Some(true) => {
            EvaluatorOutcome::Approved
        }
        Some(value) => {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("denied by evaluator")
                .to_string();
            EvaluatorOutcome::Denied(reason)
        }
        None => EvaluatorOutcome::Denied("evaluator returned no verdict".to_string()),
    }
}

/// A structured decline returned to a server whose sampling request
/// the runtime refuses (policy) or could not fulfil (model failure).
/// Maps to a JSON-RPC error response; the server decides how to
/// proceed without the sample.
fn sampling_decline(reason: &str) -> rmcp::ErrorData {
    rmcp::ErrorData::invalid_request(reason.to_string(), None)
}

/// Build a [`ModelRequest`] for a sampling completion from the
/// server's `sampling/createMessage` params, run on the agent's own
/// model. The server's `systemPrompt` becomes a system message; each
/// sampling message maps to a user/assistant message. Only text
/// content is injected in v1 (non-text is a placeholder); tools are
/// never exposed to a sampling call.
fn sampling_to_model_request(model: &str, params: &CreateMessageRequestParams) -> ModelRequest {
    let mut messages = Vec::with_capacity(params.messages.len() + 1);
    if let Some(system) = &params.system_prompt {
        messages.push(Message::System {
            text: system.clone(),
        });
    }
    for sampling_message in &params.messages {
        // MCP sampling turns are scripted context, not turns a model
        // produced here, so an assistant turn is a single text part.
        // `sampling_message_text` already flattens non-text content.
        let text = sampling_message_text(&sampling_message.content);
        messages.push(match sampling_message.role {
            Role::User => Message::User { text },
            Role::Assistant => Message::Assistant {
                parts: vec![AssistantPart::Text { text }],
            },
        });
    }
    ModelRequest {
        model: model.to_string(),
        messages,
        tools: Vec::new(),
        params: RequestParams {
            effort: None,
            temperature: params.temperature.map(|t| t as f64),
            max_tokens: Some(params.max_tokens),
        },
    }
}

/// Flatten a sampling message's content (single or multiple) into a
/// plain string for the agent model. Non-text content is represented
/// by a placeholder so conversation structure is preserved without
/// claiming to faithfully inject images/audio (a later capability).
fn sampling_message_text(content: &SamplingContent<SamplingMessageContent>) -> String {
    match content {
        SamplingContent::Single(item) => sampling_item_text(item),
        SamplingContent::Multiple(items) => items
            .iter()
            .map(sampling_item_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn sampling_item_text(item: &SamplingMessageContent) -> String {
    match item {
        SamplingMessageContent::Text(text) => text.text.clone(),
        _ => "[non-text sampling content omitted]".to_string(),
    }
}

/// Wrap an agent-model [`ModelResponse`] back into the
/// `CreateMessageResult` shape the protocol returns to the server.
fn model_response_to_create_message(model: &str, response: ModelResponse) -> CreateMessageResult {
    CreateMessageResult::new(
        SamplingMessage::assistant_text(response.text().unwrap_or_default()),
        model.to_string(),
    )
    .with_stop_reason(stop_reason_to_mcp(response.stop_reason))
}

fn stop_reason_to_mcp(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::EndTurn => CreateMessageResult::STOP_REASON_END_TURN,
        StopReason::MaxTokens => CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
        StopReason::StopSequence => CreateMessageResult::STOP_REASON_END_SEQUENCE,
        StopReason::ToolUse => CreateMessageResult::STOP_REASON_TOOL_USE,
    }
}

/// Max model attempts to produce a schema-valid elicitation value
/// before declining (ADR-0018 §4 — "bounded retry"). Each attempt is
/// a budget-counted LLM call.
const ELICITATION_MAX_RETRIES: u32 = 2;

/// The system instruction prefixed to every elicitation completion.
/// Kept as a constant so its presence in a recorded model request is
/// a stable signal that the schema-constrained completion ran.
const ELICITATION_SYSTEM_PREAMBLE: &str = "You are completing a structured form on the user's behalf. Respond with ONLY a single \
     JSON object that conforms to the JSON schema below — no prose, no code fences.";

/// Build the schema-constrained completion request for an elicitation:
/// a system message carrying the instruction + serialized schema, and
/// the server's human-readable `message` as the user turn. Run on the
/// agent's own model; no tools.
fn elicitation_to_model_request(
    model: &str,
    message: &str,
    schema: &ElicitationSchema,
) -> ModelRequest {
    let schema_json = serde_json::to_string_pretty(schema).unwrap_or_default();
    ModelRequest {
        model: model.to_string(),
        messages: vec![
            Message::System {
                text: format!("{ELICITATION_SYSTEM_PREAMBLE}\n\nJSON schema:\n{schema_json}"),
            },
            Message::User {
                text: message.to_string(),
            },
        ],
        tools: Vec::new(),
        params: RequestParams {
            effort: None,
            temperature: None,
            max_tokens: None,
        },
    }
}

/// Parse a model's elicitation answer into a JSON object, tolerating
/// surrounding whitespace and ```json code fences. Returns `None` if
/// the content is absent, unparseable, or not a JSON object.
fn parse_elicitation_value(content: Option<&str>) -> Option<Value> {
    let text = content?.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text);
    let text = text.strip_suffix("```").unwrap_or(text).trim();
    let value: Value = serde_json::from_str(text).ok()?;
    value.is_object().then_some(value)
}

/// Validate an elicitation value against the requested schema. The
/// schema type is already restricted to the MCP flat-object / primitive
/// subset by rmcp's deserialization; here we enforce, per field:
/// required-field presence, no unexpected fields, the property's
/// primitive type, string length / format (email / uri / date /
/// date-time), numeric range, and enum membership.
fn validate_against_elicitation_schema(
    value: &Value,
    schema: &ElicitationSchema,
) -> Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "elicitation response is not a JSON object".to_string())?;
    if let Some(required) = &schema.required {
        for key in required {
            if !obj.contains_key(key) {
                return Err(format!("missing required field '{key}'"));
            }
        }
    }
    for (key, field_value) in obj {
        let Some(property) = schema.properties.get(key) else {
            return Err(format!(
                "unexpected field '{key}' not declared in the schema"
            ));
        };
        validate_primitive_value(key, field_value, property)?;
    }
    Ok(())
}

/// Validate one field value against its primitive property schema.
fn validate_primitive_value(
    key: &str,
    value: &Value,
    schema: &PrimitiveSchema,
) -> Result<(), String> {
    match schema {
        PrimitiveSchema::String(string_schema) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("field '{key}' must be a string"))?;
            let len = text.chars().count() as u32;
            if let Some(min) = string_schema.min_length
                && len < min
            {
                return Err(format!("field '{key}' is shorter than minLength {min}"));
            }
            if let Some(max) = string_schema.max_length
                && len > max
            {
                return Err(format!("field '{key}' is longer than maxLength {max}"));
            }
            if let Some(format) = string_schema.format
                && !string_matches_format(text, format)
            {
                return Err(format!("field '{key}' is not a valid {format:?}"));
            }
            Ok(())
        }
        PrimitiveSchema::Number(number_schema) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("field '{key}' must be a number"))?;
            if let Some(min) = number_schema.minimum
                && number < min
            {
                return Err(format!("field '{key}' is below minimum {min}"));
            }
            if let Some(max) = number_schema.maximum
                && number > max
            {
                return Err(format!("field '{key}' is above maximum {max}"));
            }
            Ok(())
        }
        PrimitiveSchema::Integer(integer_schema) => {
            let number = value
                .as_i64()
                .ok_or_else(|| format!("field '{key}' must be an integer"))?;
            if let Some(min) = integer_schema.minimum
                && number < min
            {
                return Err(format!("field '{key}' is below minimum {min}"));
            }
            if let Some(max) = integer_schema.maximum
                && number > max
            {
                return Err(format!("field '{key}' is above maximum {max}"));
            }
            Ok(())
        }
        PrimitiveSchema::Boolean(_) => value
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| format!("field '{key}' must be a boolean")),
        PrimitiveSchema::Enum(enum_schema) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("field '{key}' must be a string (enum)"))?;
            let allowed = enum_allowed_values(enum_schema);
            // If the allowed set can't be extracted, fall back to the
            // type check rather than reject a spec-valid value.
            if allowed.is_empty() || allowed.iter().any(|a| a == text) {
                Ok(())
            } else {
                Err(format!(
                    "field '{key}' value '{text}' is not one of the allowed enum values"
                ))
            }
        }
    }
}

/// Whether `text` satisfies a basic check for an MCP string `format`.
/// Intentionally lightweight (no full RFC validation): enough to reject
/// obviously-wrong values without pulling in a parser.
fn string_matches_format(text: &str, format: StringFormat) -> bool {
    match format {
        StringFormat::Email => matches!(text.split_once('@'),
            Some((local, domain)) if !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')),
        StringFormat::Uri => matches!(text.split_once(':'),
            Some((scheme, _)) if !scheme.is_empty()
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))),
        StringFormat::Date => is_iso_date(text),
        StringFormat::DateTime => {
            matches!(text.split_once('T'), Some((date, _)) if is_iso_date(date))
        }
    }
}

/// Whether `text` has the ISO `YYYY-MM-DD` calendar-date shape.
fn is_iso_date(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

/// Extract an enum schema's allowed string values by serialising it and
/// reading the `enum` array (rmcp models enums as several variants, so
/// going through JSON is simpler than matching each).
fn enum_allowed_values(schema: &EnumSchema) -> Vec<String> {
    serde_json::to_value(schema)
        .ok()
        .as_ref()
        .and_then(|v| v.get("enum"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
