# Plan: Bring the MCP client up to full spec

**Date**: 2026-05-28
**Status**: Active

## Goal

factor-q's MCP client is tools-only. It discovers and calls
tools (`mcp.rs`: `list_all_tools` + `call_tool`) and drops
everything else — the tool-result handler explicitly discards
non-text content (`mcp.rs:99-101`), and the client runs with
the no-op `()` service handler (`mcp.rs:49`, `().serve(...)`
at `:190`), so it cannot answer any server-initiated request.

Bring the client up to the full MCP surface (spec revision
**2025-11-25**) so factor-q agents can interact idiomatically
with arbitrary third-party servers, not just our own:

- **Resources** — list / read / templates / subscribe (+ the
  context-injection decision the tools-only path never had to
  make).
- **Prompts + Completion** — list / get, argument completion.
- **Sampling** — answer `sampling/createMessage` through our
  cost controls and event bus.
- **Roots** — expose workspace boundaries to servers.
- **Elicitation** — answer `elicitation/create` autonomously.
- **Utilities** — `list_changed` notifications, resource
  subscription updates, progress, cancellation, pagination,
  logging.

The driving invariant for all server-initiated work: **a
running agent assumes no human is available** (see Step 1).

## Context

- **The SDK already supports this.** `rmcp` 1.4 (features
  `["client", "transport-child-process"]`) exposes the full
  client surface; `ClientHandler` is the trait for
  server-initiated requests (sampling, roots, elicitation,
  logging, completions, subscriptions). We just never wired it
  up — we pass `()` as the handler.
- **The reference test server is already in our tests.**
  `tests/mcp_integration.rs` drives
  `@modelcontextprotocol/server-everything` over stdio with a
  `require_npx()` skip guard and an `everything_config()`
  fixture. The everything server is purpose-built to exercise
  every protocol feature, so it is the natural TDD oracle for
  every step here.
- **Toolchain is present.** `node` 25.9.0 + `npx` via mise,
  `docker` available. The everything server runs as a stdio
  child process — the exact transport we already have.
- **Deterministic LLM for the loop-back steps.**
  `test_support::mock_anthropic` + `TestRuntime`
  (`test_support/runtime.rs`) give a scripted model, so
  sampling and elicitation can be tested without a live
  provider.
- **Config wiring exists.** Agent definitions already carry
  `mcp_servers: Vec<McpServerDeclaration>` (`agent.rs:54`);
  this plan extends that declaration with the capability
  grants Step 1 defines.

## Decisions taken on 2026-05-28

- **No human at the invocation level.** A running agent must
  assume a human user is *not* reachable. Every MCP
  human-in-the-loop hook is resolved autonomously inside the
  invocation; genuine human-in-the-loop is a *workflow-layer*
  concern, above the invocation, out of scope here. Ratified
  as ADR-0017 in Step 1.
- **Authorization is declarative; content is the LLM's.**
  Whether a server may sample, elicit, or see roots — and the
  budget/scope it gets — is declared in the agent definition
  and enforced by the runtime (injection-safe; the LLM cannot
  talk its way into spend or paths). *Answering* an
  elicitation, or choosing to use a prompt, is the agent LLM's
  job. This split is the spine of Steps 5–6.
- **Nothing by default.** Each new inbound capability
  (sampling, roots, elicitation) is off unless the agent
  definition grants it, consistent with ADR-0010 / ADR-0013.
- **Test-driven, against the reference server.** Every step
  starts from a failing test that asserts spec-correct
  behaviour observed from the pinned everything-server
  version. Exact server-side names (tool/prompt/resource IDs)
  are confirmed against the pinned version when the failing
  test is written, not guessed here.
- **Pin the everything-server version.** Step 2 pins an exact
  `@modelcontextprotocol/server-everything@<v>` so the TDD
  oracle is stable. The `require_npx()` skip guard stays for
  local-dev ergonomics, but CI must have node so the tests
  actually run (a skipped MCP test is not a passing one).
- **No `#[ignore]` escape hatch.** Same discipline as prior
  plans: a bug surfaced in a step is fixed in that step.

## Implementation Steps

Each step: write the failing everything-server test first,
then implement until green; full `cargo test -p fq-runtime`
+ clippy + fmt before the per-step commit.

### Step 1 — ADR: autonomous resolution of MCP human-in-the-loop primitives

**Goal.** Ratify the invariant and the capability model
before any server-initiated handler is built. This is the
design gate Steps 5–6 depend on; it is the one
non-test-first step.

**Output.** `docs/adrs/accepted/0017-mcp-human-in-the-loop.md`
stating:

- The no-human-at-invocation invariant and its corollary
  (HITL lives at the workflow layer).
- The authorization-vs-content split:
  - *Declarative* (agent definition / sandbox): sampling
    permitted + sub-budget; elicitation permitted; roots =
    workspace scope.
  - *LLM-resolved*: elicitation answers (structured output
    against the server's schema); prompt use (reclassified
    from user-controlled to model-controlled).
- Deny semantics: when a capability is not granted, the
  client returns a structured "unsupported / declined"
  response, never a hang.
- The injection rationale: third-party servers can now
  initiate requests *into* the agent, so the policy layer is
  the only gate and the LLM is never the authorizer.

**Done when**

- [x] ADR-0017 written and marked Accepted.
- [ ] The capability fields it defines are referenced by
      Step 8's agent-definition surface.

---

### Step 2 — `ClientHandler` scaffold + capability negotiation + pinned fixture

**Goal.** Replace the `()` handler with a real `ClientHandler`
that advertises the client capabilities we intend to support,
and turn the ad-hoc `everything_config()` into a pinned,
shared fixture. After this step the initialize handshake
negotiates the right capabilities even though the
server-initiated handlers still return "unsupported".

**Failing test first.** `negotiates_full_capability_set`:
start the pinned everything server, assert the handshake
reports the server's advertised capabilities (resources,
prompts, tools, logging, completions) and that *our*
advertised `ClientCapabilities` include roots / sampling /
elicitation slots.

**Shape.** Introduce `FactorQClientHandler` implementing
`rmcp`'s `ClientHandler`; swap `().serve(transport)` for
`handler.serve(transport)`. Handlers initially return the
spec's "not supported" responses. Pin
`@modelcontextprotocol/server-everything@<v>` in the fixture.

**Done when**

- [x] `()` handler replaced; capability negotiation asserted.
- [x] Pinned fixture shared by `mcp_integration.rs`.
- [x] Existing tools-only tests still green.

---

### Step 3 — Resources (P1)

**Goal.** Discover and read resources, including templates and
live subscriptions, and decide how read content enters agent
context (the gap behind `mcp.rs:99-101`).

**Failing tests first** (against the everything server's
resource set, confirmed at write-time): list resources; read
a resource by URI; read via a resource *template*; subscribe
and receive a `resources/updated` notification; observe a
`resources/list_changed` notification.

**Decided 2026-05-28: both consumption surfaces.** Read content
reaches the agent two ways — a model-controlled read tool *and*
host-curated injection — so the step splits into sub-chunks:

- **3a — Protocol wrappers** (mechanical, shared foundation):
  `list_resources` / `read_resource` / `list_resource_templates`
  on the manager, by server name (mirrors `server_capabilities`).
  Reusable by both surfaces below.
- **3b — Model-controlled read tools** (the *primary* inclusion
  mechanism): host-fulfilled tools the agent's LLM calls on demand
  to **list / read / list-templates** a server's resources
  (`<server>__list_resources`, `__read_resource`,
  `__list_resource_templates`; parallel `McpTool`, per-server).
  The model decides what to read, and resolves templated resources
  (e.g. `calendar://events/{y}/{m}/{d}`) by filling params into a
  concrete URI it reads.
- **3c — Subscribe + notification sink**: `subscribe` plus a
  handler sink for `notifications/resources/{updated,list_changed}`
  (the handler gains state to record/forward them).
- **3d — Static host-curated inclusion** (reshaped 2026-05-29):
  a `static_resources:` frontmatter field naming **concrete**
  `mcp://<server>/<native-uri>` resources the harness always reads
  and injects into the initial prompt at **invocation start**
  (app-controlled guaranteed inclusion, per ADR-0013) — distinct
  from 3b's model-driven discovery, which is the primary path.
  Concrete URIs only: templates can't be statically pinned (their
  params are runtime values), so templated resources go through 3b;
  a declarative template-binding (params from the trigger) is
  deferred. MCP prescribes no inclusion policy — this is one
  honest, documented host policy.

**Failing tests first** (against the everything server's resource
set, confirmed at write-time): list resources; read by URI; list
templates; the read tool returns content; subscribe + receive
`resources/updated`; and a `static_resources` concrete pin's
content appears in the assembled prompt context.

**Done when**

- [x] Protocol wrappers + read/templates tools +
      subscribe/notifications green against the pinned server.
- [x] Read resource content reaches the agent via the tools (done)
      and via `static_resources` host-curated injection (3d).

**Step 3d sub-steps** (all committed):

- **3d-i** ✅ (`fb97944`) — `McpResourceReader`: a cloneable,
  read-only handle (per-server client `Arc`s) the manager hands
  out. **Decision (2026-05-29):** the runner gets read access via
  this handle, *not* by sharing the whole `McpClientManager` — the
  manager keeps its `&mut shutdown()` in `main.rs`; `ReducerContext`
  holds the handle.
- **3d-ii** ✅ (`beb5f19`) — `static_resources:` agent-def field +
  `StaticResourcePin::parse` (`mcp://<server>/<native-uri>`,
  concrete only).
- **3d-iii** ✅ — the wiring:
  1. `ReducerContext` gained `resources: Option<McpResourceReader>`
     via a `with_resources()` builder (optional field — no
     `ReducerRunner::new` signature change).
  2. `StepInput` gained `static_resource_context: Option<String>`;
     the harness `initial_step` injects it as a context message
     after the system prompt. The pure reducer does no I/O — the
     runner reads first and passes content in.
  3. The runner's `run()` reads the agent's `static_resources` pins
     via the handle *before* the step loop (best-effort: a pin that
     fails to read is logged and skipped) and passes the rendered
     content into step 0 only; `run_loop_inner` nulls it on every
     later step and `resume()` never re-injects (pins are already
     in persisted state). Rendering is shared with the read tool
     via `mcp::render_resource_contents`.
  4. `main.rs` wires `mcp_manager.resource_reader()` into both
     `ReducerContext` construction sites; the manager stays alive
     for `shutdown()`.
  5. e2e test `static_resource_pin_appears_in_first_model_request`:
     an agent with a `static_resources` pin sees that resource's
     content in its first model request (FixtureClient mock-LLM +
     the pinned everything server).

---

### Step 4 — Prompts + Completion (P2)

**Goal.** List and fetch server prompts; offer argument
completion. Per ADR-0017, prompts are model-controlled in
factor-q (no human menu).

**Failing tests first.** List prompts; get a parameterised
prompt and assert the returned message sequence; request
`completion/complete` for a prompt argument and assert
suggestions.

**Implementation.** `list_prompts` / `get_prompt` /
`complete` on the manager; handle `prompts/list_changed`.
Represent a fetched prompt as a **reusable seed value** — its
message sequence + bound arguments + server provenance —
rather than eagerly inlining it into the current context.
Under the no-human invariant the natural consumer is a
*subagent seeded by the prompt* (the prompt supplies the
opening transcript; the parent supplies the subagent
definition). In-context injection into the current invocation
is the fallback for agents without a spawn grant.

*Forward-link, not in scope here.* Subagent spawning is
unbuilt backlog (`backlog.md` → Agent concurrency primitives,
resolved 2026-05-28: parent-owned definitions, capabilities
attenuate ⊆ parent, budget reserved from the parent). Step 4
only needs to *produce* the seed value; the future Spawn
primitive consumes it under those rules. One integration point
to expect: the reducer's `phase == Initial ⇒
messages.is_empty()` invariant must relax to accept a seeded
transcript.

**Done when**

- [x] Prompt list/get/completion tests green.
- [x] A fetched prompt is exposed as a reusable seed value
      (messages + bound args + provenance), ready for a future
      spawn to consume.

**Implemented 2026-05-31.**

- Owned, provider-neutral [`crate::prompt`] module: `PromptSeed`
  (server + name + bound `arguments` + `messages`) with a **capture
  layer** ([`PromptContent`], a 1:1 mirror of the 2025-11-25
  `ContentBlock` union — text, image, audio, resource_link, embedded
  resource — lossless serde round-trip) and a fallible **handling
  layer** (`to_text` / `to_message` / `to_transcript`; unsupported
  content returns `PromptError::NotImplemented` rather than being
  dropped). rmcp-free; the rmcp→owned conversion lives in
  `crate::mcp` (`prompt_content_from_rmcp`).
- Manager wrappers mirror the resource ones: `list_prompts`
  (rmcp `Prompt`), `get_prompt` → owned `PromptSeed`, `complete_prompt`
  → `CompletionInfo`.
- Tests against the pinned everything server: list (4 prompts),
  parameterised `get` (arg substitution + provenance), embedded **text**
  resource renders, embedded **blob** resource → `NotImplemented`,
  and `department` argument completion. Plus owned-type unit tests
  (lossless round-trip + every NotImplemented arm).

**Two rmcp gaps surfaced; both off the critical path:**

- **Embedded resource in prompts** — rmcp 1.4 failed to deserialize
  it (the `Resource` variant's `#[serde(flatten)]` was missing
  upstream). Already fixed upstream (`rust-sdk#842` / `#843`);
  resolved here by **bumping rmcp 1.4 → 1.7**, which flipped the two
  `resource-prompt` tests green.
- **Audio in prompts** — rmcp still omits the `Audio` variant from
  `PromptMessageContent` (through 1.7) and rejects it on the wire, so
  audio prompt content can't reach our capture layer yet. Our owned
  `PromptContent::Audio` is spec-canonical and ready. Reported +
  fixed upstream (`rust-sdk#864` / PR `#865`); tracked in
  `docs/plans/backlog.md`.

*`prompts/list_changed` handling* is folded into Step 7 (utilities /
`list_changed` cache refresh) rather than duplicated here.

---

### Step 5 — Sampling (P3) — the careful one

**Goal.** Answer `sampling/createMessage` from a server by
running a completion on the agent's model, **within declared
policy and budget, emitting events like any other LLM call.**

**Failing tests first** (driven by the everything server's
sampling tool, with `mock_anthropic` as the agent model):
1. *Permitted path*: server requests sampling; handler runs it
   on the mock model; asserts the response is returned, an
   `llm.request`/`llm.response` pair is emitted, and cost is
   charged to the invocation budget.
2. *Budget gate*: a request that would exceed the declared
   sub-budget is denied with a structured error; no model call
   happens.
3. *Not permitted*: an agent without the sampling grant gets a
   declined response; no model call.

**Implementation.** Per
[ADR-0018](../../adrs/accepted/0018-mcp-server-initiated-execution.md)
(the execution model for server-initiated calls): the server runs
as a **per-invocation** instance; `create_message` on the handler
is a thin bridge that forwards over a channel to the **runner**,
which is the sole LLM arbiter. The runner services it via a
`select!` during the tool-call await → gate (grant + sub-budget,
declarative grant from Step 8) → run through the one WAL'd /
evented / budgeted LLM path tagged `origin = sampling{server}` →
**validation seam** → reply. The result returns to the *server*,
not the harness; the reducer is untouched. No human review step;
no LLM authorization step.

**Done when**

- [x] All three sampling tests green (`sampling_permitted_runs_on_the_agent_model`,
      `sampling_over_subbudget_is_declined_without_a_model_call`,
      `sampling_ungranted_is_declined_without_a_model_call`).
- [x] Sampling cost flows through cost controls + event bus (the
      shared `dispatch_llm` path); over-budget and ungranted
      requests deny without calling the model.
- [x] Each sampling completion is attributed to its originating
      server via `origin = sampling{server}` on the emitted
      **cost** event (`CostMetadata`) + `InvocationTotals.sampling_cost`,
      distinct from the agent's own LLM spend (ADR-0004).
      *Narrowed:* origin rides the cost envelope (the attribution
      record); `LlmCallOrigin` is public so spreading it to the
      `llm.request`/`llm.response` trace is a trivial follow-up —
      today they correlate by `call_id`.
- [x] Outbound validation seam in place (ADR-0018): the pluggable
      `ValidatorChain<CreateMessageResult>` on `ReducerContext`
      (default empty / allow-everything) runs on the result before
      reply; `includeContext` is forced to `none` (no context
      injection yet, so nothing to redact). *Deferred to Step 6:*
      the inbound redact chain lands with context injection /
      elicitation schemas.

**Build status / resume anchor (2026-05-31).** Decomposed into
5a/5b/5c; design fully locked (ADR-0018). `main` clean + pushed.

- **5a ✅** (`e5c5fa1`) — validation seam in
  `crates/fq-runtime/src/validation.rs`:
  `ValidatorResult<T> = Allow | Modify(T) | Deny(reason)`,
  `Validator<T>` trait, left-to-right `ValidatorChain<T>`,
  `DefaultAllow`. 5 unit tests green.
- **5b ✅** (`7e344e3`) — the handler→runner bridge, *no runner
  changes*: `ServerRequest::Sampling { params, reply:
  oneshot<Result<CreateMessageResult, rmcp::ErrorData>> }`;
  `FactorQClientHandler::create_message` is now a thin bridge —
  forwards on a per-invocation channel, awaits the oneshot, returns
  it; declines with `method_not_found` when no channel is wired
  (shared tool-only server) or no listener. New
  `McpClientManager::start_server_with_requests` is the
  per-invocation start path (no dedup) that wires the channel and
  returns the receiver the runner will `select!` on; both paths
  share `start_inner`. Bridge integration test
  (`sampling_request_bridges_to_the_host`) drives the everything
  server's `trigger-sampling-request`, drains the channel + replies
  canned, asserts the tool completes. rmcp added as dev-dependency
  for the wire reply type.
- **5c ✅** (`f85a965`) — the runner surgery: `SamplingGrant`
  on `Agent` (`agent.rs`, mirrors `static_resources`); the
  tool-dispatch await in `run_tool` is now a biased `select!` over
  {tool result, `ServerRequest`}, fed a per-invocation
  `SamplingChannel` threaded through `run_with_server_requests`
  (`run` delegates with `None`; `resume` never services sampling —
  ADR-0018 §5). `dispatch_llm` extracted as the shared LLM core;
  `run_model_with_llm` is the agent-turn wrapper. `handle_sampling`
  = gate (grant → server permitted → sampling sub-budget →
  invocation budget) → `dispatch_llm` tagged
  `origin = sampling{server}` → outbound `ValidatorChain` → reply;
  policy refusal / model failure declines to the server, agent turn
  untouched. `origin` attribution on `CostMetadata` +
  `InvocationTotals.sampling_cost` (`events.rs`). 3 e2e policy tests
  green. **Remaining wiring (follow-up / Step 8):** production daemon
  doesn't yet start grant-bearing servers per-invocation and pass the
  `SamplingChannel` (the mechanism + grant + tests land; the daemon
  lifecycle switch to `start_server_with_requests` for granted
  servers is the open piece). Single granted channel for now;
  multi-server merged stream is a follow-up.

rmcp facts (1.7): `create_message(&self, params:
CreateMessageRequestParams, ctx) -> Result<CreateMessageResult,
McpError>`; `CreateMessageResult { model, stop_reason,
message: SamplingMessage }`; `McpError::method_not_found::<…>()` is
the decline. Per ADR-0018 only grant-bearing servers go
per-invocation; tool-only servers may stay shared.

---

### Step 6 — Roots + Elicitation (P4)

**Goal.** Expose workspace roots to servers (declarative), and
answer elicitation autonomously via the agent LLM (or decline
per policy).

**Failing tests first.**
- *Roots*: advertise roots derived from the sandbox fs grant;
  assert the everything server receives them on `roots/list`, and
  that an automated trigger fires `roots/list_changed`.
- *Elicitation (granted)*: server requests structured input;
  produces a schema-valid value via `mock_anthropic`, returns
  `accept` with content, attributed to the server.
- *Elicitation (not granted / over-budget / retries-exhausted)*:
  returns `decline`; for ungranted, no model call.

**Implementation.** Per
[ADR-0018](../../adrs/accepted/0018-mcp-server-initiated-execution.md):
both inherit per-invocation instances, the grant model, and the
bidirectional validation seam. They differ in *arbitration*:

*Roots — handler-only, no LLM, no budget.* `list_roots` returns
invocation-scoped config; it never touches the runner.
- **Source: derived from the agent's sandbox filesystem grant**,
  with the invariant **advertised roots ⊆ sandbox fs boundary**
  (narrowable, never wideable — can't advertise a path you don't
  enforce). `file://` only for v1; other URI schemes later.
- **Boolean per-server grant**, nothing by default.
- **Advisory, not enforcement** — roots tell a cooperative server
  its intended scope; the sandbox / ADR-0010 proxy is the wall.
- Advertised through the **outbound `Validator` chain** (default
  `DefaultAllow`).
- **`roots/list_changed`**: expose the mechanism (update roots +
  `notify_roots_list_changed`); the real dynamic-workspace trigger
  defers to the "Workspace state" backlog item. Covered by an
  automated test that invokes the trigger programmatically.

*Elicitation — sampling-shaped (runner-arbitrated).* Same
`select!` path / budget / events / validation seam as Step 5; only
the answer stage differs:
- **Schema-constrained structured output** via a **reusable runner
  "structured completion against a schema" primitive**: validate
  against `requested_schema` → **bounded retry** (default N=2, each
  retry a budget-counted LLM call) → exhausted ⇒ `Decline`. The
  same primitive later serves the sampling evaluator-validator and
  the backlog's spawn-deliverable typing.
- **Action mapping**: `Accept` (valid value) / `Decline`
  (ungranted, over-budget, or retries exhausted — refuse but the
  server continues) / `Cancel` (reserved for genuine invocation
  abort, not policy refusal).
- **Restricted schema subset**: enforce MCP's flat-object /
  primitive-enum elicitation schema; decline anything outside it.
- **The schema is a named extraction channel** (sharper than
  sampling's free-form output): the **inbound seam inspects the
  schema's field names + message** (a server can request
  `{ api_key: string }` and coax the model to fill it), and the
  **outbound seam censors the structured value**.

Sampling + elicitation unify under one server-initiated request
path (`ServerRequest { Sampling | Elicitation }`, one channel, one
`select!` arm); only the answer stage differs.

**Done when**

- [x] **6a** (`c8eaa4c`) — Roots advertised (⊆ sandbox fs grant via
      `roots_from_sandbox`/`advertised_roots`); `roots/list` +
      automated `roots/list_changed` tests green (oracle: the
      everything server's `get-roots-list`). `RootsGrant` on `Agent`;
      `RootsHandle::set_roots` exposes the `list_changed` mechanism.
- [x] **6b** (`4bfec85`) — Elicitation: granted→`accept` (schema-valid,
      round-trips back through the server), ungranted→`decline` (no
      model call), over-budget→`decline` (no model call),
      retries-exhausted→`decline`; all green.
- [x] The elicitation-answer LLM call is attributed
      `origin = elicitation{server}` on the cost event +
      `InvocationTotals.elicitation_cost`, distinct from agent-turn
      spend.
- [x] Bidirectional validation seam wired for both: roots outbound
      (`ValidatorChain<Vec<Root>>` in `advertised_roots`); elicitation
      inbound (`CreateElicitationRequestParams`) + outbound (`Value`)
      chains on `ReducerContext`, reusing the ADR-0018 `Validator`
      chain. All default-empty (allow); concrete validators are a
      later policy-surface concern (Step 8).

**Remaining wiring (follow-up / Step 8), same as Step 5c:** the
production daemon doesn't yet start grant-bearing servers
per-invocation and pass the `SamplingChannel` / `RootsHandle` /
advertised roots — the mechanisms + grants + tests land here; the
daemon lifecycle switch to `start_server_with_requests` for granted
servers is the open piece. Elicitation schema validation is v1
(object shape + required-field presence); deeper per-field type
checking is a refinement.

---

### Step 7 — Utilities: notifications, progress, cancellation, pagination, logging

**Goal.** The cross-cutting machinery that makes the client
robust against real servers.

**Failing tests first.** Progress: call the everything
server's long-running-operation tool, assert N
`notifications/progress` received against the progress token.
Cancellation: cancel an in-flight request and assert it
aborts. Pagination: drive a paginated `list` and assert all
pages are walked via the cursor. Logging: set a log level and
assert `notifications/message` records arrive.

**Implementation.** Progress + cancellation plumbing;
cursor-following on every `list`; a `notifications/message`
sink folded into `tracing` / the event bus; ensure
`*_list_changed` handlers from Steps 3–4 refresh cached
capability lists rather than serving stale data.

**Done when**

- [x] Progress, cancellation, pagination, logging tests green.
      Commits: `f18d1e1` (7a logging — notification backbone +
      `on_logging_message` + `set_logging_level`), `4a646d7` (7b
      progress — per-call progress token + `on_progress`), `b5bcd02`
      (7c/7d/7e — `refresh_tools` + list_changed forwarding,
      `call_tool_cancellable` via rmcp `send_cancellable_request`,
      pagination already via `list_all_*`).
- [x] `list_changed` refreshes cached lists: `refresh_tools`
      re-discovers the cached tool list (the only cached one — the
      manager's `tool_names`); resources/prompts are fetched
      on-demand via `list_all_*` (never cached → never stale).
      `on_tool_list_changed`/`on_prompt_list_changed` forward
      notifications so a consumer reacts.

**Notes / deferred (same theme as 5c/6):** the production daemon
doesn't yet drive the notification→action loops — react to a
forwarded `ToolListChanged` by calling `refresh_tools` and
re-registering into the live `ToolRegistry`; surface progress/logging
to an operator; cancel in-flight calls on invocation abort. The
mechanisms, the unified `ServerNotification` sink, and the
`call_tool_cancellable` / `refresh_tools` / `set_logging_level`
methods all land here with tests; wiring them into the daemon's event
loop is the remaining piece. A dedicated multi-page pagination test
and a server-driven `list_changed`→refresh test want a mock
paginating/mutating MCP server (test-infra follow-up); pagination
itself is correct (cursor-following `list_all_*`).

---

### Step 8 — Agent-definition + sandbox capability surface

**Goal.** Make ADR-0017's grants declarable and enforced
*end-to-end*. The grant **types** and runtime **gates** already
exist (`SamplingGrant` / `RootsGrant` / `ElicitationGrant`, set
programmatically; the Step 5/6 gates consume them, and Step 7's
mechanisms — `start_server_with_requests`, `SamplingChannel`,
`RootsHandle`, `call_tool_cancellable`, `refresh_tools` — are in
place). Step 8 makes them (a) **declarable** in agent frontmatter
and (b) actually **fire in the daemon's invocation path** (Steps
5c/6/7 deferred the production wiring).

**Failing tests first.** An agent def granting sampling +
elicitation produces a handler that advertises and honours
them; an agent def granting neither advertises neither and
denies both; roots scope reflects the declared workspace; a
**full daemon-path invocation** against a grant-bearing server
performs a sampling / elicitation / roots exchange (not just the
direct `run_with_server_requests` test path).

**Implementation.** Parse the new fields in `agent/definition.rs`
(frontmatter → `SamplingGrant` / `RootsGrant` / `ElicitationGrant`
incl. sampling/elicitation sub-budgets); thread them through the
Agent builder + `ConfigSnapshot`. Then close the production-wiring
gap left by Steps 5c/6/7: the daemon must start **grant-bearing
servers per-invocation** (`start_server_with_requests`) and thread
the resulting `SamplingChannel` + `RootsHandle` + `advertised_roots`
into `run_with_server_requests`, so the gates run in a real
invocation, not only under test.

**Done when**

- [x] **8a** (`9d97d09`) — Sampling / elicitation / roots grants parse
      from frontmatter (per-server flags + aggregate sub-budgets),
      round-trip through the Agent builder + `ConfigSnapshot`.
- [x] **8b** (`c98cde7`) — Grants drive advertised capabilities
      (`AdvertisedCapabilities` per server); ungranted capabilities are
      neither advertised nor honoured (a server gates its
      server-initiated tools on what the client advertises, so it
      registers none of them — proven by
      `advertised_capabilities_gate_what_the_server_registers`).
- [x] **8c** (`cc19c43`) — `ReducerRunner::run` starts grant-bearing
      servers per-invocation, derives caps + advertised roots from the
      grant, layers their tools onto the base registry, and wires the
      `SamplingChannel` — a full `run`-path sampling exchange works
      end-to-end (`run_auto_starts_a_grant_bearing_server_and_samples`),
      not just the direct `run_with_server_requests` test path. fq-cli
      boot skips grant-bearing servers (they run per-invocation).

**Remaining (backlog, non-blocking):** multi-server server-initiated
support (v1 wires one grant-bearing server per invocation); the
validation seam is wired default-allow (config-driven validators are
the "concrete validators" backlog item).

---

### Step 9 — Documentation, close

**Goal.** Document the full-spec client and the autonomous
HITL model; close the plan.

- [x] `ARCHITECTURE.md` MCP section updated (no longer tools-only;
      full capability set + autonomous-HITL + per-invocation model).
- [x] Agent-definition guide (`docs/guide/agent-definitions.md`) gains
      an MCP section: `mcp:` servers, resource tools, `static_resources`,
      and the capability grants + sub-budgets + nothing-by-default.
- [x] New MCP guide (`docs/guide/mcp.md`): what's supported, the
      no-human invariant, how to grant capabilities, worked examples,
      current limits.
- [x] ADR-0005 notes the added frontmatter fields (points to the live
      guide); new ADR index (`docs/adrs/README.md`) — also flags the
      `0014` number clash + unfiled root ADRs for a cleanup pass.
- [x] Example agent `agents/examples/mcp-grants.md` demonstrates the
      grants end-to-end (validated by the registry example-load test).
- [ ] Move this plan to `docs/plans/closed/` with a closing summary +
      commit list. (Ready: Steps 1–8 done, docs landed; the remaining
      items are backlog, non-blocking. Deferred to a deliberate close.)

## Cross-cutting concerns

- **No-human invariant holds in every handler.** No
  server-initiated path may block waiting on a human; it
  resolves autonomously or returns a structured decline.
- **Sampling is a trust boundary.** It spends the agent's
  budget on a third party's behalf — it routes through cost
  controls and emits events, always.
- **Bugs surfaced get fixed in the same step.**
- **Docs land with the code** where a step changes
  user-facing behaviour; Step 9 covers the rest.
- **CI must run these.** node on CI so `require_npx()` doesn't
  silently skip the whole suite.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| `rmcp` 1.4 doesn't expose some client method we assume | A step's implementation can't find the API | Verify per step against docs.rs; bump rmcp or upstream a fix. Caught early because tests are written first. |
| Server-initiated requests are an injection surface | A test server coerces spend/paths | ADR-0017's declarative gate; LLM never authorizes; Step 8 enforces grants. |
| everything-server version drift changes the oracle | Tests break on a server upgrade | Pinned version (Step 2); upgrades are deliberate. |
| CI lacks node and silently skips MCP tests | Green CI with zero MCP coverage | Make node a CI prerequisite; consider failing if `require_npx()` skips under a CI env flag. |
| Elicitation-via-LLM produces schema-invalid output | Step 6 test flakes | Validate against the requested schema; `decline` on repeated failure rather than loop. |

## Closing condition

This plan closes when:

- All 9 steps' "Done when" boxes are ticked.
- The client negotiates and exercises the full 2025-11-25
  surface against the pinned everything server; full suite +
  clippy + fmt clean.
- ADR-0017 is Accepted and the agent-definition docs describe
  the capability grants.
- This plan moves to `docs/plans/closed/`.

## Design references

- `services/fq-runtime/crates/fq-runtime/src/mcp.rs` — the
  tools-only client this plan extends.
- `services/fq-runtime/crates/fq-runtime/tests/mcp_integration.rs`
  — existing everything-server TDD harness.
- [ADR-0013](../../adrs/accepted/0013-memory-as-mcp-service.md)
  — memory as MCP; the downstream consumer of resources.
- [ADR-0010](../../adrs/accepted/0010-agent-execution-isolation.md)
  — isolation / nothing-by-default; basis for capability
  grants.
- MCP spec 2025-11-25; `@modelcontextprotocol/server-everything`.
