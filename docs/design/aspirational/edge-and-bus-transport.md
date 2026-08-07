# Transport for an Event-Driven Edge and Core

**Design-ahead** (this doc lives in `aspirational/`): almost nothing here is
decided. It records a friction that is real and measurable today, surveys the
options for resolving it at the operator edge, at the internal bus, and at a
possible third surface for remote workers, and states the tradeoffs as honestly
as it can. A short list of things *have* been decided in discussion and are
marked as such; everything else is open. Treat the option survey as a thinking
tool, not a shortlist.

Note that the last section raises three decisions — assignment leases, provider
credential placement, and workspace retention — which are **not** transport
questions and would outlive any choice made here.

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

**Remote daemon access is expected.** That tilts the balance toward quinn.

It tilts a little further if the [remote worker
surface](#a-third-surface-remote-workers-over-adversarial-transport) is built,
because that surface wants the same properties for the same reasons — key-based
identity, traversal, and migration across links that drop. Note this is a
*second constituency*, not the original consolidation argument, which remains
withdrawn: the worker surface is deliberately separate from the operator edge,
so it does not unify anything. It only means a single transport choice would
serve two places instead of one.

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

**C3 — Workers as clients of the operator edge (rejected on shape).** Workers
authenticate to the *operator* edge instead of the bus.

- **Strengths:** one identity model for both trust boundaries.
- **Weaknesses:** the edge is shaped for operators poking at views and issuing
  commands. Workers want something else — receiving assignments, streaming
  results, heartbeating: bidirectional, long-lived, high-volume. Jamming a
  worker protocol into the *operator surface* because they share a socket
  would be a mistake.
- **Status:** rejected, but note precisely what is rejected — reusing the
  *operator surface*, not remote workers. A purpose-built worker surface is a
  different proposition and has its own section below.

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

## A third surface: remote workers over adversarial transport

C1–C4 all assume workers reach the control plane over a network someone has
made trustworthy. This section assumes the opposite, and it is the most
consequential idea in this document.

**Not an alternative to the bus — an additional deployment mode.** Local
workers keep NATS and change nothing. A remote worker surface is what a worker
uses when it is not on the same trusted network, which makes "on the bus" and
"remote client" two ways to deploy the same role rather than two architectures
to choose between.

### The argument

The weak version is "we would rather not run a VPN". The strong version is that
**security stops depending on network placement**, and once that is true,
deployment topology becomes a free variable.

A mesh network makes the network trustworthy so the protocol does not have to
be. Assuming adversarial transport makes the protocol trustworthy so the
network does not have to be. Only the second composes: local, multi-region,
multi-cloud, on-prem, and a spare box under a desk are all the same case. The
first grows an operational burden with every placement decision.

Two supporting arguments:

- **A publicly reachable JetStream server is an uncomfortable object.** NATS is
  a large, general-purpose protocol with a great deal of capability behind one
  credential. A worker surface can be minimal — take work, report progress,
  return results, heartbeat — and a small surface is both a smaller attack
  surface and something that can actually be audited.
- **It is cheaper to assume now than to retrofit.** Assume adversarial
  transport and local deployment is the degenerate case. Assume a trusted
  network and adding adversarial transport later means re-auditing every
  assumption about who could observe or inject what.

### The cost that bites: delivery guarantees now span two hops

Today a worker's assignment is a JetStream delivery: durable, acked,
redelivered on failure. A remote worker surface puts a second hop between the
stream and the executing process, and **at-least-once across two hops with
independently chosen deadlines is a known failure mode in this system.**

The trigger redelivery storm is the precedent: a 1s ack deadline against the
async-nats client's pull buffering produced duplicate PRs per issue under fleet
saturation. That was *one* hop with mismatched ack semantics. A remote worker
surface makes two-hop acks structural rather than incidental.

The mitigation shape is knowable in advance:

- Keep JetStream internal; the worker surface is a **bridge**, not a
  replacement, so durability and replay stay where they already work.
- **Never ack upstream until the worker has durably accepted downstream.** Ack
  translation is precisely where this class of bug lives.
- Worker-side **idempotency becomes mandatory**, not advisory, because
  at-least-once across two hops means genuine duplicate delivery.

### Partition stops being exceptional

[The data architecture](../committed/data-architecture.md) states that once an
invocation is assigned to a worker, **the assignment is immutable for that
invocation's lifetime**, with heartbeats, a stale threshold and
`worker.orphaned` handling failure. That is a model in which partition is rare.

Across regions, clouds, or an adversarial network, partition is routine — and
immutable assignment plus a partitioned worker means work that stalls with no
legal reassignment. The standard answer is **leases with fencing tokens**, so a
worker returning from a partition cannot complete work that has already been
reassigned to someone else.

That is an amendment to a committed design, not an implementation detail, and
it is the single most likely thing to be discovered too late.

### Who holds the provider credentials

Possibly the larger decision, and independent of transport.

The LLM client and API-key resolution live in `fq-runtime` — the same crate a
worker runs. A remote worker executing invocations therefore needs provider
credentials **in whatever cloud it is deployed to**.

The alternative is for the daemon to proxy LLM calls so keys never leave the
control plane. That costs latency, sends every token through the control plane,
and turns the daemon into a throughput bottleneck exactly when workers were
distributed to remove one.

Neither is obviously right. It should be decided deliberately, because it
determines how much trust a worker placement implies.

### Workspace locality

Workspaces are worker-local today, and `workspace_ref` is a reserved column
with nothing behind it — the store's own migration notes call it a placeholder
for a future, "likely content-addressed" workspace-storage layer.

Remote workers make that layer load-bearing: artifacts, archives and results
all have to cross the boundary, and what a worker is permitted to *retain*
after an invocation completes becomes a question with a security answer rather
than a convenience answer.

### The recommendation this section carries

**Design the boundary now; defer the wire.**

What a worker may know, what it may do, how delivery and acknowledgement behave
across a link that can vanish, and what happens to an invocation when its
worker partitions — these are answerable today, they are the expensive things
to retrofit, and none of them depends on choosing a wire protocol. The protocol
can follow once the edge transport question settles, since it will likely want
the same transport.

This also keeps the "NATS for now" position intact rather than forcing a
migration: local workers keep the fast path, and the remote surface arrives as
an additional deployment mode when it is needed.

## The identity split

If C1 is taken and the remote worker surface is eventually built, the system
has three surfaces rather than two:

| | mechanism | principals | authorization | trust assumption |
|---|---|---|---|---|
| **Bus** | nkeys / JWT | workers, control plane | subject-scoped permissions | trusted network |
| **Edge** | biscuits + pinned TLS | operators, dashboard, integrations | capability attenuation | adversarial |
| **Worker surface** | undecided | remote workers | scoped to assigned invocations | adversarial |

Three is more than two, and that is a real cost. It is defensible because the
boundaries are **by role and by trust assumption** rather than accidental: the
bus is machine-to-machine where placement can be trusted, the edge is
human-facing at the perimeter, and the worker surface is machine-to-machine
where placement cannot be trusted.

The test to keep applying: can the split be explained in a sentence without
reference to history? If it ever cannot, the surfaces have drifted and one of
them should absorb another.

Note what this does to the transport argument. The original case for an
iroh-style transport was that *one* identity model could cover both boundaries
— an argument that died when workers stayed on the bus. A remote worker surface
does **not** revive it, because that surface is deliberately separate from the
operator edge. What it revives is weaker but still real: a second *constituency*
wanting the same transport properties — key-based identity, NAT traversal,
connection migration across networks that drop. quinn can still be adopted
without iroh, and iroh added later if it earns its place, since iroh is built
on quinn.

## What is decided, and what is not

**Decided in discussion:**

- Workers stay inside the bus **for now**. NATS/JetStream remains the
  worker↔control-plane path, and nothing has to migrate.
- Remote daemon access is expected, not hypothetical.
- Reusing the *operator edge* for worker traffic is rejected on shape (C3).
- A *purpose-built* remote worker surface is **in scope as a future deployment
  mode**, not ruled out. Local and remote become deployment modes of one role.

**Not decided — the substance of this document:**

- The edge transport (Options A–H).
- Whether browsers ever talk to the edge directly.
- Which core credential model, and when it lands relative to worker separation.
- Whether metrics feed the scheduler.
- Whether iroh is adopted alongside quinn.
- Everything about the remote worker surface except that it is worth having.

**Decisions that are not transport questions at all**, but which the remote
worker surface forces, and which are listed here because they are easy to
mistake for implementation detail:

- Whether assignment stays immutable for an invocation's lifetime, or becomes a
  lease with fencing tokens.
- Whether remote workers hold provider credentials, or the daemon proxies LLM
  calls.
- What a remote worker may retain after an invocation completes.

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
7. **Does assignment stay immutable, or become a lease?** Immutable assignment
   assumes partition is rare. A remote worker surface makes it routine, and
   stalled-with-no-legal-reassignment is the failure that follows. Leases with
   fencing tokens are the standard answer, and this amends a committed design.
8. **Do remote workers hold provider credentials, or does the daemon proxy LLM
   calls?** The first puts your keys in someone else's cloud; the second sends
   every token through the control plane and rebuilds the bottleneck that
   distributing workers was meant to remove. Independent of transport, and
   arguably the larger decision.
9. **What may a remote worker retain after an invocation completes?**
   `workspace_ref` is a reserved column with nothing behind it. Remote workers
   make the content-addressed workspace layer load-bearing, and retention stops
   being a convenience question and becomes a security one.
10. **Where does the two-hop ack boundary sit, and who owns idempotency?** The
    trigger redelivery storm was one hop with mismatched ack semantics. Two
    hops make that structural, so the answer needs to exist before the first
    remote worker does.

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
