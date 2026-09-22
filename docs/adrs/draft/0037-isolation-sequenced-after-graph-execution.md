# ADR-0037: Isolation is sequenced after the first graph-execution pass — a dated hold with a named exit

## Status

Draft — proposed 2026-09-22, recording a decision the maintainer took in
discussion on 2026-09-21. This record is the maintainer's to accept; until
then it is the written reason for a hold that would otherwise be
indistinguishable from neglect (the rule in
[STATUS.md](../../../STATUS.md): *anything held carries a named exit
condition*).

Implementation: not applicable — this ADR sequences work, it does not
build any. The work it orders is the graph executor's first pass
([#414](https://github.com/bricef/factor-q/issues/414)) first, then the
isolation design session
([context](../../design/aspirational/agent-isolation-discussion-context.md))
and the ladder it settles
([#208](https://github.com/bricef/factor-q/issues/208),
[#209](https://github.com/bricef/factor-q/issues/209)).

## Context

Two isolation models are accepted and neither is built.
[ADR-0010](../accepted/0010-agent-execution-isolation.md) decided
containers by default and microVMs for untrusted models or production
credentials; [ADR-0028](../accepted/0028-tool-scoped-isolation-and-workspace.md)
moved the unit of isolation from the agent to the tool, behind a
harness-owned workspace. What runs is the phase-1 process sandbox that
ADR-0010 itself describes as insufficient: path canonicalisation plus
`exec_cwd`, where an exec grant dominates every narrower declaration and
`sandbox.network` enforces nothing. The dogfood fleet runs unattended on
public issue text with the owner's write-scoped GitHub token, and the human
review-and-merge gate is the only enforced control on that chain.
[SECURITY.md](../../../SECURITY.md) states this posture and carries the
register of open exposures.

The [2026-09-03 production-readiness review](../../reviews/2026-09-03-production-readiness-review.md)
parked containers (#209) until its Phase 2 was done and named the egress
proxy (#208) as the first item after it. The multi-node hold (#414) was
itself waiting on two security proofs-of-concept
([#399](https://github.com/bricef/factor-q/issues/399),
[#400](https://github.com/bricef/factor-q/issues/400)); both are closed
(PRs #792, #803, #836). On 2026-09-17 the maintainer read #208 and #400 as
band-aids on a boundary that has not been designed, and sent the design to a
dedicated session rather than to incremental issues. This ADR records the
order in which that session and the executor happen.

## Decision

1. **The graph executor's first pass comes first.** The two-node
   propose → review vertical
   ([plan](../../plans/active/2026-07-07-graph-executor-two-node-vertical.md),
   #414) is built on the phase-1 sandbox as it stands. Isolation is
   reviewed after it, not alongside it.

2. **Why in that order.** The isolation boundary has to fit the executor's
   shape: what a node is, where a tool runs, what a workspace is, and which
   credentials a node legitimately holds. ADR-0028's harness-owned
   workspace is defined at the node/tool boundary, and that boundary does
   not exist yet. Designing the substrate first would fix it around today's
   single-agent runner and be redesigned once nodes exist. The executor is
   also what the measurement instrument needs to answer the M1 question;
   holding it behind isolation would hold both.

3. **What the hold covers.** No isolation substrate work is built standalone
   before the design session: not the proxy (#208), not the containers
   (#209), not workspace overlays. Incremental isolation issues are not
   filed; findings go to the context document.

4. **What the hold does not cover.** Credential *scope* is not substrate
   and shrinks the blast radius without touching it, so it proceeds on its
   own merits: a separate identity for the fleet and the watcher
   ([#904](https://github.com/bricef/factor-q/issues/904)), an issues-only
   token for the groomer ([#402](https://github.com/bricef/factor-q/issues/402)),
   and any narrowing of what a token can do. Point fixes to a proven escape
   (the kind #399 and #400 were) are likewise not held; they are bugs.

5. **Exit condition.** The isolation design session opens when #414 is
   closed by a merged PR **and** the two-node vertical has completed at
   least one real issue end to end on the dogfood instance. The session
   starts from the context document's eight questions and produces the
   ADR that supersedes or refines ADR-0010 and ADR-0028; the sequencing
   question inside it (proxy first, tier 1 at once, or secret placement
   first) is the session's to answer, not this record's.

6. **Early reopen.** Any of the following reopens the design at once,
   regardless of #414: a credential or sandbox escape observed outside a
   proof-of-concept; a change that lets anyone other than the maintainer
   trigger the fleet (the trigger-boundary cost controls of
   [#42](https://github.com/bricef/factor-q/issues/42) are the likely
   vehicle); or a second repository or instance coming under the fleet.

## Consequences

- The exposures in SECURITY.md's register stay open for the duration, and
  the record says so in one place with a date, rather than in a caveat
  sentence that ages silently.
- The executor is designed with containment in mind even though it ships
  on the process sandbox: no node may assume ambient credentials, and the
  tool boundary is the one ADR-0028 names, so that the later substrate
  slots in under it rather than through it.
- The human merge gate remains the sole enforced control, and the
  advisory merge verdicts (#879) must not be promoted to a merge action
  while this hold stands — that promotion is a control change and belongs
  after isolation, or after identity separation at the very least.
- The fleet token stays narrow (no `workflow` scope); CI wiring stays a
  maintainer-session act.

## Alternatives considered

- **Proxy first (#208), as the September review sequenced it.** A
  CONNECT-filtering proxy in the current shape is defence in depth that an
  exec'd process can unset its way around; it would be rebuilt at the
  namespace edge once the substrate exists. Held with the rest.
- **Secret placement first**, because it shrinks the blast radius most for
  the least work. Plausible, and it is question 8 of the context document;
  deferred to the session rather than decided here without the substrate
  in view.
- **Isolation first, executor later.** Rejected: it designs the boundary
  around a runner the executor replaces, and it holds the measurement
  instrument's data behind it.
