# ADR-0018: Execution Model for Server-Initiated MCP Calls

## Status
Accepted (2026-05-31)

## Context

[ADR-0017](./0017-mcp-human-in-the-loop.md) settled the *policy*
for MCP's server-initiated primitives — **sampling**
(`sampling/createMessage`), **elicitation** (`elicitation/create`),
and **roots** (`roots/list`): resolve autonomously, authorize
declaratively, the runtime is the only gate, nothing by default.
It did not settle the *architecture*: where these calls run, who
arbitrates them, and how the trust boundary they open is enforced
in the runtime. Sampling forces those questions first (it is the
one that spends the agent's budget on a third party's model
request), so they are settled here before Step 5 of the full-spec
plan is built. Elicitation (Step 6) reuses the same model.

Three properties of the current runtime make this non-trivial:

- **The MCP handler is created at server start, not per
  invocation.** `FactorQClientHandler` (`mcp.rs`) is built when a
  server connection is opened. In the daemon, `McpClientManager`
  opens connections once and shares them across invocations. rmcp
  delivers `create_message(&self, params, context:
  RequestContext<RoleClient>)`; that `context` carries the rmcp
  peer and request id, **not** factor-q's invocation. So a shared
  handler cannot tell which agent's budget, model, or grant a
  sampling request belongs to.
- **The LLM, budget, event chain, and recovery WAL live in the
  runner**, not the handler. A sampling request that did its own
  LLM call from the handler would bypass cost controls, the event
  bus, and the §5.5 write-ahead log that makes invocations
  recoverable.
- **Server-initiated requests arrive mid-tool-call.** A server
  asks to sample *because* the agent called one of its tools; the
  request lands while the runner is parked `await`-ing that tool
  result. The invocation is therefore unambiguous, but the runner
  loop is paused at the await.

And one property of the wider ecosystem: **third-party MCP servers
are not reliably idempotent.** We can build ours to be; we cannot
assume it of servers we integrate. A server instance carrying
request-scoped state cannot be safely shared across concurrent
invocations.

## Decision

### 1. Server-initiated-capable MCP servers run per invocation

Any MCP server **granted an inbound capability** (sampling,
elicitation, roots) runs as a **per-invocation instance** — its
own child process and connection, started when the invocation
starts and torn down when it ends. This is the only model that
attributes a server-initiated request to the correct invocation's
budget, grant, and event chain, and it is isolation-correct per
[ADR-0010](./0010-agent-execution-isolation.md): a server process
shared across agents is a cross-agent boundary violation that
tools-only usage let us ignore. Non-idempotent third-party servers
make it mandatory regardless.

Pure tool-only servers (no inbound grant) **may** remain shared as
an optimization; the per-invocation requirement is grant-driven,
not blanket.

### 2. The runner is the sole arbiter of LLM calls

**Every LLM call — agent turns and sampling alike — flows through
the runner's single path** (`run_model_with_llm`): WAL
intent→dispatched→completed, `llm.request`/`llm.response` events,
pricing, budget. Sampling is that path with a different *origin*
and a different *destination*; it is **not** a reducer action (the
reducer decides agent turns; sampling is the server's, arbitrated
by the host), so the reducer stays pure.

The handler becomes a **thin bridge**. On `create_message` it
translates the request to a `SamplingRequest` + a oneshot reply
channel, sends it on a per-invocation channel, and awaits the
reply. The runner services it by turning the tool-dispatch await
into a `select!`:

```
while awaiting a tool result, also service inbound
server-initiated requests:

  select! {
     tool_result = tool.execute(...)  => return to reducer
     Some(req)   = server_rx.recv()   => handle_sampling(req)
  }
```

`handle_sampling`:
1. **gate** — granted? within the declared sub-budget and the
   invocation total? If not → reply a structured decline, **no
   model call** (ADR-0017: the runtime is the only gate).
2. **run** through the shared LLM path → WAL'd (hence in runner
   state and **recoverable**), evented, cost charged, tagged
   `origin = sampling{server}`.
3. **validate** the result (§4) → reply the final, possibly
   censored, result or a decline.

This keeps sampling in the runner's recovery WAL, makes the runner
the single budget/cost chokepoint, and resolves the parked-runner
problem without a second LLM caller.

### 3. The sampling result returns to the server, not the harness

Per the protocol, `sampling/createMessage` returns
`CreateMessageResult` **to the requesting server**, which consumes
it to continue its work. The result does **not** enter the agent's
harness transcript; the reducer never sees it. The runner runs and
records the call (budget, events, WAL); the *content* crosses
outward to the server.

This is precisely why the validation seam (§4) is a security
boundary and not a nicety: it is the one place an output of *our*
model — which may have seen the agent's context and secrets —
crosses to an untrusted third party.

### 4. A bidirectional, pluggable validation seam

Server-initiated calls pass through validation on **both** sides:

- **Inbound** — what the server is allowed to put *into* the
  sampling request. `includeContext` (`none | thisServer |
  allServers`) lets a server ask us to inject agent/MCP context
  into its prompt; it **requires explicit grant, defaults to
  `none`**, and even when granted the injected context passes
  through an inbound validate-and-redact chain.
- **Outbound** — the model result before it returns to the server
  (censor secrets, evaluate for leakage, etc.).

Both seams are the same shape: an **ordered chain of pluggable
validators**, each returning a reusable

```
ValidatorResult<T> = Allow | Modify(T) | Deny(reason)
```

The chain composes left to right: `Allow` passes the value
unchanged to the next validator, `Modify` carries the transformed
value forward, `Deny` short-circuits to a structured decline. An
empty chain (the default, `DefaultAllow`) passes everything.
`ValidatorResult` is generic and reused elsewhere (elicitation in
Step 6, and any future input/output gate), not sampling-specific.

The chain runs **inside the runner**, so its decisions are
auditable (a validator may emit an event) and cannot be bypassed
by the handler. Concrete validators — e.g. a `HighEntropyRedactor`
or a `ValidateRequestPolicy` — are added to the chain without
touching the seam. Which validators ship beyond `DefaultAllow` is
a later scoping call (Step 8's policy surface); the seam and the
default land with sampling.

Elicitation sharpens why the inbound seam is non-optional: its
`requestedSchema` is a **named extraction channel** — a server can
ask for `{ api_key: string }` and coax the model to fill it from
context, where sampling's output is merely free-form. So the
inbound chain inspects the schema's field names and the message,
not just the injected context.

### 5. Recovery

A server-initiated LLM call in flight is WAL'd like any other. A
worker crash mid-tool-call leaves it in the ambiguous `dispatched`
state along with the tool call that triggered it; recovery does
**not** auto-replay it — it surfaces via `fq recover` for operator
triage (§3.4 contract), because the per-invocation server
connection died with the crash and cannot receive a replayed
result. No separate recovery story for sampling.

## Consequences

- **Cost attribution (ADR-0004).** The WAL row and the
  `llm.request`/`llm.response`/cost events gain an `origin`
  (agent-turn vs `sampling{server}`), so sampling spend is
  distinct from the agent's own turns and bounded by the same
  invocation budget (the sampling sub-budget is one line item
  inside the invocation total).
- **Loop change.** The tool-dispatch await becomes a `select!`
  over {tool result, inbound server-initiated request}. This is
  the one structural change to the runner loop.
- **Lifecycle change.** Granted servers move from
  daemon-start connection to invocation-start connection;
  `McpClientManager` grows a per-invocation mode. Tool-only
  servers are unaffected.
- **New types.** `ValidatorResult<T>`, a `Validator` trait, and a
  `ValidatorChain`; the per-invocation request channel and a
  unified `ServerRequest { Sampling | Elicitation }`; and a
  **reusable runner "structured completion against a schema"
  primitive** (validate → bounded retry → decline). Elicitation
  consumes it first; the sampling evaluator-validator and the
  backlog's spawn-deliverable typing reuse it.
- **Scope.** This model is the foundation for Step 5 (sampling)
  and Step 6 (elicitation). Roots is read-only config (no LLM
  call) but inherits the per-invocation + grant model.
- **Reducer untouched.** Sampling is host-driven and never reaches
  the reducer, so the reducer's purity and the
  `phase == Initial ⇒ messages.is_empty()` invariant are
  unaffected.

## References

- [ADR-0017](./0017-mcp-human-in-the-loop.md) — the policy this
  implements (autonomous resolution, declarative authorization,
  runtime-is-the-gate, nothing-by-default).
- [ADR-0010](./0010-agent-execution-isolation.md) — isolation,
  which per-invocation server instances satisfy.
- [ADR-0004](./0004-cost-controls-from-day-one.md) — cost
  attribution and the budget-subtree bound that sampling spend
  flows through.
- [MCP full-spec plan](../../plans/active/2026-05-28-mcp-client-full-spec.md)
  — Step 5 (sampling) and Step 6 (roots + elicitation) build on
  this.
