# Security Policy

factor-q is alpha software. This page is the canonical summary of its
security posture, and it deliberately keeps two things apart: **what is
enforced today** and **what the project intends to enforce**. The gap
between them is a dated decision, not a gap in attention — the reasoning
and the exit condition are in
[ADR-0037](docs/adrs/draft/0037-isolation-sequenced-after-graph-execution.md),
and every open exposure is a row in the [risk register](#risk-register)
below, bound to the issue that closes it. [STATUS.md](STATUS.md) retains
the operational one-line caveats.

## Posture today

*As of 2026-09-22.* This section describes what the code enforces now. If
it contradicts the code, one of them is wrong — fix whichever it is.

- **Sandbox:** built-in tools are denied by default, and filesystem and
  command working-directory path allowlists are enforced. Agent definitions
  may also declare `sandbox.env` and `sandbox.network`; the environment
  allowlist limits child inheritance and Linux makes direct reads of
  `/proc/<fqd-pid>/environ` root-only, but arbitrary exec can still reach
  secrets by other means, while network declarations are not yet enforced.
  Treat every agent as network-unrestricted regardless of its definition.
  The sandbox dimensions are therefore not independent: an `exec_cwd` grant
  lets a process read and write anything its OS identity can access, inspect
  ambient secrets (including through `/proc`), and make unrestricted network
  requests, regardless of narrower `fs_read`, `fs_write`, `env`, or `network`
  declarations. In practice, exec dominates the other grants; they constrain
  the built-in tools, not programs started by exec. Do not use a narrow
  filesystem or environment declaration alongside exec as an isolation
  boundary. The issues that first identified the gap,
  [#34](https://github.com/bricef/factor-q/issues/34) and
  [#35](https://github.com/bricef/factor-q/issues/35), are closed — #35 by a
  change that added a load-time warning, not enforcement.
- **NATS:** the bundled NATS service requires a static development token. The
  token is committed to this public repository, so it is not a secret: do not
  expose the port beyond the host, and replace the token for any non-local
  deployment (the dogfood stack mints its own at bootstrap).
- **`fq-cas serve`:** the content-store service is localhost-only and
  unauthenticated until M5. Its RPC surface is restricted to `put`, `get`,
  `get_range`, `has`, `size`, and `stats`; GC-only removal and block-inspection
  operations run in-process and are not exposed over the wire.
- **Agent identity:** agent GitHub actions currently use the owner's
  `GH_TOKEN`. A separate identity for the fleet and the watcher is an open
  decision ([#904](https://github.com/bricef/factor-q/issues/904)); per-agent
  attestation is still
  [design work](docs/design/aspirational/agent-identity-and-attestation.md).
- **Dependencies:** every push and pull request runs `just audit` —
  `cargo audit` and `cargo deny` against `deny.toml`, the reviewed advisory
  and licence baseline (one explained ignore per accepted finding, never a
  blanket allow). Dependabot opens weekly update PRs for the Cargo
  workspace, the Go adapters and the workflow actions. The `main-latest`
  binaries the dogfood host pulls are built from a lockfile that has
  passed this gate.
- **The enforced control for the fleet:** the dogfood fleet's unattended
  agents process untrusted public issue text with the owner's ambient,
  write-scoped token and can reach other owner credentials, including model
  API secrets, through exec. Restrictions against operations such as pushing
  or merging are prompt instructions, not typed or runtime-enforced
  controls. For that chain the **human review and merge gate is the sole
  enforced control** before agent-produced changes reach the protected
  branch (four required CI checks, admins enforced, no automatic merge; the
  merge verdicts of
  [#879](https://github.com/bricef/factor-q/issues/879) are advisory and are
  not a control). Treat the fleet as privileged automation with
  attacker-controlled input, not as sandboxed execution.

## Intended posture

This is the posture the accepted decisions describe. None of it is built,
and the folder rule in [docs/design](docs/design/README.md) applies: a
decision can be accepted and still unbuilt.

- **Isolation:** containers by default and microVMs for untrusted models or
  production credentials
  ([ADR-0010](docs/adrs/accepted/0010-agent-execution-isolation.md)),
  applied per tool behind a harness-owned workspace
  ([ADR-0028](docs/adrs/accepted/0028-tool-scoped-isolation-and-workspace.md)),
  with a network proxy at the namespace edge as the single enforcement
  point for `sandbox.network`, and secrets delivered per call rather than
  inherited by every child. The design is settled in a dedicated session
  that starts from the
  [isolation discussion context](docs/design/aspirational/agent-isolation-discussion-context.md)
  and its eight open questions.
- **Identity:** the fleet and the watcher act under their own GitHub
  identity with the narrowest permissions each role needs
  ([#904](https://github.com/bricef/factor-q/issues/904)), and the backlog
  groomer holds an issues-only token
  ([#402](https://github.com/bricef/factor-q/issues/402)); cryptographic
  attestation builds behind that and is gated on isolation.
- **Sequencing:** isolation is deliberately sequenced *after* the first
  graph-execution pass. The decision, its reasons, what it does and does
  not hold, and the exit condition that reopens the design are recorded in
  [ADR-0037](docs/adrs/draft/0037-isolation-sequenced-after-graph-execution.md).
  Until that exit condition is met, every row in the register below is
  open **by decision**.

## Risk register

Each row is an exposure that exists today, the control that removes it,
and the open issue that tracks that control. A row leaves this table only
when its issue closes with the control in place; the *as of* date above is
refreshed whenever a row changes.

| # | Exposure today | Closed by | Tracked in |
|---|---|---|---|
| R1 | Unattended agents hold the owner's write-scoped GitHub token while processing attacker-controlled issue text | A separate fleet identity with the narrowest permissions per role; token scoping becomes a GitHub-enforced control instead of a prompt instruction | [#904](https://github.com/bricef/factor-q/issues/904) |
| R2 | Any exec'd process reads and writes whatever the daemon's OS identity can, including ambient secrets via `/proc` and the environment | Tier-1 containers with a read-only root, declared binds and per-call secret delivery (ADR-0010, ADR-0028) | [#209](https://github.com/bricef/factor-q/issues/209) |
| R3 | `sandbox.network` is declared but not enforced; every agent is network-unrestricted | An egress proxy at the namespace edge (interim: a CONNECT-filtering forward proxy) | [#208](https://github.com/bricef/factor-q/issues/208), [#209](https://github.com/bricef/factor-q/issues/209) |
| R4 | Public issue text can steer the backlog groomer, which rewrites the specifications an implementing agent consumes, and the groomer shares the fleet's push token | Split the groomer's authority from its reading, and give it an issues-only token | [#403](https://github.com/bricef/factor-q/issues/403), [#402](https://github.com/bricef/factor-q/issues/402) |
| R5 | Tool output can carry credentials into transcripts and event payloads | Redaction at the transcript and event boundary | [#72](https://github.com/bricef/factor-q/issues/72) |
| R6 | Shared MCP servers are deduplicated without regard to `env`, so one server can run on another agent's credentials | Include the environment in the sharing key | [#523](https://github.com/bricef/factor-q/issues/523) |
| R7 | The `includeContext` path of server-initiated MCP execution bypasses the inbound redaction chain | Route `includeContext` injection through the redact chain (ADR-0018 §4) | [#345](https://github.com/bricef/factor-q/issues/345) |

## Reporting a Vulnerability

Whilst this project is in alpha (version < 1.0.0), please raise security
issues as normal GitHub issues.
