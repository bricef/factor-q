# ADR-0032: Trigger dispatch is exactly-once by durable claim, not by timing

## Status

Draft — proposed 2026-07-19. Prompted by the 2026-07-18 duplicate-invocation
incident (#327: one trigger for issue #189 → three invocations → three PRs,
numbers 322/323/324). Execution plan:
[exactly-once trigger dispatch](../../plans/active/2026-07-18-exactly-once-trigger-dispatch.md).

## Context

JetStream is an **at-least-once transport**. Today the runtime's defence
against duplicate invocations is entirely *timing-based*: ack-on-durable-start
(#41) acks a trigger shortly after processing begins, and correctness holds
only while "delivery ≈ processing-start". Under fleet saturation that
assumption fails unboundedly — a trigger can be delivered (starting the
server's ack clock, effectively 1s via `TRIGGER_RETRY_BACKOFF[0]`) minutes
before any executor slot frees, and the async-nats pull client compounds this
by holding standing pull requests that buffer deliveries client-side while
the dispatcher is not polling. The incident produced three complete
invocations from one trigger; the reproduction and full mechanism are in the
execution plan and #327.

Two architectural facts shape the fix:

- Coordination state today is an **event-sourced projection** (idempotent
  upserts fed at-least-once, `control_plane/coordination_consumer.rs`). It
  can absorb duplicates but cannot *arbitrate* them — there is no atomic
  claim primitive anywhere in the coordination plane.
- The runtime is deliberately built toward a **multi-worker deployment**
  (`Worker` trait seam, ADR-0031's `fqd` direction). Any correctness-bearing
  state must therefore live in shared coordination infrastructure, not in a
  process-local store.

## Decision

**Safety comes from a durable, identity-keyed claim adjudicated by an atomic
create; the broker's responsibility ends at the ack that follows the claim.
No timing parameter appears in the correctness argument.**

1. **Trigger identity.** A trigger is identified by
   `(stream epoch, stream sequence)` — the JetStream-assigned sequence of the
   message on `fq-triggers`, plus the stream's creation timestamp (guards
   sequence reuse across stream recreation). Every redelivery of a message
   carries the same identity.

2. **The trigger inbox.** A claim registry in the coordination plane, backed
   by a **NATS KV bucket** (`fq-trigger-claims`; JetStream-durable, same
   broker as the transport). Acquisition is the KV **revision-0 `create`** —
   an atomic create-if-absent; the CAS is the arbiter. The claim record is
   the trigger's **durable intent**: identity, subject, payload, delivered
   count, a preallocated invocation id, the claimant worker id, a start
   attempt counter, and a disposition
   (`accepted → started → completed | failed | dead_letter`).

3. **Responsibility handover at the ack.** The dispatcher claims first, then
   acks. From that moment the broker is out of the trigger's story: the
   inbox owns scheduling, retry, recovery, and dead-lettering. An ack that
   is lost (crash between claim and ack) is harmless — the redelivery meets
   the claim and is dropped.

4. **Idempotent redelivery.** A delivery whose identity is already claimed
   is acked and dropped, loudly (log + counter + `fq doctor` visibility).
   This holds for broker redeliveries, client-buffered stale copies, and any
   future delivery-duplication mechanism — the claim does not care why a
   duplicate exists.

5. **Invocations start from the claim.** The runner receives the claim
   record, not the in-flight NATS message. Recovery extends the existing
   `scan → categorise → resume` one step earlier in the lifecycle: an
   `accepted`-but-unstarted claim is startable work, exactly as an in-flight
   WAL row is resumable work (ADR-0027's "deploys are controlled recovery",
   applied to triggers). Multi-worker: a claim is normally recovered by its
   claimant; claims whose claimant is **stale** (per the existing
   `coordination_worker` heartbeat/stale sweep) may be **adopted** by
   another worker via a CAS update of the claimant field. Staleness gates
   only *when adoption is attempted*; the CAS guarantees at most one adopter
   wins, so liveness heuristics never touch safety.

6. **Poison and dead-lettering split at the handover.** Pre-claim (claim
   write fails — store unreachable): NAK, broker `max_deliver` + the #169
   advisory machinery apply, unchanged. Post-claim: the inbox bounds start
   attempts and moves the claim to `dead_letter`, emitting the existing
   `trigger_exhausted`-style terminal event. Expiry is a disposition, not a
   timeout: the bucket uses **no `max_age`** — an unstarted claim is either
   started, adopted, or dead-lettered, never silently dropped.

7. **Pull discipline.** The dispatcher issues **one pull request per held
   permit** (single bounded request, no streaming prefetch, no background
   renewal). Triggers the daemon cannot start immediately stay on the
   server — restoring the drain property ADR-0027 depends on. This is
   hygiene (latency, drain, blast radius), not safety; safety is items 2–4.

8. **Deletions.** The `DurableStart` channel, the ack-select loop in
   `TriggerDispatcher::handle`, and the post-run ack/NAK choreography for
   in-flight messages are removed. #41's invariants are preserved
   structurally (see the invariant map in the execution plan): the claim
   *is* the first durable write, so the window #41 closed no longer exists.

### Alternatives considered

- **Tune ack-wait/backoff:** shrinks the window, keeps timing in the safety
  argument. Rejected (retained only as redelivery pacing for pre-claim
  crashes).
- **JetStream publish-side dedup / double-acks:** `Nats-Msg-Id` covers
  publish duplication (already in place, 120s window); nothing consumer-side
  offers exactly-once processing. Rejected as insufficient.
- **Fix the client:** the async-nats standing-pull behaviour deserves an
  upstream report, but a client's internals are not a correctness
  foundation. Rejected as the safety story.
- **Per-worker SQLite claims:** correct for a single-daemon deployment,
  invisible to siblings in a multi-worker one. Rejected by the multi-worker
  requirement.
- **Claims as projected events:** projections are eventually consistent and
  cannot arbitrate a race. Rejected; the inbox is deliberately the first
  *authoritative* (non-projected) coordination state, and sets the precedent
  for how such state is held (KV + CAS).

## Consequences

- The exactly-once argument is closed under delivery count, delivery timing,
  client buffering, crash, drain, and redeploy: `create` succeeds once per
  identity, and everything downstream keys off the claim.
- Accepted work survives drain and deploys in the store; the broker queue
  returns to being *only* a transport.
- The dispatcher sheds its most delicate machinery (the mid-flight ack
  dance); the inbox's disposition field makes trigger state inspectable
  (`fq doctor`, dashboard) where it was previously implicit in broker
  redelivery state.
- The step-0 preamble's `attempt` becomes the claim's start-attempt counter
  (the broker `delivered` count is recorded on the claim for forensics) —
  #87 semantics shift accordingly.
- New operational surface: the KV bucket needs creation-at-startup, explicit
  GC of terminal claims (retention ≥ the trigger stream's 24h `max_age` so
  late redeliveries still meet their claim), and doctor checks for orphaned
  or long-unstarted claims.
- Replication of the bucket follows the broker's (replicas=1 today); losing
  the broker loses transport and registry together — one failure domain,
  which is the same blast radius as today.
