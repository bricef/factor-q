# ADR-0017: Autonomous Resolution of MCP Human-in-the-Loop Primitives

## Status
Accepted (2026-05-28)

## Context

Bringing factor-q's MCP client up to full spec (see
[`2026-05-28-mcp-client-full-spec.md`](../../plans/closed/2026-05-28-mcp-client-full-spec.md))
means honouring MCP's *server-initiated* primitives — **sampling**
(`sampling/createMessage`), **roots** (`roots/list`), and
**elicitation** (`elicitation/create`) — and its user-controlled
**prompts** primitive. Every one of these assumes a human in the
loop in the host application: a person approves a sampling
request, answers an elicitation, grants filesystem roots, or
picks a prompt from a menu.

factor-q runs agents autonomously. A running agent must make the
strong assumption that **no human is available**. So the question
this ADR settles is: how does an autonomous runtime resolve hooks
that the protocol designed around a human?

A second concern compounds the first. These primitives invert the
usual flow — a third-party MCP server initiates requests *into*
the agent: spend its tokens (sampling), query its model
(elicitation), learn its filesystem (roots). That is a new trust
surface. Today the client sidesteps all of it by running with the
no-op `()` service handler (`mcp.rs`), which cannot answer
server-initiated requests at all.

## Decision

**At the invocation level there is no human.** Every MCP
human-in-the-loop hook is resolved autonomously inside the
invocation, or declined — never blocked waiting on a person.
Genuine human-in-the-loop is a **workflow-layer** concern, above
the invocation, and is out of scope here.

Resolution follows one split:

- **Authorization and boundaries are declarative.** Whether a
  server may sample, elicit, or see roots — and the budget/scope
  it gets — is declared in the agent definition / sandbox by the
  agent *author* and enforced by the runtime. **The LLM never
  authorizes** these. Because a third-party server can initiate
  requests into the agent, the runtime policy layer is the only
  gate; letting the model authorize would let a malicious or
  compromised server prompt-inject its way to budget or paths.
- **Judgment and content are the LLM's.** Where a substantive
  answer is required — producing an elicitation response,
  choosing to use a prompt — the agent's own model decides.

**Nothing by default.** Each inbound capability is off unless the
agent definition grants it (consistent with
[ADR-0013](./0013-memory-as-mcp-service.md) and
[ADR-0010](./0010-agent-execution-isolation.md)). An ungranted
request receives a structured "unsupported / declined" response
per the spec; the client never hangs.

### Per-primitive resolution

- **Sampling** — when granted, run the completion on the agent's
  model. The spend draws from the **shared invocation budget**
  (the same pool as the agent's own LLM calls), with the grant
  setting a **ceiling** on sampling's share; cost is tracked and
  emitted as events like any other LLM call
  ([ADR-0004](./0004-cost-controls-from-day-one.md)). Ungranted,
  or over the ceiling, → structured decline with no model call.
  The decision to honour is declarative, not the LLM's.
- **Elicitation** — when granted, the agent's LLM produces a
  schema-valid answer (structured output against the server's
  requested schema), validated before return (`accept` with
  content). Ungranted → structured `decline`, no model call.
  Repeated schema-validation failure declines rather than loops.
  This is the one path where the LLM is the in-loop respondent.
- **Roots** — derived from the agent's *existing* filesystem
  sandbox scope. Roots are a projection of a capability the agent
  already holds, not a new grant, so no separate roots permission
  is required; the LLM never selects roots. Exposed to connected
  servers, with `roots/list_changed` on workspace change.
- **Prompts** — reclassified from MCP's *user-controlled* to
  **model-controlled** in factor-q: the agent may discover
  (`list`) and use (`get`) server prompts; there is no human
  menu. (A fetched prompt is a reusable seed value for a future
  sub-agent spawn — consumer side tracked in the plan and
  [`backlog.md`](../../plans/backlog.md).)

### Cost attribution

Because sampling shares the invocation budget and elicitation
answers are themselves LLM calls, both consume budget on a
server's behalf. Every such call is **attributed** — tagged with
its origin (sampling vs. elicitation) and the originating MCP
server — in the cost events and the invocation trace, per
ADR-0004's cost-attribution rule. A shared invocation budget is
therefore never an opaque total: `fq costs` and the trace always
show where spend went and on whose behalf, distinguishing a
server's sampling/elicitation spend from the agent's own
reasoning.

### Attenuation under delegation

The sampling / elicitation / roots grants are capabilities, so
they **attenuate ⊆ parent** when an agent spawns a sub-agent
(per the spawn decisions in `backlog.md`). The sampling ceiling
is one line item inside the subtree budget bound established by
ADR-0004 — a child can neither sample if its parent cannot, nor
beyond the parent's ceiling.

## Consequences

- The MCP client must implement a real `ClientHandler` (replacing
  the `()` handler) to answer server-initiated requests, and must
  advertise the corresponding client capabilities at handshake.
- Agent definitions / sandbox gain capability fields: sampling
  `{allowed, ceiling}` and elicitation `{allowed}`; roots are
  derived from the existing filesystem sandbox. These fields are
  consumed by the plan's Step 8.
- Sampling cost flows through ADR-0004 cost controls and the
  event bus; ungranted or over-ceiling requests deny without
  calling the model.
- Elicitation answers are schema-validated; the LLM is the
  respondent only when the capability is explicitly granted.
- No invocation-level path ever blocks on a human. Workflow-layer
  human gating (approval nodes, operator pauses) is the future
  home for genuine human-in-the-loop and is deliberately not
  built here.
