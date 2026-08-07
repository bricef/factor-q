# Transport for an Event-Driven Edge and Core

**Design-ahead** (this doc lives in `aspirational/`): almost nothing here is
decided. It records a friction that is real and measurable today, surveys the
options for resolving it at both the operator edge and the internal bus, and
states the tradeoffs as honestly as it can. Two things *have* been decided in
discussion and are marked as such; everything else is open. Treat the option
survey as a thinking tool, not a shortlist.

External-ecosystem claims here reflect the state of things around mid-2026 and
should be re-checked before any of them is load-bearing.

## Context

### The friction

factor-q is an event-driven system whose operator surface speaks a
request/response RPC. [tarpc](https://github.com/google/tarpc) has no
server-streaming, so every "stream" on the operator surface is a long poll: a
call that asks the daemon to hold the line, returns a batch, and hands back a
cursor to ask again with.

The interface is being shaped by what the transport can express. That is
backwards — the tool should not dictate the shape of the interface — and the
cost is no longer theoretical.

### What the workaround has already cost

**A shipped, user-visible defect.** From `fq-cli/src/edge_call.rs`, recording
why:

> tarpc's default deadline is a flat ten seconds, which is **shorter than the
> window these calls ask the daemon to hold** (30s). A poll that legitimately
> waits out its window is then abandoned by the very client that asked for it,
> and the verb dies with `edge rpc failed: DeadlineExceeded`.
>
> That this was not obvious is worth recording: `event.stream` reads the whole
> log, and the daemon heartbeats every `DEFAULT_INTERVAL_MS` — exactly 10s — so
> an idle tail's poll was ended by a heartbeat in a photo finish with the
> deadline, and lost the race only under load. `turn.stream` has no such cover:
> it is filtered to one agent's subject, so `invocation transcript --follow` on
> a quiet invocation loses every time.

A follow verb that could not survive ten idle seconds. The code is careful and
the comment is excellent; the defect exists because a subscription had to be
expressed as a call with a timeout, and then two independent timers had to be
reasoned about together.

**The same workaround, independently, in a second place.** From
`fq-dashboard/src/main.rs`:

> tarpc has no server-streaming, so poll-and-forward is the tarpc-shaped bridge

The dashboard polls the daemon and forwards to the browser as SSE. Two
subsystems, two authors, the same shape forced by the same limitation.

**Transport vocabulary in the domain.** `from_seq` is a JetStream coordinate on
the wire. Keeping it contained required an explicit rule — *cursors may be
transport coordinates; identities may not* — and enforcing that rule has been
real work.

**Batching nothing in the domain asked for.** `EVENT_BATCH_CAP` and
`DEAD_LETTER_BATCH_CAP`, both 64. A batch is not a domain concept; it exists
because an answer has to fit in one response. The page caps (`event.list` at
2000, `dead_letter.list` at 500) are likewise derived from an 8 MiB frame
rather than from anything about events or dead letters.

### The detail that changes the cost estimate

The entire tarpc service is two methods:

```rust
#[tarpc::service]
pub trait Edge {
    async fn invoke(request: InvokeRequest) -> Result<InvokeResponse, WireError>;
    async fn next_batch(request: NextBatchRequest) -> Result<StreamBatch, WireError>;
}
```

tarpc's headline benefit — a service definition that *is* a Rust trait of typed
methods — is already forfeit, because the operator surface deliberately went
dynamic: operations dispatch through the registry as `(OpId, serde_json::Value)`
(see [ADR-0006](../../adrs/accepted/0006-registry-first-api.md)). tarpc is
functioning as a framed request/response envelope with two entry points, **one
of which exists only to emulate streaming**.

So this is not "replace our RPC framework". It is "replace an envelope", and
one of its two methods disappears rather than being ported.

## What any answer must preserve

These are not negotiable without a separate decision, and they filter the field
more than performance does:

1. **A registry-driven, self-describing surface.** Operations are declared, not
   hand-written, and `describe` is how a consumer learns them. A transport that
   needs its own schema adds a second source of truth and a parallel discovery
   mechanism that describes the schema rather than the surface.
2. **Capability tokens with offline attenuation.** Biscuits, minted once,
   narrowed without contacting the daemon. This is how narrower principals
   (dashboard, integrations) exist at all.
3. **TLS with a pinned fingerprint, and a pairing an operator performs once.**
4. **A native Rust client that is pleasant to write**, since the CLI and
   dashboard are the first consumers.
5. **Typed on both ends** — not hand-marshalled JSON.
6. **No codegen.** The project's position is shared definitions plus derive,
   with dynamic consumers reading `describe`. This is the single strongest
   discriminator below.

## The edge

### Option A — keep tarpc, add a side channel

Request/response stays on tarpc; live delivery moves to a purpose-built
subscription channel.

- **Strengths:** smallest blast radius; commands keep a shape that genuinely
  suits them; the streaming design can be iterated without touching the command
  path.
- **Weaknesses:** two transports to secure, describe, version and keep in
  agreement. Two handshakes and two auth integrations for one logical
  connection. The seam between them becomes a place bugs live.
- **Tradeoff:** buys time, pays rent forever.

### Option B — replace tarpc's protocol over the existing TLS + framing

Keep the rustls stack, the pinned verifier, the pairing, and the
`LengthDelimitedCodec` framing. Replace only tarpc's request/response protocol
with a message protocol that supports server push.

- **Strengths:** the minimal delta. No new dependency — it *removes* one.
  Nothing in the security model changes. `next_batch` and both `BATCH_CAP`s
  delete rather than port. `wire.rs` is already transport-agnostic
  (`InvokeRequest`, `InvokeResponse`, `WireError`), so the types survive intact.
- **Weaknesses:** you own the session layer — keepalives, half-open detection,
  close semantics, reconnect, protocol versioning. Perhaps 300–500 lines, of a
  kind that is easy to get subtly wrong and hard to test.
- **Tradeoff:** maximum control, and the maintenance that comes with it.

### Option C — WebSocket

`tokio-tungstenite` or axum's `ws`, carrying serde-derived message enums.

```rust
enum ServerMsg {
    Response  { id: u64,  result: Result<InvokeResponse, WireError> },
    StreamItem { sub: u64, event: EventEnvelope },
    StreamEnd  { sub: u64, reason: EndReason },
}
```

- **Strengths:** a well-specified message session layer — ping/pong, close
  codes, message boundaries — for roughly one dependency, which is Option B's
  300–500 lines already written and debugged. Extremely mature libraries.
  `websocat` and browser devtools for debugging. TCP/443 traverses everything.
  Rust types are the schema: derive, not codegen. Keeps the TCP+TLS shape, so
  the rustls config, pinned verifier and pairing carry over nearly unchanged.
- **Weaknesses:** one TCP connection is one ordered byte stream, so
  multiplexing subscriptions over a single socket reintroduces head-of-line
  blocking — and under the [list/stream
  semantic](#relationship-to-open-tickets) *stream is the unredacted side*,
  carrying whole payloads that can be megabytes. A fat event on an idle tail
  would stall a concurrent command. Per-subscription backpressure over one
  socket must be hand-built (credits, pause/resume), and getting it wrong means
  either unbounded daemon buffering or silent gaps.
- **Mitigation that mostly dissolves both weaknesses:** one connection per
  subscription plus one for commands. TCP flow control then *is* per-
  subscription backpressure, and there is no multiplexing to block. With a
  handful of first-party clients this is ordinary, not a workaround.
- **Tradeoff:** the boring choice, and boring is worth a lot.

### Option D — QUIC (quinn)

- **Strengths:** multiplexed streams as a real primitive — one subscription per
  stream, no correlation ids to invent, no head-of-line blocking between a slow
  tail and a fast command, and **per-stream flow control** so backpressure is
  free rather than hand-built. TLS 1.3 is intrinsic and the pinning verifier
  ports directly (quinn uses rustls). Connection migration: a client whose
  network changes does not drop its subscription. 0-RTT reconnect is available.
  `quinn` is mature and production-used.
- **Weaknesses:** UDP, which some corporate networks and VPNs block —
  precisely the paths a remote operator might use. Smaller tooling ecosystem;
  no `websocat` equivalent, and Wireshark needs keying material. Replaces the
  TLS/TCP layer rather than sitting on it, so connection setup and the pairing
  flow move. Byte streams, so you keep your own framing (you already have it).
- **Tradeoff:** the domain concept and the transport concept finally line up —
  a subscription *is* a stream — at the cost of a less-trodden operational path.

### Option E — WebTransport over HTTP/3

Only relevant if browsers talk to the edge directly.

- **Strengths:** the sole option whose browser API matches the existing
  security model. `serverCertificateHashes` lets a browser connect by
  certificate hash, bypassing CA validation — which is exactly
  pinning-on-a-fingerprint, expressed as a web API. Same multiplexing and flow
  control as QUIC.
- **Weaknesses:** constraints on that API (notably short certificate validity,
  on the order of two weeks, and specific key types) mean the daemon needs
  certificate rotation it does not have. Server-side Rust support
  (`wtransport`, the h3 ecosystem) is younger and smaller than quinn's. Browser
  WebTransport cannot set arbitrary request headers, so the token must travel
  as a first message. Adds an HTTP/3 layer that native clients do not need.
- **Tradeoff:** the only credible route to browser-direct access without
  introducing a CA — and dead weight if browser access never happens.

### Option F — gRPC / tonic

- **Strengths:** best-in-class streaming ergonomics — a handler returns
  `impl Stream<Item = Result<T, Status>>`, the client gets `Streaming<T>` and
  calls `.message().await`. Mature, tokio-native, good backpressure,
  polyglot clients for free.
- **Weaknesses:** `.proto` becomes a second source of truth beside the
  registry, and the build grows a codegen step — a direct conflict with
  constraint 6. gRPC server reflection describes protobuf services, not
  atoms/views/commands, so `describe` either duplicates it or contradicts it.
- **Tradeoff:** the best streaming story, bought by adding the exact seam the
  architecture was built to avoid.

### Option G — Cap'n Proto RPC

- **Strengths:** capability-based security maps conceptually onto biscuit
  attenuation; promise pipelining is elegant for chained calls.
- **Weaknesses:** schema-file driven (same codegen objection), and the Rust
  implementation's ergonomics are rougher than tonic's.
- **Tradeoff:** philosophically closest to the capability model, practically
  the least pleasant to write.

### Option H — ZeroMQ

- **Strengths:** battle-tested brokerless patterns; PUB/SUB is a natural fan-out
  shape; strong control over topology and latency.
- **Weaknesses**, all specific to this system rather than to 0MQ:
  - We already have the messaging system. NATS/JetStream is the internal bus;
    0MQ would be a second messaging stack whose only job is the external edge.
    The bottleneck is not missing pub/sub patterns — it is that a typed,
    describable, authenticated operator surface cannot express a stream.
  - PUB/SUB is **lossy by design** (high-water-mark drops). Silent gaps are the
    failure mode the stream semantic explicitly rules out. Recovering
    reliability means DEALER/ROUTER plus acks plus resume — rebuilding what
    JetStream already provides, on the client.
  - It brings its own identity model (CURVE keypairs), parallel to biscuits and
    pinned TLS. Two auth systems, or pairing rebuilt on 0MQ's terms — and
    offline attenuation is what makes narrower principals possible.
  - No typed service definition; dispatch and serialization are yours. From
    tarpc that is a regression. The `zmq` crate wants libzmq (a C dependency);
    pure-Rust options are less proven.
- **Tradeoff:** solves a problem this system does not have, and does not solve
  the one it does.

### The browser question

Today **no browser talks to the edge.** The dashboard is server-rendered Rust
using Datastar, which reaches the browser over SSE; the browser's counterparty
is the dashboard, and the dashboard is a tarpc client of the daemon. Every edge
client is Rust.

That matters because it is the hinge for Option E. If browser-direct access to
the daemon is ever a goal:

- **WebSocket from a browser requires a CA-valid certificate.** There is no
  pinning API. Retrofitting the pinning model onto browser WebSocket means
  introducing a CA — a larger architectural concession than choosing a younger
  crate.
- **WebTransport's `serverCertificateHashes` is the only browser API that
  matches the model already in use.**

It also has a cost rarely stated: a browser-reachable daemon means capability
tokens in browser storage, an XSS bug becoming a token-exfiltration bug, and
origin policy entering the security model. Today the dashboard holds the token
and the blast radius is smaller. Browser reach is a capability, not a freebie.

### Where the balance currently sits

With browsers out of scope, WebTransport leaves the field and its certificate
advantage — the strongest argument on the QUIC side — goes with it. What
remains for QUIC is multiplexing, per-stream flow control, and connection
migration; and the first two are obtainable on TCP via connection-per-
subscription. That leaves **migration and NAT traversal** as QUIC's only
un-substitutable advantages, which are worth little for a localhost daemon and
a great deal once remote daemon access is real.

**Remote daemon access is expected.** That tilts the balance toward quinn,
though on one leg rather than two — see [the identity split](#the-identity-split)
for the argument that was withdrawn.

## The core

### What staying on the bus commits us to

**Decided:** workers stay inside the bus. Worker↔control-plane traffic —
assignments, results, heartbeats — remains NATS rather than becoming edge
traffic.

The immediate consequence is a security one. Bus auth today is a shared bearer
token (`nats://TOKEN@host`). That is fine on localhost and inadequate as a
trust boundary between hosts: every worker holds the same credential, there is
no per-worker identity, and holding it grants the whole bus. The
[data architecture](../committed/data-architecture.md) already anticipates
workers on different nodes with placement preferences, so this is a
prerequisite for that work rather than a nicety.

### Core options

**C1 — NATS with nkeys/JWT (the natural path).** Per-worker credentials with
subject-scoped publish/subscribe permissions.

- **Strengths:** ed25519 keys, and — unlike a flat key model — a real
  hierarchy: an operator key signs account JWTs, an account key signs user
  JWTs. Authorization is built in at the subject level, so a worker can
  subscribe to its own assignment subject and publish its own results and
  nothing else. Revocation lists exist. Validation is self-contained, with no
  callout on connect.
- **Weaknesses:** a credential lifecycle to operate (issuance, distribution,
  rotation, revocation) and an enrolment ceremony to design. A second identity
  system beside the edge's.
- **Tradeoff:** the most capable option, and the one with real operational
  surface.

**C2 — NATS with mTLS.** Per-worker client certificates.

- **Strengths:** familiar; integrates with existing PKI if an organisation has
  one.
- **Weaknesses:** authorization is not expressed in the credential, so subject
  permissions must be configured separately and kept in sync. Certificate
  lifecycle is heavier than nkeys.
- **Tradeoff:** worse than C1 on almost every axis unless an external CA is
  already mandated.

**C3 — Workers as edge clients (considered, not taken).** Workers authenticate
to the operator edge instead of the bus.

- **Strengths:** one identity model for both trust boundaries; the bus returns
  to being purely internal, which is what
  [ADR-0006](../../adrs/accepted/0006-registry-first-api.md)'s appendix says it
  should be. Combined with an iroh-style transport it would give NAT traversal
  for workers behind firewalls.
- **Weaknesses:** the edge is shaped for operators poking at views and issuing
  commands. Workers want something else — receiving assignments, streaming
  results, heartbeating: bidirectional, long-lived, high-volume. Sharing
  identity and transport is the win; jamming a worker protocol into the
  *operator surface* because they share a socket would be a mistake.
- **Status:** not taken. Recorded because the argument for it was the second
  reason to prefer quinn+iroh, and that reason is now withdrawn.

**C4 — Replace NATS.** Not seriously evaluated. JetStream's durability, replay
and subject model are load-bearing throughout, and nothing in the friction
above is a NATS problem.

### Management channel

Workers need to be drained, reloaded, inspected and shut down.

The shape that fits what is already decided: **edge command → control plane →
bus subject → worker.** The operator never touches NATS. `worker.drain`,
`worker.reload`, `worker.shutdown` become declared Commands returning Receipts,
exactly like `invocation.drop`, and the control plane owns the translation.

- Keeps ADR-0006's appendix intact (NATS is not an external interface).
- Consistent with the decision to stop using NATS as an external control source
  and mirror the drop verb.
- Worker management inherits the operator surface's authorization rather than
  growing a parallel one.
- `fq.control.reload` and `fq.control.down` become internal plumbing.

The alternative — operators publishing to control subjects directly — is
rejected for the reasons that decision was already made: it puts a second,
unauthenticated, undescribed control surface beside the authenticated one.

### Metrics channel

Worker resource usage, queue depth, throughput. Three questions, and the third
is the one that matters.

**Which substrate?** Core NATS, not JetStream. Metrics are fire-and-forget
telemetry; durability is cost without benefit. Putting them on the event stream
inflates the log, the projection and the sweep — and would place high-
cardinality machine measurements next to cost rows that are retained
*indefinitely* by policy.

**Which domain shape?** Not atoms. An atom is an immutable fact about the
domain; worker CPU and queue depth are measurements of the machinery, which is
what **synthetics** (live state) and **reports** (computed over a window)
already exist for. Modelling them as atoms would produce streamable, indexable,
retained facts requiring aggressive sweeping.

**Observability stack, or in-band?** Worth separating two consumers:

- *Operators and dashboards* are well served by the existing ecosystem — a
  Prometheus scrape endpoint per worker, or OTLP push to a collector. Grafana
  and friends come free. Pull models struggle when workers sit behind NAT;
  push (OTLP) does not.
- *The scheduler*, if it uses metrics for placement, needs them **in-band** and
  cannot depend on an external collector being up. At that point metrics stop
  being observability and become an input to correctness, and "lossy
  fire-and-forget" needs a stated tolerance — a stale-metric fallback, in the
  shape of the existing heartbeat/stale-threshold pattern — rather than an
  assumption.

A plausible split is a small in-band set for scheduling and a richer OTLP
export for humans, but that is exactly the kind of thing that should be decided
deliberately rather than arrived at.

## The identity split

If C1 is taken, the system has two credential models:

| | mechanism | principals | authorization |
|---|---|---|---|
| **Bus** | nkeys / JWT | workers, control plane | subject-scoped permissions |
| **Edge** | biscuits + pinned TLS | operators, dashboard, integrations | capability attenuation |

Two systems is a cost. It is defensible here because the boundary is **by
role** — machine-to-machine inside the trust domain versus human-facing at the
perimeter — rather than accidental. The test to keep applying: can the split be
explained in a sentence without reference to history? If it ever cannot, that
is the signal to revisit C3.

Note what this withdraws. The strongest argument for an iroh-style transport
was that it would let *one* identity model cover both boundaries. With workers
staying on the bus, that argument is void, and iroh's remaining value is NAT
traversal and migration for **operators** reaching a remote daemon — real, but
one leg rather than two. quinn can be adopted without iroh, and iroh added
later if it earns its place, since iroh is built on quinn.

## What is decided, and what is not

**Decided in discussion:**

- Workers stay inside the bus (C3 not taken).
- Remote daemon access is expected, not hypothetical.

**Not decided — the substance of this document:**

- The edge transport (Options A–H).
- Whether browsers ever talk to the edge directly.
- Which core credential model, and when it lands relative to worker separation.
- Whether metrics feed the scheduler.
- Whether iroh is adopted alongside quinn.

## Open questions

1. **How many concurrent subscriptions does one client realistically hold?**
   This decides whether connection-per-subscription is elegant or absurd, and
   therefore whether QUIC's multiplexing is a requirement or a nicety.
2. **Does the pairing model survive a transport change unchanged?** It should
   for B and C; D moves the connection setup; E needs certificate rotation.
3. **What is the enrolment ceremony for a new worker?** Whichever core option
   is chosen, the join flow is the part most likely to be got wrong, and it is
   the `fq connect` problem pointed in the other direction.
4. **How is a compromised worker credential revoked, and how fast?** JWT
   revocation lists answer this for C1; bearer capabilities on the edge are the
   harder half and lean on short expiry.
5. **Does the operator surface need a versioning story before the transport
   changes?** Today the registry is self-describing, which absorbs a great deal
   — but a client that predates a message-shape change is a different problem
   from one that predates an operation.
6. **What is the smallest change that removes the deadline defect?** If the
   answer is much cheaper than any option here, it should be taken first, and
   this document allowed to proceed at its own pace.

## Relationship to open tickets

- **#469** — the question this document exists to answer.
- **#468** — the list/stream semantic (historical/paginated/redacted versus
  immediate/live/unredacted). That semantic holds under any transport; its
  *cursor* may not. With a real subscription, "resume at a cursor after a
  dropped call" may become "a subscription that does not drop". Note that
  resume does not disappear entirely under any option, because daemon restarts
  still need it.
- **#465** — byte-budgeted pages instead of row caps. A frame-sized problem; a
  streaming transport changes what "one answer" means and may dissolve it
  rather than solve it.
