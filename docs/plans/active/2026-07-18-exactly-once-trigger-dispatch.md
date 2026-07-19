# Exactly-once trigger dispatch — execution plan

**Status:** active (2026-07-19). Highest-priority correctness fix. Written up
from the 2026-07-18 duplicate-PR incident (issue #189 → PRs #322/#323/#324,
board capture #327); root cause reproduced empirically against a scratch
broker before this plan was written. Decision record:
[ADR-0032](../../adrs/draft/0032-exactly-once-trigger-dispatch.md).

**Decisions (2026-07-19, maintainer):** go **straight to the end-state** (no
interim per-worker claim gate — pre-prod, no detour we'd delete later);
**assume a multi-worker world** in the design; no attachment to the existing
DurableStart mechanism, but its **tested invariants must be preserved**
(§5.3 map); backoff retune approved; watcher duplicate alarm approved with
**existing provenance-stamping behaviour preserved**.

**The one-line principle:** JetStream gives at-least-once *delivery*; nothing
built on delivery timing can give exactly-once *processing*. Safety comes
from a durable, identity-keyed claim adjudicated by an atomic create — ack
deadlines, backoff ladders, and client pull behavior may affect latency and
efficiency, but never appear in the correctness argument.

## 1. The incident

One watcher trigger for issue #189 (published 21:24:33Z) produced **three**
full `m0-issue-fix` invocations and three near-identical PRs against the same
issue:

| Invocation | Created (UUIDv7 ms) | Delivery | PR | Cost |
|---|---|---|---|---|
| `019f772c-fa47…` | 21:41:04.199 | `attempt: 1` | #322 | ~$1.9 |
| `019f7731-99d4…` | 21:46:07.188 | `attempt: 2` | #323 | $1.87 |
| `019f7734-2f9c…` | 21:48:56.604 | `attempt: 3` | #324 | ~$1.9 |

The `attempt:` value is the step-0 environment preamble's copy of
`msg.info().delivered` (#87, `runner.rs` `invocation_preamble`), so the
transcripts themselves prove all three runs came from **the same JetStream
message, delivered three times** — the watcher published exactly once
(watcher.log, single "triggered agent for issue" line).

Saturation context: the watcher batch triggered five issues (184, 44, 190,
175, 189) in two minutes against `max_concurrent = 4`. The four batch-mates
each ran exactly once; only the fifth trigger — the one that had to *queue* —
duplicated. Each duplicate invocation started at the exact moment an earlier
invocation freed an executor permit (21:41:04, 21:46:07, 21:48:56).

## 2. Root cause — three stacked layers

**Layer 1 — config: the effective ack deadline is 1 second.**
`bus.rs:103` `TRIGGER_RETRY_BACKOFF = [1s, 5s, 30s, 120s]` doubles as the
consumer's `backoff`, and JetStream uses `backoff[0]` (1s) as the effective
ack-wait. Comments in `dispatcher.rs` still claim a "30s ack-wait". Any
trigger not acked within 1s of *delivery* becomes redeliverable. Invocations
run for 20–30 minutes; a trigger that waits for a permit blows this deadline
by three orders of magnitude.

**Layer 2 — client: "permit-before-pull" does not keep triggers on the
server.** `dispatcher.rs:215-232` documents the assumption that with
`max_messages_per_batch(1)` and a permit acquired before each
`messages().next()`, "excess triggers truly stay queued on the server". This
is **false** for async-nats 0.38: the pull `Stream` spawns a background task
(`pull.rs`, `Stream::stream`) that re-issues pull requests on a ~35s
expiry-renewal cycle *even when the application never polls*. A trigger
published while all permits are busy is therefore **delivered** into a
standing pull, sits invisible in the client's subscriber buffer with the
server's ack clock running, blows the 1s deadline, and is redelivered into
the next standing pull — accumulating stale copies client-side. (The storm
capped at 3 copies, not `max_deliver = 5`, only by an accident of the same
client internals: the background task's result channel has capacity 1 and
wedges when the app isn't polling, stopping renewals after two in-window
deliveries; the third was snatched by the first post-starvation poll.)

**Layer 3 — architecture: ack-on-durable-start is a timing argument.**
#41's ack-on-durable-start (`dispatcher.rs:434-492`, `runner.rs:1503-1507`)
acks ~10ms after the dispatcher *begins processing* a delivery. It worked
exactly as designed in the incident — each handler acked its copy — but an
ack can neither beat a deadline that expired minutes before the app ever saw
the message, nor recall duplicate copies already buffered in the client. The
design implicitly assumed *delivery ≈ processing-start*. Under saturation
that assumption is unboundedly wrong, and no ack timing can repair it.

Empirical confirmation: a probe against a scratch broker (async-nats 0.38,
identical consumer config, 100s simulated saturation) reproduced the exact
incident signature — two buffered deliveries during the busy window, a third
snatched at the first poll, dequeued one per freed permit as `delivery=1/2/3`,
first ack fully acking the message server-side while the stale buffered
copies still replayed. Reproduction recipe in §10.

## 3. Why tuning cannot fix this

Raising `backoff[0]` to an hour shrinks the window; it does not close it. A
long GC pause, a slow WAL write, a redeploy that leaves a message unacked, a
future client library change, or simply a saturation period longer than the
chosen deadline re-opens it. Every timing knob moves probability around;
none of them changes the fact that the broker is an at-least-once transport.
The fix is to make duplicate *processing* impossible regardless of how many
times the broker (or a buggy client buffer) presents the same message.

## 4. Target invariants

- **I1 (safety, timing-free):** for each trigger identity, at most one
  invocation ever starts. Enforced by an atomic create in a durable shared
  store — the arbiter is a CAS, never a clock.
- **I2 (liveness):** every published trigger eventually yields exactly one
  invocation or an explicit dead-letter disposition. Broker redelivery
  retries only the pre-claim window; the inbox owns everything after.
- **I3 (handover):** once the claim is durably created and the message
  acked, the broker is out of the trigger's story — scheduling, retry,
  recovery, and dead-lettering are store-driven. A lost ack is harmless
  (redelivery meets the claim, is dropped).
- **I4 (no hidden buffering):** no pull request is outstanding unless a
  permit is held. Deliveries the daemon cannot start immediately stay on
  the server — restoring the drain property ADR-0027 relies on.
- **I5 (observability):** duplicate deliveries are dropped *loudly* (log +
  counter + `fq doctor`), and the watcher alarms on >1 PR per issue while
  still stamping provenance on every PR as today.

**Trigger identity** = `(stream epoch, stream sequence)` from
`msg.info().stream_sequence` plus the `fq-triggers` stream's creation
timestamp (guards against sequence reuse if the stream is ever
purged/recreated). Server-assigned, shared by all redeliveries, available on
every delivery.

## 5. Design: the trigger inbox (ADR-0032)

### 5.1 The claim registry

A NATS KV bucket **`fq-trigger-claims`** in the coordination plane —
JetStream-durable on the same broker as the transport, shared by all
control-plane/worker instances (multi-worker assumed from day one; today's
single daemon is the degenerate case). Acquisition is the KV **revision-0
`create`**: atomic create-if-absent, the sole arbiter of I1. This is
deliberately the first *authoritative* (non-projected) coordination state —
the existing coordination store is an idempotent-upsert projection
(`coordination_consumer.rs`) and cannot arbitrate races.

Claim record (key `«epoch».«seq»`):

```text
identity      stream epoch + stream sequence (the key)
subject       fq.trigger.<agent>
payload       full trigger payload (the durable intent)
delivered     broker delivery count at claim time (forensics)
invocation_id preallocated UUIDv7
claimant      worker id that holds the claim
attempts      inbox start attempts
disposition   accepted → started → completed | failed | dead_letter
claimed_at / updated_at
```

Dispatch flow (replaces `handle()`'s ack-select machinery):

1. Parse delivery → build claim record → KV `create`.
2. **Created** → ack the message (I3 handover) → start the invocation *from
   the claim record* (never from the in-flight message).
3. **Already exists** → duplicate delivery: log at INFO with identity and
   `delivered`, bump the drop counter, ack, return.
4. `create` fails (store unreachable) → NAK with backoff. Pre-claim, the
   broker's `max_deliver` + #169 advisory machinery apply unchanged.

### 5.2 Lifecycle, recovery, multi-worker adoption

The runner starts from the claim and moves its disposition
`accepted → started` (alongside the first WAL write) and to a terminal
disposition on completion. Recovery extends the existing
`scan_in_flight → categorise → resume` (`worker/recovery.rs`) one step
earlier: an `accepted`-but-unstarted claim is startable work, exactly as an
in-flight WAL row is resumable work — ADR-0027's "deploys are controlled
recovery" applied to triggers.

Multi-worker: a claim is normally recovered by its claimant. Claims whose
claimant is **stale** — per the existing `coordination_worker`
heartbeat/stale sweep — may be **adopted**: a CAS update of the claimant
field, so at most one adopter wins regardless of how many race. Staleness
gates only *when* adoption is attempted; the CAS keeps timing out of safety.

Poison post-claim: the inbox bounds `attempts`; exhaustion moves the claim
to `dead_letter` and emits the existing `trigger_exhausted`-style terminal
event. **Expiry is a disposition, not a timeout**: the bucket uses no
`max_age` — an unstarted claim is started, adopted, or dead-lettered, never
silently dropped. Terminal claims are GC'd after 48h (≥ the trigger
stream's 24h `max_age`, so the latest possible redelivery still meets its
claim).

Crash/duplication analysis (the safety argument, exhaustively):

| Window | Durable state | Outcome |
|---|---|---|
| Duplicate delivery, any time, any cause | claim exists | dropped + acked — **I1** |
| Crash before `create` | nothing | message unacked → broker redelivers → claimed then — **I2** |
| Crash between `create` and ack | claim, message unacked | redelivery meets claim → dropped + acked — **I3** |
| Crash between ack and start | claim `accepted`, no WAL | claimant recovery (or adoption if claimant stays down) starts from the claim |
| Crash after start | claim `started` + WAL | WAL resume, exactly once (existing recovery); redeliveries impossible (acked) and would be dropped anyway |
| Two instances race one delivery / adoption | one CAS winner | loser observes existing record/revision, drops or backs off |

### 5.3 Invariant preservation map (#41, #169, ADR-0027)

The mechanism changes; the tested guarantees carry over. Existing tests are
ported to assert the same observable, not deleted:

| Existing invariant (mechanism) | New expression | Test carrier |
|---|---|---|
| No trigger lost between delivery and durable start (#41: unacked until first WAL write) | Unacked until claim `create`; claim *is* the first durable write | crash-before-`create` and crash-before-ack cases in §7 |
| Long invocation never re-run past ack-wait (#41 ack timing) | Structural: acked at claim; duplicates CAS-dropped | incident-replay integration test |
| Transient pre-WAL failure retried, permanent not (dispatcher NAK/ack fates) | Pre-claim: NAK path unchanged. Post-claim: inbox `attempts` + dead-letter disposition | inbox retry/poison tests |
| Poison trigger surfaced, not looped (#169 dead-letter + advisory) | Pre-claim: unchanged. Post-claim: `dead_letter` disposition + terminal event | dead-letter tests both sides of the handover |
| Drain leaves un-started work for the next binary (ADR-0027) | Un-pulled stays on server (I4); accepted-but-unstarted survives in the inbox | drain test with queued + accepted triggers |
| Resume/drain observational equivalence (runner) | Untouched — invocation start reads the claim exactly once | existing suite re-run |

### 5.4 Attempt semantics

The step-0 preamble's `attempt:` becomes the claim's start-attempt counter;
the broker `delivered` count at claim time is persisted on the claim for
forensics. This also closes the #87 remnant (resumed runs reconstructing
`attempt` as 1) — the counter now lives in durable state.

### Alternatives considered and rejected

Recorded in ADR-0032: timing tuning (retained only as pre-claim redelivery
pacing); JetStream publish-side dedup / double-acks (wrong side of the
problem); upstream client fix (worth reporting, not a correctness
foundation); watcher-side dedup (sees creation, not delivery); per-worker
SQLite claims (invisible to siblings — rejected by the multi-worker
requirement); claims as projected events (projections cannot arbitrate).

## 6. Execution plan (PR-sized)

| PR | Contents | Gates |
|---|---|---|
| **PR-0** | This plan + ADR-0032 (docs-only). | Docs lint |
| **PR-1** | Identity plumbing: carry `stream_sequence` + `delivered` through `run_invocation` into `TriggerPayload`; persist `trigger_stream_seq` on `invocation_state` (schema v9 → v10) for forensics/doctor joins. | `just ci`; migration round-trip tests |
| **PR-2** | Claim registry: `fq-trigger-claims` bucket (created at startup like the streams), `TriggerClaims` seam in the coordination plane — `claim`, `mark_started`, `mark_terminal`, `adopt`, `scan_accepted`, GC. No dispatcher wiring. | Unit + property tests over the claim state machine, incl. adoption races |
| **PR-3** | Inbox dispatch: claim → ack → start-from-claim; delete `DurableStart` channel + ack-select loop + post-run ack choreography; duplicate-drop path (log + counter); startup scan of own `accepted` claims (no liveness hole while single-instance); preamble `attempt` from the claim. | Incident-replay integration test (§7); #41-map tests green |
| **PR-4** | Multi-worker recovery: adoption gated on `coordination_worker` staleness; inbox `attempts` bound + `dead_letter` disposition + terminal event; full crash-window suite + DST sweep. | §7 crash/DST suites |
| **PR-5** | Pull discipline (I4): one bounded pull request per held permit (no streaming prefetch, no background renewal). | Saturation test: `num_waiting == 0`, delivery cursor frozen while permits exhausted |
| **PR-6** | Config + truth: `TRIGGER_RETRY_BACKOFF` → `[30s, 2m, 10m, 30m]` (pre-claim pacing only); consumer drift-update (`bus.rs:553`) reconciles `ack_wait` too; fix stale "30s ack-wait" comments. | Consumer-info assertion in NATS suite |
| **PR-7** | I5: watcher warns + applies `fleet:needs-decision` on `pr_count > 1` **while preserving today's provenance stamping on every PR**; `fq doctor` checks (duplicate seqs, `accepted` claims older than threshold, stale-claimant claims, historical `attempt > 1`). | Watcher unit tests |

Order: PR-1/PR-2 in parallel now; PR-3 is the fix landing; PR-4 completes
the ADR; PR-5–7 are independent hardening. PR-3 before PR-4 has no liveness
hole for the current single-daemon dogfood (own-claim startup scan); PR-4 is
required before any actual multi-worker deployment. Upstream async-nats
report filed alongside (tracked on #327, not a PR here).

## 7. Test plan

- **Oracle:** for any delivery history of one trigger identity — any number
  of deliveries, any interleaving with crashes/drains — exactly one
  invocation starts; every other delivery is dropped-with-ack; the claim
  ends in exactly one terminal disposition.
- **Unit:** claim create/conflict; adoption CAS (two adopters, one winner);
  GC (terminal-only, 48h); disposition transitions.
- **Integration (per-test broker, #262 harness):** port the incident probe —
  `max_concurrent = 1`, slow stubbed invocation, second trigger published
  into saturation, forced redeliveries; assert exactly one invocation, N−1
  duplicate-drops, message acked at claim time.
- **Crash windows:** each row of the §5.2 table exercised — kill before
  `create`, between `create` and ack, between ack and start, after start;
  assert the table's outcome.
- **Property/DST sweep:** per-identity state machine (`unclaimed → accepted →
  started → terminal`, events: deliver / claim / ack / crash / recover /
  adopt / start / complete / exhaust) driven by randomized interleavings
  across 1–3 simulated instances; invariants I1–I3 checked on every trace.
- **Invariant map (§5.3):** each row lands as a named test so the #41/#169/
  ADR-0027 guarantees are visibly preserved, not silently dropped with the
  mechanism.

## 8. Deploy plan

1. Merge order as §6; each PR rides the normal `ops/dogfood/deploy.sh` flow.
2. PR-1 bumps `WORKER_SCHEMA_VERSION` 9 → 10: **rollback across the bump is
   unguarded** (deploy SOP) — verify before any `deploy.sh <sha>` rollback.
3. PR-2/3 create the KV bucket at startup (same pattern as stream
   creation); no manual broker step. Verify post-deploy with
   `/jsz` on :8223 (bucket present) and a `fq doctor` run.
4. PR-6's consumer changes apply via the startup drift-update; verify
   `ack_wait` reads 30s, backoff `[30s, 2m, 10m, 30m]`.
5. Deploy PR-3 during fleet idle or after `fq down` drain per SOP. In-flight
   invocations are unaffected (claims are written at dispatch; recovery
   tolerates pre-claim-era invocations having no claim row).

## 9. Incident remediation (operational, independent of the code fix)

- [ ] Adjudicate PRs #322 / #323 / #324 (three parallel refactors of
      `fq-cli/src/main.rs` — they will conflict; pick one, likely #322,
      close the others referencing #327).
- [ ] Re-check issue #189 labels/state after adjudication.
- [ ] File the upstream async-nats report (0.38 `Stream`: background pull
      renewals while the app is not polling; capacity-1 result channel
      wedge). Check ≥0.39 changelogs first.
- [ ] Keep the three invocation archives (cost retention principle) — the
      duplicate runs are themselves evidence.

## 10. Appendix: reproduction recipe

Scratch broker (`docker run --rm -d -p 127.0.0.1:4299:4222 nats:latest -js`),
async-nats 0.38 client. Stream `probe.trigger.>`; pull consumer with
`ack_wait = 1s`, `backoff = [1s, 5s, 30s, 120s]`, `max_deliver = 5`,
`AckPolicy::Explicit`; `stream().max_messages_per_batch(1).messages()`.

1. Publish A; consume; ack +10ms — healthy path, no redelivery.
2. Publish B; stop polling 100s ("all permits busy"). Server-side observer
   shows B delivered twice into standing pulls (~t+27, ~t+62) with
   `ack_pending = 1`, `redelivered = 1` — while the app never polled.
3. Resume polling one message per simulated permit-free (45s apart): app
   receives `delivery=1`, `delivery=2`, `delivery=3`; the first ack fully
   acks the message server-side (`ack_pending = 0`), yet the buffered copies
   still replay. Fourth poll: nothing — matches the incident stopping at 3.

Incident forensics that generalize: UUIDv7 invocation-id timestamps give
creation order; `fq invocation transcript <id> | grep "attempt:"` exposes the
delivery count per run; `/jsz?consumers=true&config=true` gives the deployed
(not documented) consumer config and delivery/ack-floor cursors.
