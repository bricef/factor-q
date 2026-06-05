# Plan: Finish the MCP integration (hardening, config-driven validation, Streamable HTTP)

**Date**: 2026-06-04
**Status**: Active
**Owns the deferred tail of**:
[`2026-05-28-mcp-client-full-spec.md`](2026-05-28-mcp-client-full-spec.md)

## Goal

The full-spec plan brought factor-q's MCP client to the 2025-11-25
surface **over stdio**, with server-initiated sampling / elicitation /
roots resolved autonomously (ADR-0017 / ADR-0018). It shipped each
capability as *mechanism + grant + tests* and deliberately deferred a
tail of hardening, attribution, daemon-wiring, validation, and one
transport. This plan clears that tail to a genuinely finished state:

1. **No built-but-dead code.** Every shipped mechanism actually fires
   and enforces in the running daemon — notifications are drained and
   acted on, cancellation has a trigger, validators are populated.
2. **Config-driven capability validation.** Grants gain declarative
   guardrails (redaction, request-policy, evaluator) declared *alongside
   the grant*, for the capabilities we have today.
3. **Streamable HTTP transport.** The client speaks the spec-current
   remote transport — required for full 2025-11-25 compliance — with a
   running-service test strategy.
4. **Deferred features captured.** Genuinely forward-looking work is
   explicitly parked with rationale and a pointer, not silently dropped.

On completion, **both** this plan and the 2026-05-28 full-spec plan
move to `docs/plans/closed/`. Completing this plan **closes the MCP
pillar of Phase 2**; the remaining Phase 2 pillars — Memory, Skills, the
shared vector DB, and context-window management — are separate, deferred
plans.

## Context — where we are (grounded 2026-06-04)

The full-spec plan's Steps 1–9 are functionally complete. The tail it
left, verified against the current tree:

- **Validation seam is built but empty.** `validation.rs` has the full
  machinery (`Validator<T>`, `ValidatorChain<T>`,
  `ValidatorResult = Allow | Modify | Deny`). All four chains ship
  default-allow: `sampling_validators` (`CreateMessageResult`),
  `elicitation_inbound_validators` (`CreateElicitationRequestParams`),
  `elicitation_outbound_validators` (`Value`) on `ReducerContext`
  (`runner.rs:117–126`), and roots-outbound is passed a fresh
  `ValidatorChain::new()` at `runner.rs:387`.
- **Daemon mechanisms exist but are never invoked.** `refresh_tools`,
  `call_tool_cancellable`, `set_logging_level` (`mcp.rs:1093 / 1276 /
  1335`) are called nowhere outside `mcp.rs`. Nothing drains the
  `ServerNotification` channel (`recv_notification` exists, no caller).
- **Structured completion is inline.** The parse → schema-validate →
  bounded-retry → decline loop lives inside `handle_elicitation`
  (`runner.rs:2140–2183`); `validate_against_elicitation_schema`
  (`runner.rs:2345`) checks only object-shape + required-field presence.
- **Single server-initiated channel.** `run_with_server_requests` takes
  one `SamplingChannel` (`runner.rs:456`); one grant-bearing server per
  invocation.
- **Origin rides only the cost event.** `LlmCallOrigin` is on
  `CostMetadata` (`events.rs:510`) but not on the `llm.request` /
  `llm.response` trace.
- **Transport: stdio only.** `Cargo.toml:25` enables `client` +
  `transport-child-process`.

Two enabling facts confirmed for the transport work:

- **rmcp 1.7 already exposes the transports.** Its `[features]` include
  `transport-streamable-http-client-reqwest` (the spec-current remote
  client, reqwest-backed) and `client-side-sse` (legacy SSE). We just
  don't enable them.
- **The pinned everything server serves all three transports.**
  `npx @modelcontextprotocol/server-everything@2026.1.26 {stdio | sse |
  streamableHttp}`. So remote-transport tests reuse the existing TDD
  oracle over the wire, rather than needing a new mock for the happy
  path.

## Scope decisions (2026-06-04)

- **Config-driven validators — IN.** Design + implement declarative
  validation config for the capabilities we have today (sampling /
  elicitation / roots), declared *alongside the capability grant*. See
  [§ Validator configuration design](#validator-configuration-design).
- **Streamable HTTP transport — IN; required for spec compliance.** The
  2025-11-25 remote transport is **Streamable HTTP** (the old HTTP+SSE
  transport was deprecated 2025-03-26). We implement the Streamable
  HTTP **client**. **Legacy SSE is out** (confirmed 2026-06-04): it was
  deprecated 2025-03-26 and is not required for compliance. rmcp's
  `client-side-sse` + the everything server's `sse` mode remain a small
  follow-on only if a real server ever forces it.
- **Deferred — captured, not dropped** (see
  [§ Deferred](#deferred--captured-for-the-future)): `includeContext`
  injection, non-`file://` roots, audio-in-prompts.

## The work (A–E)

The breakdown that motivated this plan, annotated with disposition and
the file each item touches. **In** = this plan; **Deferred** = parked
with a pointer.

### A — Validation (the core of "finished")

- **A1 · Concrete validators + config — IN.** Implement the three named
  `Validator<T>` impls and make them declarable per agent-def:
  `HighEntropyRedactor` (strip secrets / high-entropy strings from
  sampled output and elicited values), `ValidateRequestPolicy` (reject
  coercive inbound elicitation — e.g. a field named `api_key`), and the
  **sampling evaluator-validator** (judge a completion before it returns
  to the server). Populate the four chains from config. The builder
  hooks already exist (`sampling_validators()` etc., `runner.rs:167–185`).
- **A2 · Elicitation schema-validation depth — IN.** Enrich
  `validate_against_elicitation_schema` (`runner.rs:2345`): per-field
  primitive types, format checks (email / uri / date), enum, numeric
  range; **reject** schemas outside MCP's flat-object / primitive subset
  instead of passing them through.
- **A3 · Extract the structured-completion primitive — IN.** Pull the
  inline loop out of `handle_elicitation` into a reusable runner method
  (validate → bounded-retry → decline). A1's evaluator and the future
  spawn-deliverable typing (agent-concurrency backlog) reuse it. **Do
  first** — A1/A2 build on it.

### B — Daemon wiring (make shipped mechanisms fire)

- **B1 · Notification → action loops — IN.** Drain the
  `ServerNotification` channel in the running daemon: on
  `ToolListChanged` call `refresh_tools` **and re-register into the live
  `ToolRegistry`** (the manager refreshes its own `tool_names`; registry
  re-registration is the missing half); cancel in-flight calls on
  invocation abort (`call_tool_cancellable` exists; wire a trigger —
  timeout / budget / shutdown); surface progress / logs to an operator.
- **B2 · Server logs → event bus — IN.** `on_logging_message` folds
  records into `tracing` + the `ServerNotification::Log` sink; the "→
  event bus" half is unbuilt — add an `EventPayload` variant + a
  `schema_id_for` arm (`events.rs:430`) + a JSON schema, then bridge the
  drained `Log` notifications onto NATS. Depends on B1's drain.
  **Confirmed scope (2026-06-04):** emit the bus event **and** wire the
  operator surface (surface MCP server logs / progress through the
  operator event consumer / CLI); the broader operator-observability
  effort is tracked in `backlog.md` → *Observability*.
- **B3 · `origin` on the llm trace — IN.** Spread `LlmCallOrigin` onto
  the `llm.request` / `llm.response` payloads (today only on
  `CostMetadata`). Mechanical: a field + construction sites.

### C — Generalizations

- **C1 · Multi-server server-initiated channel — IN.**
  `run_with_server_requests` takes a single `SamplingChannel`; merge
  into a server-tagged stream so >1 grant-bearing server per invocation
  works. The `select!` arm + gate already key on server name — a
  contained refactor.
- **C2 · `includeContext` injection + inbound redact — DEFERRED.**
  Sampling forces `includeContext: none`. Honoring `thisServer` /
  `allServers` is a *new* privacy-sensitive feature with no current
  consumer; the inbound redact chain (A1) is the seam it will reuse.
  Confirmed 2026-06-04: **defer, default `includeContext: none`.** A
  compliant client *may* include context; `none` is a valid choice, so
  deferring does not break compliance.

### D — Test infrastructure

- **D1 · Mock paginating / mutating server — IN (scoped).** The
  everything server doesn't paginate and emits `tools/list_changed` only
  once at startup, so a multi-page pagination test and a server-driven
  `list_changed` → `refresh_tools` test can't be written against it.
  Build a small in-process rmcp server (rmcp ships
  `transport-streamable-http-server`) to drive B1's refresh path and the
  pagination cursor. Pagination *implementation* is already correct;
  this closes a coverage gap.

### E — Transport + blocked / premature

- **E3 · Streamable HTTP transport — IN; required.** See
  [§ Streamable HTTP testing strategy](#streamable-http-testing-strategy).
- **E1 · Audio in prompt messages — DEFERRED (upstream).** Our owned
  `PromptContent::Audio` is spec-ready; rmcp omits the variant through
  1.7 (`rust-sdk#865` fixes it). Reachable on a version bump; no
  factor-q change needed until then.
- **E2 · Roots non-`file://` schemes — DEFERRED.** `roots_from_sandbox`
  emits `file://` only; the sandbox *is* a filesystem, so there is
  nothing non-file to point at yet. Owned by the "Workspace state"
  backlog item.

## Implementation steps (ordered)

Discipline from the prior plans: **failing test first** wherever there
is observable behavior; full `cargo test -p fq-runtime` + `--test
mcp_integration` + clippy + fmt before each per-step commit; **no
`#[ignore]`**; a bug surfaced in a step is fixed in that step; commit
per step.

1. **A3** — extract the structured-completion primitive (refactor; tests
   stay green).
2. **A1** — validators + config design/impl (the big one — owns the
   [validator-config design](#validator-configuration-design)). Failing
   tests: a grant declaring redaction strips an injected secret from a
   sampled result; a coercive elicitation request is rejected; an
   ungranted/unconfigured chain still allows.
3. **A2** — elicitation schema-validation depth. Failing tests:
   per-field type / format / enum / range; out-of-subset schema
   declines.
4. **B3** — `origin` on the llm trace. Failing test: a sampling call's
   `llm.request`/`llm.response` carry `origin = sampling{server}`.
5. **C1** — multi-server server-initiated channel. Failing test: two
   grant-bearing servers each get serviced in one invocation.
6. **D1** — in-process mock server (paginating + mutating), used by 7.
7. **B1** — daemon notification → action loops (+ cancellation trigger).
   Failing tests (via D1): a `tools/list_changed` refreshes the live
   registry; an aborted invocation cancels an in-flight call.
8. **B2** — server logs → event bus (new `EventPayload` variant +
   schema). Failing test: a server log surfaces as a bus event.
9. **E3** — Streamable HTTP transport + running-service test harness.
10. **Close** — docs pass (guide + ARCHITECTURE deltas), move **both**
    plans to `docs/plans/closed/` with closing summaries.

## Validator configuration design

The grant is where the capability is turned on; it is the natural place
to declare the capability's guardrails. Today a grant is a bare flag:

```yaml
mcp:
  - server: everything
    sampling: true
    elicitation: true
    roots: true
```

**Proposal:** a flag stays valid (capability on, default guardrails),
*or* it expands to a table of validator policies:

```yaml
mcp:
  - server: everything
    sampling:
      redact_secrets: true     # HighEntropyRedactor on the outbound result
      evaluator: true          # sampling evaluator-validator gates the reply
    elicitation:
      reject_sensitive_fields: true   # ValidateRequestPolicy on the inbound request
      redact_secrets: true            # censor the outbound structured value
    roots: true
sampling_budget: 0.25          # unchanged — sub-budgets stay agent-level
elicitation_budget: 0.10
```

The parser maps each flag to a built-in `Validator<T>` and assembles the
per-capability `ValidatorChain`; the runner installs them per invocation
through the existing builder hooks. **v1 ships the three built-in
validators referenced by name/flag**; a free-form policy DSL or a
named-policy registry is explicitly out of scope (a later concern if
real policies outgrow the flags). `serde` `untagged` (bool | table)
handles the bare-flag / table duality.

**Open design questions** (resolve in step 2, not here): exact flag
names; whether `redact_secrets` is one validator shared across
capabilities or per-capability instances; how a redactor reports a
*modification* (`Modify`) vs a hard `Deny` back through the decline
path. The ConfigSnapshot must capture the resolved policy for
replay/audit (mirror the grant fields).

## Streamable HTTP testing strategy

The constraint the user flagged — "testing requires a running service" —
resolves cleanly because the oracle already speaks the transport.

- **Client feature:** enable `transport-streamable-http-client-reqwest`
  in `Cargo.toml` (additive to `transport-child-process`).
- **Oracle:** the **same pinned everything server**, started in HTTP
  mode — `npx -y @modelcontextprotocol/server-everything@2026.1.26
  streamableHttp` — as a child process listening on a loopback port.
- **Harness:** allocate an ephemeral free port; spawn the child bound to
  it; **poll readiness** (HTTP GET on the endpoint, bounded timeout)
  before connecting; connect the rmcp Streamable HTTP client; run a
  **transport-agnostic** slice of the existing assertions — handshake /
  capability negotiation, list + call a tool, and **at least one
  server-initiated exchange** (sampling) to prove the handler works over
  HTTP, not just stdio; tear the child down on drop.
- **Reuse, don't fork:** factor the transport-agnostic assertions so one
  test body runs over **both** stdio and Streamable HTTP (parameterize
  the transport). This is the strongest signal that the client is
  transport-correct.
- **Sandbox / CI:** these reach **loopback TCP**, so — exactly like the
  NATS-dependent tests — they must run under **direct `cargo`
  (unsandboxed)**; the `just` sandbox blocks loopback the way it blocks
  NATS (see memory: *run cargo tests directly, not just*). Extend the
  `require_npx()` guard to a `require_node_http` analog; node stays a CI
  prerequisite so the suite doesn't silently skip.
- **Legacy SSE (only if in scope):** everything-server `sse` mode +
  rmcp `client-side-sse`, same harness shape.

## Deferred — captured for the future

Forward features, not loose ends. Tracked here **and** cross-linked from
`docs/plans/backlog.md` → *MCP full-spec follow-ups*, so closing this
plan doesn't lose them:

- **`includeContext` injection + agent-context redaction** (C2) — a new
  capability (inject `thisServer` / `allServers` context into a server's
  sampling prompt, censored through the inbound redact chain A1 builds).
  No current consumer; not required for compliance.
- **Roots: non-`file://` URI schemes** (E2) — nothing non-file to
  advertise until a non-file workspace concept exists; owned by the
  "Workspace state: snapshotting and base layers" backlog item.
- **Audio content in MCP prompts** (E1) — blocked on an rmcp release
  carrying `rust-sdk#865`; owned type ready, reachable on version bump.
- **Streamable HTTP auth / session hardening** — v1 targets
  unauthenticated loopback. OAuth (`auth` feature) and resumable
  sessions are a transport-hardening follow-on once a real remote server
  requires them.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| everything-server HTTP mode behaves differently from stdio | A parameterized test passes on stdio, fails on HTTP | Pinned version; assert handshake/capability parity across transports first |
| Validator config surface creeps into a DSL | Step 2 grows a parser for policy expressions | Ship 3 built-ins + flags for v1; DSL explicitly deferred |
| Streamable HTTP drags in session/auth complexity | rmcp HTTP client needs an auth flow to connect | v1 = unauthenticated loopback; auth is a captured follow-on |
| Test port flakiness | Intermittent connection-refused in CI | Ephemeral port + readiness poll + generous timeout; direct-cargo (unsandboxed) |
| Draining notifications races the invocation loop | A refresh mutates the registry mid-step | Re-register at a step boundary, not mid-tool-call; D1 mock makes this testable |

## Closing condition

This plan closes when:

- All **A–E in-scope** items are done; **no shipped MCP mechanism is
  built-but-dead** (notifications drained + acted on, cancellation
  triggered, validators populated from config).
- Capability validators are **declarable per agent-def** and captured in
  the ConfigSnapshot.
- The client passes the full suite over **both stdio and Streamable
  HTTP**; clippy + fmt clean.
- Deferred items are captured here + in `backlog.md`.
- **Both** [`2026-05-28-mcp-client-full-spec.md`](2026-05-28-mcp-client-full-spec.md)
  and this plan move to `docs/plans/closed/`.

## Design references

- [`2026-05-28-mcp-client-full-spec.md`](2026-05-28-mcp-client-full-spec.md)
  — the plan whose deferred tail this finishes.
- [ADR-0017](../../adrs/accepted/0017-mcp-human-in-the-loop.md) /
  [ADR-0018](../../adrs/accepted/0018-mcp-server-initiated-execution.md)
  — the autonomous-HITL + server-initiated execution model.
- `services/fq-runtime/crates/fq-runtime/src/validation.rs` — the
  default-allow seam this plan populates.
- `services/fq-runtime/crates/fq-runtime/tests/mcp_integration.rs` — the
  everything-server harness (`require_npx`, `everything_config`) the HTTP
  tests extend.
- MCP spec 2025-11-25 (Streamable HTTP transport);
  `@modelcontextprotocol/server-everything@2026.1.26`.
