# Architecture Decision Records

ADRs capture significant, hard-to-reverse decisions and the reasoning
behind them. Each is a point-in-time record: it reflects what was
decided when, and isn't rewritten as the system evolves (later ADRs
supersede earlier ones; guides track the current state).

Accepted ADRs live in `accepted/`, drafts in `draft/`, each named
`NNNN-slug.md`.

## Status

Every ADR opens with a Status section, and its status is one of three:

- **Draft** — written down and not yet accepted. A draft lives in `draft/`
  and can still change.
- **Accepted** — decided. An accepted ADR lives in `accepted/`, and its body
  is not rewritten: a later decision gets an ADR of its own.
- **Superseded by ADR-NNNN** — a later ADR replaced the decision. The record
  stays in `accepted/` as it was, with that pointer on its Status line.

The Status line also carries pointers to the records around it: `Refines`
or `Addendum to ADR-NNNN` on an ADR that extends or adjusts a decision that
still stands, and `Refined by ADR-NNNN` on the one it refines; `Partially
superseded by ADR-NNNN` when only part of a decision is replaced; `Amends`
when an ADR changes a clause of another. On an accepted ADR, such a pointer
is the only edit its Status line takes; [AGENTS.md](../../AGENTS.md) has the
rule.

## What an ADR holds

An ADR records a decision: its context, the decision itself, its
consequences, the alternatives considered, and what the decision does not
cover. A sentence saying what the decision leaves out belongs in it; a
sentence saying when the rest will be done does not.

An ADR carries no delivery state. It does not say what is built, shipped,
verified or scheduled, or in which phase, pull request or issue the work
lands, and it holds no implementation, rollout, migration or phase plan.
That state moves with the work, and it lives where it is kept current:

- [STATUS.md](../../STATUS.md) — what runs today, and what is not built yet;
- [MILESTONES.md](../../MILESTONES.md) — what each milestone means, and
  whether it is reached;
- the [issues](https://github.com/bricef/factor-q/issues) and the
  [plans](../plans/README.md) — the work itself.

Until 2026-11-21 every ADR also carried an `Implementation:` line, saying
whether the decision was built. The lines were taken out on the
maintainer's instruction, and what they said that was recorded nowhere
else moved to STATUS.md.

For prose, a design doc's *folder* says whether the thing it describes is
built: [`design/committed/` vs `design/aspirational/`](../design/README.md).

## Accepted

| ADR | Decision |
|---|---|
| [0001](accepted/0001-internal-task-management.md) | Internal task management |
| [0002](accepted/0002-self-hosted-server.md) | Self-hosted server, not a local CLI tool |
| [0003](accepted/0003-model-agnostic-per-agent.md) | Model-agnostic, per-agent model selection |
| [0004](accepted/0004-cost-controls-from-day-one.md) | Cost controls from day one (per-invocation budget; sampling/elicitation sub-budget attribution) |
| [0005](accepted/0005-agent-definition-format.md) | Agent definition format — Markdown + YAML frontmatter |
| [0006](accepted/0006-registry-first-api.md) | Registry-first runtime API — typed operations, CQRS surface, derived interfaces; replaces the prior draft, amends ADR-0031 (Appendix A); NATS is not an external control surface (Appendix C); a synthetic has no Get (Appendix D); `worker.prune` is retired, not evented (Appendix E) |
| [0009](accepted/0009-technology-choices.md) | Technology choices (Rust runtime) |
| [0010](accepted/0010-agent-execution-isolation.md) | Agent execution isolation (containers by default; nothing-by-default sandbox) — the agent-scoped unit of isolation is superseded by [ADR-0028](accepted/0028-tool-scoped-isolation-and-workspace.md) |
| [0011](accepted/0011-event-bus-and-persistence.md) | Event bus and persistence (NATS + JetStream) — persistence/source-of-truth role partially superseded by [ADR-0026](accepted/0026-event-log-system-of-record.md); bus role stands |
| [0012](accepted/0012-graph-definition-format.md) | Execution graph definition format — its subject-wired structure is superseded by [ADR-0007](accepted/0007-inter-agent-communication.md); do not author a graph from it |
| [0013](accepted/0013-memory-as-mcp-service.md) | Memory as an MCP service |
| [0014](accepted/0014-agent-harness-as-reducer.md) | Agent harness as a reducer with a runtime-owned loop |
| [0015](accepted/0015-rust-runtime-polyglot-tools.md) | Rust runtime, polyglot tools, language boundary at the event bus |
| [0016](accepted/0016-typed-operations-no-free-form-apis.md) | Typed operations exposed to agents, no free-form storage APIs — the *shape* agent side effects must take |
| [0017](accepted/0017-mcp-human-in-the-loop.md) | Autonomous resolution of MCP human-in-the-loop primitives (the capability-grant policy) |
| [0018](accepted/0018-mcp-server-initiated-execution.md) | Execution model for server-initiated MCP calls (sampling/elicitation/roots) |
| [0019](accepted/0019-skill-format.md) | Skill format and discovery |
| [0020](accepted/0020-mcp-notification-handling.md) | MCP server notifications — drained in the daemon, tools refresh between invocations |
| [0021](accepted/0021-mcp-cost-control-and-memory-boundary.md) | Cost control for MCP services via `_meta` (budget hint + cost report); memory stays MCP; embedding boundary deferred to the storage design |
| [0022](accepted/0022-binary-distribution-and-licensing.md) | Binary distribution (musl/Apple Silicon release matrix, install.sh, cargo-binstall) and BSL 1.1 licensing |
| [0023](accepted/0023-storage-and-vector-foundation.md) | Storage, extraction, and vector index foundation (Phase 2 pillar #2) |
| [0024](accepted/0024-separate-databases-storage-foundation.md) | Separate databases for the storage foundation's three stores (refines ADR-0023 F9) |
| [0007](accepted/0007-inter-agent-communication.md) | Inter-agent communication — agents never touch the transport; one graph executor with two authoring surfaces (declared graphs + spawn as sugar); per-traversal budget with an ε cost floor; graduated from the April draft |
| [0026](accepted/0026-event-log-system-of-record.md) | A dedicated CAS-backed archive service is the event log's system of record (supersedes ADR-0011's source-of-truth half; NATS becomes transport) |
| [0027](accepted/0027-graceful-drain-deploys.md) | Deploys are a graceful drain — a `fq drain` control command suspends in-flight invocations to a step boundary; the new binary resumes via recovery (not kill-and-replace) (verb folded into `fq down`, #271) |
| [0028](accepted/0028-tool-scoped-isolation-and-workspace.md) | Tool-scoped isolation + a harness-owned virtual filesystem (safe by construction); supersedes ADR-0010's agent-scoped unit of isolation |
| [0029](accepted/0029-fuse-binding-crate.md) | FUSE binding crate for the harness-owned VFS is `fuse-backend-rs` (`fuse3` fallback), chosen by a blind bake-off; refines ADR-0028 |
| [0030](accepted/0030-object-manifest-gc-back-off.md) | Object/manifest GC uses back-off (no generations) — objects get the block protocol's claim CAS + reserve-before-rely, but a writer meeting a claimed object retries rather than minting a generation; TLC-checked, refines ADR-0023 (F2) |
| [0031](accepted/0031-daemon-cli-split.md) | Split the runtime and CLI into `fqd` and `fq` over a typed tarpc control interface; capability-token-over-TLS auth (Appendix A, superseding the original shared secret) as swappable middleware |
| [0032](accepted/0032-exactly-once-trigger-dispatch.md) | A duplicate trigger delivery is refused by a durable claim in the control-plane store, keyed on stream, stream-incarnation epoch and sequence and arbitrated before anything else happens to the delivery; the trigger is still acked at the invocation's first WAL write — accepted 2026-11-11 in the form that landed (#809), not the trigger inbox it was drafted as, which is kept as a deferred design; correct for one daemon and assumes one, with the multi-worker consequences and the condition for reopening written down (#835) |
| [0033](accepted/0033-bsl-reaffirmation.md) | BSL 1.1 stands after the 2026-07-25 cleanroom review's finding 1.4 — reaffirms [ADR-0022](accepted/0022-binary-distribution-and-licensing.md) §7 unamended; costs carried knowingly, the named-scenario gap left open, revisit triggers proposed |
| [0034](accepted/0034-reasoning-as-a-content-part.md) | Reasoning is a content part and `Message` is an enum over turn kinds; reasoning replays within an invocation and is stripped across one; parts stay internal and stop at the operator boundary, which receives a reasoning concept that distinguishes absence from opacity |
| [0035](accepted/0035-container-image-and-compose-supervision.md) | The deployable unit is a container image tagged with its commit, and docker compose is the supervisor and stack definition — no systemd units; a minimal distroless image is the bare-envelope gate and a fat dogfood image layers the agent toolchain on it until the worker / control-plane split; refines [ADR-0027](accepted/0027-graceful-drain-deploys.md), answers #553 |
| [0036](accepted/0036-ops-image-and-scheduler-service.md) | The stack schedules its own operations: an `fq-ops` image carries the deploy, hygiene and backup scripts and their schedule, a compose `ops` service fires each as a one-shot sibling container, and the host crontab and bootstrap's script copy retire; narrows [ADR-0035](accepted/0035-container-image-and-compose-supervision.md) clause 7 to "never into a container that runs the daemon or an agent" |
| [0038](accepted/0038-a-trigger-dealt-with-and-not-run-keeps-its-claim-consumed.md) | A trigger dealt with and not run keeps its claim as `consumed`, with its reason and, beside it, any operator's drop of the run, and the claim is written before the delivery is acked; refines [ADR-0032](accepted/0032-exactly-once-trigger-dispatch.md), whose §4 ("Every other ack gives the claim back") it replaces for those answers |

## Draft

| ADR | Decision |
|---|---|
| [0008](draft/0008-extension-model.md) | Extension and plugin model |
| [0025](draft/0025-storage-gc-observability.md) | Storage GC observability |
| [0037](draft/0037-isolation-sequenced-after-graph-execution.md) | Isolation is sequenced after the first graph-execution pass: the two-node vertical (#414) is built on the phase-1 sandbox, no substrate work (#208, #209) is built standalone before a dedicated design session, credential scoping (#904, #402) is not held, and the session opens once #414 has merged and run one real issue on dogfood — a dated hold with a named exit |

## Related guides

- [Writing Agent Definitions](../guide/agent-definitions.md) — the live frontmatter reference (implements ADR-0005).
- [MCP](../guide/mcp.md) — the MCP capability model (implements ADR-0013/0017/0018).
- [Reducer harness](../guide/reducer-harness.md) — the execution model (implements [ADR-0014](accepted/0014-agent-harness-as-reducer.md)).
