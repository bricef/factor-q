# Agent Identity, Attribution & Attestation

## Status

Draft (2026-07-07). Captures a design conversation prompted by the M0
dogfood loop: agents open PRs, comment, and (eventually) merge, yet every
action on GitHub is attributed to the human owner's credentials, so it is
impossible to tell which agent or human did what. This doc frames the
*architecture* — an identity model, a trust model, and a migration path
from convention to cryptographic attestation. It is **design-ahead**
(lives in `aspirational/`): nothing here is built yet, and the cryptographic
layers have a hard dependency on isolation ([ADR-0010](../../adrs/accepted/0010-agent-execution-isolation.md))
that is not yet satisfied. It elaborates the exposure flagged in
[assessment §4](../../reviews/2026-07-05-project-assessment.md) (credentials and
identity at the agent boundary).

## The problem: three half-identities that disagree

factor-q already has rich identity *internally* — every event carries
`(agent_id, invocation_id)`, so the event log always knows which agent and
which run produced any action. The problem is entirely at the **external
boundary**, and today it is worse than "no identity":

- **The commit** is authored by a shared fleet identity set in git config
  (`factor-q-m0 <m0@factor-q.local>`) — not per-agent, not per-invocation,
  and unknown to the runtime (it lives in git config, outside factor-q).
- **Every GitHub *action*** — opening a PR, commenting, merging — is the
  human owner, via a shared `GH_TOKEN` that *is* the owner's token.

So a reader sees: authored by `factor-q-m0`, opened by `brice`, commented
by `brice`, merged by `brice`. The identities disagree, and the one place
it is not the human is a coarse hack the runtime does not model. **That
disagreement is the bug**, not the absence of identity.

Worse, the agent does not merely *lack* an identity — it **impersonates
the owner**. Because `GH_TOKEN` is the owner's token, on GitHub the agent
*is* `brice`. That is delegation-by-impersonation. The goal is delegation
to a *distinct actor acting on the owner's behalf* — the OAuth
"on-behalf-of" model, not "become the user". (Same root as the shell-escape
finding: the agent also reaches the owner's SSH keys. Identity and
credential isolation are two faces of one boundary.)

## Why this matters more than audit: separation of duties

The near-term framing is "make the audit trail legible". The real driver is
the target state: **agents review, commit, and merge autonomously, as
distinct grant-scoped roles** (a reviewer agent that lacks a write grant; a
merger agent that only merges when policy is met). Human review does not
scale — the human becomes the bottleneck — so the endpoint removes the human
from the routine loop.

That single move changes what identity *is* here. It stops being an audit
concern and becomes an **authorization** one. The proof is the reviewer
example: for "reviewed by a write-less reviewer, distinct from the
committer" to be a property a merger can *rely on*, the attribution must be
**unforgeable**. Convention (trailers, footers) is fine for a human skimming
history; it is useless for authz, because a write-capable committer agent
could simply write `Reviewed-by: review-agent` into a trailer and the
separation collapses. Hence the spine of this whole area:

> **Separation of duties ⇒ unforgeable attribution ⇒ cryptographic attestation.**

## The identity model: a triple, expressed as a chain

Identity here is not one thing; it is a chain of accountability, and each
layer answers a different question:

| Layer | Example | Answers | Analogue |
|---|---|---|---|
| **Principal** | `brice` | *on whose behalf?* — accountability | the user in "service account on behalf of user" |
| **Definition** | `m0-review-fix` | *what kind of actor, allowed to do what?* — role + authorization | a service account / SPIFFE workload ID |
| **Invocation** | `019f3871` | *which specific run?* — audit granularity | a session / request ID / SPIFFE instance SVID |

All three, layered. Authorization keys off the **definition** (that is where
sandbox/budget/tools already live). Audit needs **definition + invocation**.
Accountability roots at the **principal** — which factor-q has *no concept
of today*; there is no `owner` in config. Note git already ships this exact
shape: **author** (who wrote it) vs **committer** (who committed it) vs
`Co-Authored-By`.

Cryptographically this is a **certificate chain**: principal (root) →
definition (durable intermediate, where grants attach, rotated rarely) →
invocation (ephemeral leaf, minted by the harness at invocation start, bound
to `(definition, invocation_id, grants-snapshot, expiry)`). Every attested
action is signed by the leaf and verifies up to the principal. Neither level
alone suffices: definition-only loses run-granularity and forces a
long-lived signing key to sit around and be stolen; invocation-only loses
the stable authorization anchor. The **ephemerality of the leaf is a
feature** — it dies with the run, so there is no long-lived leaf key to
exfiltrate or revoke.

## The harness as issuer and attestor

This is the architectural crux, and factor-q is uniquely placed for it: the
harness **launches** invocations (so it knows ground truth — this really is
a run of that definition) and **mediates every tool call** (so it can sign
actions at the boundary). So factor-q becomes, in effect, a mini
**SPIRE-server / CA**: it mints the invocation leaf, and it *attests
actions* — "invocation 019f3871 of m0-review-fix, on behalf of brice,
performed `merge` on PR #3" — signed with a key the **agent never holds**.
That is what makes it unforgeable: the agent cannot produce the attestation,
only the harness can. The natural hook is the tool-dispatch seam factor-q
already owns.

**Attestation-gated actions** are how separation of duties gets *enforced*,
not merely recorded: a merger's `merge` tool refuses unless the PR carries a
valid review attestation from a distinct, write-less identity. Policy at the
boundary. It generalizes — a deploy agent requiring a "tests-passed"
attestation, and so on — so attestations become **capability tokens in a
verifiable workflow**. (That reviewer→committer→merger pipeline is itself a
multi-agent graph — exactly what [ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md)'s
executor is for. Identity + the graph executor are what make the pipeline
real.)

## Two hard interlocks — where the complexity lives

1. **Isolation is the TCB. An attestation is only ever as strong as the
   sandbox.** The exact shell-escape that reaches the owner's SSH keys would
   equally **steal the invocation leaf key → forge attestations → collapse
   separation of duties.** So cryptographic identity has a *hard dependency*
   on [ADR-0010](../../adrs/accepted/0010-agent-execution-isolation.md)
   (isolation). The important reframing: **isolation stops being a sandboxing
   nicety and becomes the root of the entire trust model.** You cannot have
   unforgeable per-invocation identity on a sandbox an invocation can escape.
   The two must be co-designed; the cryptographic layer cannot credibly land
   first.

2. **The event log is the attestation ledger**
   ([ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md)). Sign
   the events and the log becomes a tamper-evident, CAS-backed **provenance
   ledger** — every action already carries `(agent_id, invocation_id)`; add
   the signature and you have non-repudiable history. This resolves "GitHub
   does not understand our attestations": **it does not need to.** The
   internal log is the source of truth; GitHub receives a best-effort
   *projection* (gitsign-signed commits, check-runs, status). Sovereignty of
   the record stays with factor-q.

## Prior art to stand on (so we are not inventing crypto)

- **SPIFFE / SPIRE** — workload identity, SVIDs, the node-attestation issuer
  model. The closest overall fit for harness-as-issuer + definition/invocation.
- **in-toto / SLSA** — attestation *format* and supply-chain provenance;
  likely our attestation schema.
- **Sigstore / gitsign + Rekor** — keyless commit signing via OIDC, and a
  *transparency log*. Strikingly close to "harness-as-issuer +
  event-log-as-ledger"; worth studying hard, minus the multi-agent SoD.
- **OAuth token-exchange (RFC 8693)** — the on-behalf-of / delegation
  semantics that replace impersonation.
- **X.509 short-lived certificates** — the ephemeral-leaf model.

## Migration path: convention first, crypto behind it

The value ramp lets attribution improve immediately while the cryptographic
model is designed against isolation:

1. **Content-stamping** (cheap, today). Model the triple as a runtime value
   (add `owner`/principal to config; `agent_id` + `invocation_id` exist).
   Propagate it via git's native author/committer split (author = the
   specific agent, committer = the fleet/daemon) plus trailers
   (`FQ-Agent`, `FQ-Invocation`, `On-Behalf-Of`) and PR/comment footers.
   Fixes the *inconsistency* and the impersonation-at-the-commit-level.
   *Con:* convention only — forgeable, so not an authz boundary.
2. **Distinct bot identity** (GitHub App / machine user). Native actor-level
   separation (GitHub shows `factor-q[bot]`), real on-behalf-of; the step
   that stops the agent using the owner's token at all — the same work as
   giving the agent its own scoped credentials.
3. **Cryptographic invocation identity + harness attestation.** The
   unforgeable layer; co-designed with isolation; makes attestation-gated
   separation of duties real.

The merge staying a human act is *transitional*, not the target — the point
is only to make the human's deliberate gate acts distinguishable from the
fleet's autonomous ones until the merger agent exists.

## Expected ADRs

This design doc is the frame; the decisions belong in ADRs:

- **Identity model** — the triple, the `principal`/`owner` concept, how
  `(definition, invocation)` are represented. Foundational; can start
  non-cryptographic.
- **Boundary attribution** — git author/committer split + trailers +
  footers. The cheap first increment; ships value while the rest is designed.
- **Cryptographic invocation identity & harness attestation** — leaf
  issuance, the signing scheme, what gets attested. Co-designed with 0010.
- **Attestation-gated actions & separation of duties** — the grant model
  (review/write/merge), the merger's verification, the policy enforcement
  point.
- **Attestation storage, verification & revocation** — interlocks with 0026;
  the transparency-ledger question; the short-lived-leaf revocation story.

Sequenced so **attribution lands first** (real value, M0-sized), the
**cryptographic layers build behind it**, and they are **gated on isolation**
maturing.

## Open questions

- Where do definition and harness signing keys live, and how are they
  custodied / rotated? (The harness becomes a high-value secret holder.)
- Does the principal hold a root key, or is it an OIDC identity a
  keyless/Fulcio-style flow certifies?
- Do we adopt in-toto attestation format wholesale, or a narrower internal
  schema first?
- How much of the GitHub projection is worth building before the internal
  ledger is the trusted record — is gitsign on commits the right first
  external signal?
- What is the revocation story for a compromised *definition* (vs. the
  self-expiring invocation leaf)?
