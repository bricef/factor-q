# ADR-0032: A duplicate trigger delivery is refused by a durable claim; the ack stays at durable start

## Status

Accepted (2026-11-11). Proposed 2026-07-19 as the **trigger inbox**: a
claim registry in a NATS KV bucket, the ack moved to claim time, and
invocations started from the claim. That design was not built. On
2026-11-11 the maintainer decided, on
[#835](https://github.com/bricef/factor-q/issues/835), that the durable
claim which landed on 2026-09-16
([#809](https://github.com/bricef/factor-q/issues/809), PR
[#817](https://github.com/bricef/factor-q/pull/817)) is the project's
exactly-once dispatch design, and that the inbox is not built while one
daemon is the deployment.

This record was rewritten around what is on `main` in the change that
moved it to `accepted/`. It had never been accepted in its earlier form,
so no accepted text was rewritten. The inbox design is kept: summarised
under [The deferred design](#the-deferred-design-the-trigger-inbox)
below, and in full in §5 of the
[closed execution plan](../../plans/closed/2026-07-18-exactly-once-trigger-dispatch.md).

Implementation: complete for what this record decides. The claim and its
stream-epoch key are on `main` (#809, PR #817), as are the ack at
durable start ([#41](https://github.com/bricef/factor-q/issues/41)), the
30-second first-delivery window
([#810](https://github.com/bricef/factor-q/issues/810)) and publish-side
deduplication ([#825](https://github.com/bricef/factor-q/issues/825)).
Two leftovers are open: the retry ladder
([#953](https://github.com/bricef/factor-q/issues/953)) and an operator
surface for the trigger-to-invocation link
([#954](https://github.com/bricef/factor-q/issues/954)).
[What this design does not close](#what-this-design-does-not-close)
lists five gaps that are known and accepted with this record.

Code references are to `main` at `1ce79ab5`. Paths are relative to
`services/fq-runtime/crates/fq-runtime/src/` unless they begin with a
crate name.

## Context

JetStream delivers a message at least once. A trigger that is delivered
twice and handled twice becomes two invocations, and for the dogfood
fleet two invocations of one issue are two pull requests. That has
happened three times, in two shapes.

**The invocation outlived the ack deadline (2026-07-06).** The
dispatcher acked a trigger when its invocation *completed*. An
invocation that ran longer than the consumer's ack window was
redelivered while it was still running, and run again.

**The trigger was redelivered before its invocation had durably started
(2026-07-18, and again on 2026-09-16).** On 2026-07-18 one trigger for
issue 189 produced three invocations and three pull requests
([#327](https://github.com/bricef/factor-q/issues/327)). The fleet was
saturated; the trigger had to wait for a worker permit; the consumer's
effective ack deadline was one second; and the async-nats client held
deliveries in its own buffer while the dispatcher was not polling. The
broker delivered the same message three times, and each copy started an
invocation as an earlier one freed a permit. On 2026-09-16 eight
triggers produced thirteen invocations by a different route to the same
place: triggers held behind a per-agent concurrency cap were kept alive
by an in-progress ack every 250 ms, one tick slipped, and the
redelivered copy walked into a dispatcher that had no way to know it was
a copy (#809). The incident, its root cause in three layers and a
reproduction recipe are in §1, §2 and §10 of the closed plan.

The July answer was a seven-PR design, the trigger inbox. What was built
instead is smaller, and by 2026-09-16 it had closed both shapes for a
single daemon. The question #835 put to the maintainer was whether to
build the inbox anyway.

## Decision

**A delivery is arbitrated against a durable claim row before anything
else happens to it, and a trigger is acked when its invocation makes its
first WAL write. The trigger inbox is not built.**

### 1. The claim's identity is the stream message, in one incarnation of the stream

A claim is keyed on `TriggerKey { stream, stream_epoch, stream_seq }`
(`control_plane/store/trigger_claim.rs`): the stream name
(`TRIGGER_STREAM_NAME`, `fq-triggers`), the time that incarnation of the
stream was created, and the message's sequence in it. Every redelivery
of a message carries the same sequence, whatever caused it.

The trigger's own id cannot be the key. `trigger::delivered` mints a
fresh id for each delivery of a message that arrived without an
`Fq-Trigger-Id` header, so the thirteen invocations of 2026-09-16 had
thirteen ids.

The epoch is in the key because JetStream restarts sequences at 1 when a
stream is recreated. Without it, the first triggers of a rebuilt
`fq-triggers` would meet the `durably_started` rows of the old stream
and be acked and dropped as duplicates, silently, since a dropped
duplicate starts nothing by design. With it, rows left by a stream that
no longer exists are inert, and no operator has to remember to clear a
table. The epoch is the stream's creation time in nanoseconds, read from
stream info by `EventBus::ensure_trigger_stream` and served by
`EventBus::trigger_stream_epoch`. It is read once, at connect: a stream
recreated under a running daemon is not noticed until that daemon
reconnects. No test covers that case.

### 2. The claim is a row in the control-plane store, and arbitration comes first

The row lives in the `trigger_claim` table of the control-plane store
(`control_plane/store.rs`, schema version 2), with a `claimant`, a
`state` (`claimed` or `durably_started`), the `invocation_id` once
there is one, and `claimed_at`. `ControlPlaneStore::claim_trigger` is
one insert-if-absent statement followed by a read of the winning row.

`TriggerDispatcher::handle` calls `claim_delivery`
(`control_plane/dispatcher/claim.rs`) as its first operation: before the
drain check, before the subject is read, before the registry, before
either admission hold, before the payload is parsed and before
`trigger::delivered` names the trigger. A copy that is refused has done
nothing and minted nothing.

| The row says | The dispatcher does |
|---|---|
| No row (`TriggerClaim::Won`) | Proceeds. The row is now `claimed` by this process. |
| `durably_started` (`Started`) | Acks the delivery, logs at `warn`, counts it (`ConsumerLedger::note_duplicate_drop`), and stops. |
| `claimed` by this process (`Held`, same claimant) | Stops. No ack and no NAK, because either would resolve the delivery the first copy is still holding. No log line and no count. |
| `claimed` by another id (`Held`, other claimant) | Adopts the row with `take_over_trigger_claim` and proceeds. See [claimant identity and adoption](#2-claimant-identity-and-adoption). |
| The store or `msg.info()` fails | Stops, leaving the delivery unacked for the broker to redeliver. |

### 3. The ack stays at durable start

`ReducerRunner` fires `DurableStart` after its first
`upsert_invocation_state` (`worker/reducer/runner.rs`, in
`run_loop_inner`). `handle` waits for that signal; on it,
`mark_durable_started` writes `durably_started` and the invocation id to
the claim row with `mark_trigger_started`, and then acks. If the mark
fails, the delivery is left unacked.

The inbox would have acked at claim time and started the work from the
claim. The ack stays where it is for three reasons.

- **It is what bounds the 2026-07-06 shape, on its own.** The message is
  settled seconds into a run, so an invocation of any length is not
  redelivered.
- **Before the first WAL write, the broker is still the retry
  mechanism.** A crash, a drain or a transient failure in that window
  leaves the trigger unacked or NAKs it, and the broker brings it back.
  Nothing in the runtime has to scan for work that was accepted and
  never started.
- **Acking at claim time is only safe with start-from-claim.** Once the
  broker has been told the trigger is handled, the claim row is the only
  record of it. It would have to carry the payload, and something would
  have to find and start claims a dead process left behind. That is the
  inbox's PR-3 and PR-4, which is the work this record declines.

### 4. Every other ack gives the claim back

`ack` and `nak` (`dispatcher/claim.rs`) both call
`release_unstarted_claim`, which deletes the row if and only if it is
still `claimed` (`release_trigger_claim`). So a trigger that is refused
for a bad subject or an unknown agent, dead-lettered, NAKed for a
transient failure before its first WAL write, or requeued from a hold
(`requeue_held` republishes it under a new sequence and acks the old
one) leaves no row behind, and the retry wins its claim cleanly. A
`durably_started` row is never deleted by anything.

Three paths return without acking and leave a `claimed` row on purpose:
the drain check at the top of `handle`, a pause hold interrupted by a
drain (`Admission::Interrupted`), and a `requeue_held` whose publish
failed. All three are on a process that is exiting. The next process has
a new id and adopts the row.

### 5. Which mechanism bounds which incident

| Incident | Bounded by | The other mechanism |
|---|---|---|
| 2026-07-06, invocation outlives the ack window | The ack at durable start | The claim is a second line: a copy arriving after durable start meets a `durably_started` row |
| 2026-07-18 and 2026-09-16, redelivery before durable start | The claim: a copy arriving while the row is `claimed` is refused | The 30 s window (#810) and the hold keepalive (`HOLD_KEEPALIVE`, `hold_tick`, `acquire_run_permit`) make the redelivery rare. They are timing, and are not part of the safety argument |

The 2026-07-18 conditions do not produce a redelivery on `main` the way
they did in July. Since
[#733](https://github.com/bricef/factor-q/issues/733) the consume loop
holds no worker permit and pulls each delivery as it arrives; a trigger
that cannot start yet is *held* in the dispatcher and kept alive with
in-progress acks. A redelivery before durable start now needs one of two
things: a keepalive tick that slips (2026-09-16), or a start that takes
longer than the ack window between its claim and its first WAL write.
`handle` sends no keepalive during that part of `run_invocation`, which
in `ReducerRunner::run_loop_for` covers workspace provisioning, starting
the agent's grant-bearing MCP servers and publishing `triggered`. The
replay test below uses the second route, because it needs no fault
injection: the deadline simply passes.

### 6. The trigger inbox is not built

The inbox's main payoff is supporting more than one worker: a claim
every instance can see, adoption of a dead worker's claims, and work
that stays on the broker until some worker has capacity for it.
Multi-node is deliberately on hold
([#414](https://github.com/bricef/factor-q/issues/414)). The inbox would
be the largest single change in the
[2026-09-03 production-readiness review](../../reviews/2026-09-03-production-readiness-review.md)'s
Phase 2, touching the dispatcher's whole admission path, and it would be
made for a failure the single-daemon deployment no longer reaches by
either of the routes that produced the incidents.

That reason is the maintainer's, recorded on #835. Two qualifications
belong beside it, because they were found while proving the decision and
are not in the issue:

- The inbox would also have closed a single-daemon gap that this design
  leaves open: a crash between the first WAL write and the claim's
  `durably_started` update. It is the first entry under
  [What this design does not close](#what-this-design-does-not-close),
  and it can be closed without the inbox.
- What this record accepts is, in the draft's own words, the alternative
  it rejected: "Per-worker SQLite claims: correct for a single-daemon
  deployment, invisible to siblings in a multi-worker one". Both halves
  of that sentence are still true. The decision is that the first half
  is the deployment.

The decision carries a named condition for reopening it, at the end of
[What this means for more than one worker](#what-this-means-for-more-than-one-worker).

## What holds, and the test that shows it

Every row names the code it rests on and a test that was run for this
record.

"Unit" tests are in `control_plane/dispatcher.rs` and
`control_plane/store/tests.rs`. The dispatcher ones run against a real
`nats-server` process. Those that exercise the claim also use a real
control-plane SQLite store; they pull a message themselves and pass it
to `handle` directly. No unit test runs the consume loop with a claim
store installed.

"Replay" tests are in
[`fq-runtime/tests/july_incident_replay.rs`](../../../services/fq-runtime/crates/fq-runtime/tests/july_incident_replay.rs).
They run `TriggerDispatcher::run`, the daemon's own entry point, on the
production durable with its production ack window, with the claim store
installed as `fq-daemon/src/hosted.rs` installs it. Every redelivery in
them is the broker's own.

**In every test the worker is a stub** at the `Worker` seam, which fires
`DurableStart` when the test says its first WAL write has landed. No
reducer, WAL or model call is involved.

| Property | Rests on | Test |
|---|---|---|
| A trigger is acked at its invocation's first WAL write, while the invocation is still running | `mark_durable_started` | Unit: `started_claim_acks_duplicate_without_second_invocation` (first half). Replay: `an_invocation_that_outlives_the_ack_window_is_not_redelivered` |
| An invocation that outlives the ack window is delivered once (2026-07-06) | The same | Replay: `an_invocation_that_outlives_the_ack_window_is_not_redelivered`. The invocation stays in flight for 33 s against a 30 s window; the broker reports one delivery and no redelivery, and the claim drops nothing |
| A trigger is not acked before its first WAL write, so a crash there loses nothing | The `select!` in `handle` | Unit: `trigger_is_not_acked_before_the_first_wal_write` |
| A transient failure before the first WAL write is retried; a permanent one is not | `trigger_fate` | Unit: `transient_failure_before_first_wal_write_naks_for_redelivery`, `permanent_failure_before_first_wal_write_acks`, `ambiguous_wal_resume_verdict_acks_without_redelivery` |
| A copy redelivered while the first is held un-started starts nothing and does not resolve the first copy's delivery (2026-09-16) | `claim_delivery`, same-claimant `Held` arm | Unit: `redelivered_held_trigger_is_dropped_and_its_original_copy_starts_once`. The redelivery is forced with a NAK; saturation is a per-agent cap |
| A trigger redelivered before its first WAL write starts one invocation, with a second trigger queued behind it (2026-07-18) | The same | Replay: `a_trigger_redelivered_before_its_first_wal_write_starts_once`. One worker permit; the redelivery is the broker's own, from the 30 s deadline passing; each trigger starts once as `attempt: 1` |
| A copy arriving after durable start is acked, counted and starts nothing | `claim_delivery`, `Started` arm | Unit: `started_claim_acks_duplicate_without_second_invocation` (second half). The copy is a clone of the first delivery fed back in, not a broker redelivery |
| A waiting trigger is kept alive across a whole ack window | `hold_tick`, `acquire_run_permit` | Unit: `a_permit_wait_keeps_the_trigger_alive_and_is_interrupted_by_drain`, `a_paused_models_trigger_is_held_and_started_once_after_the_pause` |
| A released claim can be won again; a requeue releases the old sequence | `release_trigger_claim` | Unit: `held_claims_stop_same_worker_and_are_adopted_from_another_worker` (middle), `requeue_held_releases_old_stream_sequence_claim` |
| A `claimed` row under another id is adopted on the next delivery | `take_over_trigger_claim` | Unit: `held_claims_stop_same_worker_and_are_adopted_from_another_worker` (end) |
| A recreated stream starts a new claim space | `TriggerKey::stream_epoch` | Unit: `a_claim_from_an_earlier_stream_incarnation_does_not_drop_a_fresh_delivery`, `trigger_claim_state_machine_round_trip` |

Two things in that table are thinner than they look. No test has the
broker redeliver a trigger *after* its durable start, because the ack
prevents exactly that; the `Started` arm is exercised only by feeding a
copy in by hand. And no test checks where the real `ReducerRunner` fires
`DurableStart`: that it fires after the first `upsert_invocation_state`
and not before rests on reading `run_loop_inner`.

### The runs

Run on 2026-11-11. The unit tests ran at `1ce79ab5`; the replay tests
ran on that commit plus the test file
([#955](https://github.com/bricef/factor-q/pull/955)).

```console
$ mise exec node@24.21.0 -- just test-runtime --lib -- \
    started_claim_acks_duplicate_without_second_invocation \
    trigger_is_not_acked_before_the_first_wal_write \
    redelivered_held_trigger_is_dropped_and_its_original_copy_starts_once \
    held_claims_stop_same_worker_and_are_adopted_from_another_worker \
    requeue_held_releases_old_stream_sequence_claim \
    transient_failure_before_first_wal_write_naks_for_redelivery \
    permanent_failure_before_first_wal_write_acks \
    ambiguous_wal_resume_verdict_acks_without_redelivery \
    a_permit_wait_keeps_the_trigger_alive_and_is_interrupted_by_drain \
    a_paused_models_trigger_is_held_and_started_once_after_the_pause \
    trigger_claim_state_machine_round_trip \
    a_claim_from_an_earlier_stream_incarnation_does_not_drop_a_fresh_delivery
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 1013 filtered out; finished in 61.22s

$ mise exec node@24.21.0 -- just test-runtime --test july_incident_replay
test an_invocation_that_outlives_the_ack_window_is_not_redelivered ... ok
test a_trigger_redelivered_before_its_first_wal_write_starts_once ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 33.28s
```

Each mechanism was then disabled with a local change that was not
committed, and the same tests run again.

With the claim's arbitration off (`claim_delivery` returning `Proceed`
from its `Started` and same-claimant `Held` arms), the 2026-07-18 replay
fails with the incident itself, a third start for the stalled trigger
stamped `attempt: 2`:

```text
test a_trigger_redelivered_before_its_first_wal_write_starts_once ... FAILED
assertion `left == right` failed: each trigger starts exactly one invocation, as its first delivery. A third start for the stalled trigger, stamped `attempt: 2`, is the 2026-07-18 incident
  left: [Start { trigger: 01a1c9d5-0e52-…, attempt: Some(1) }, Start { trigger: 01a1c9d5-0e6c-…, attempt: Some(1) }, Start { trigger: 01a1c9d5-0e52-…, attempt: Some(2) }]
 right: [Start { trigger: 01a1c9d5-0e52-…, attempt: Some(1) }, Start { trigger: 01a1c9d5-0e6c-…, attempt: Some(1) }]
```

The three unit tests of the claim fail with it:
`redelivered_held_trigger_is_dropped_and_its_original_copy_starts_once`
("the duplicate is refused rather than held: Elapsed(())"),
`started_claim_acks_duplicate_without_second_invocation` ("the duplicate
is answered rather than held: Elapsed(())") and
`held_claims_stop_same_worker_and_are_adopted_from_another_worker`. The
2026-07-06 replay stays green with the claim off, which is the split in
[§5](#5-which-mechanism-bounds-which-incident).

With the ack removed from `mark_durable_started`, the 2026-07-06 replay
fails:

```text
test an_invocation_that_outlives_the_ack_window_is_not_redelivered ... FAILED
the trigger must be acked at the invocation's first WAL write, while the invocation is still in flight (num_ack_pending=1 after 5s)
```

and so does `started_claim_acks_duplicate_without_second_invocation`
("trigger not acked while the invocation is still in-flight
(num_ack_pending=1) — the redelivery-storm regression").

**The amended Phase 2 exit criterion.** The production-readiness review
asks that "the incident-replay integration test from the exactly-once
plan passes". That test was the gate of the plan's PR-3, which is not
built. Under this record the criterion is: the two tests in
`fq-runtime/tests/july_incident_replay.rs` and the twelve unit tests
named above pass in `just runtime-ci`. The review itself is a snapshot
and is not edited; the amendment is recorded here and in
[STATUS.md](../../../STATUS.md).

## What this design does not close

These were found by reading the code for this record, except the second,
which a test found. Each is stated as what the code does. None is one of
the incidents, and none has its own issue yet.

1. **A crash between the first WAL write and the claim's
   `durably_started` update starts the trigger twice.** The WAL row is
   written by the runner to the worker store; the claim row is updated
   afterwards by the dispatcher in the control-plane store
   (`mark_durable_started`), and the ack follows that. A process that
   dies between the two leaves an in-flight WAL row, a `claimed` row and
   an unacked message. The next process resumes the invocation from the
   WAL (`fq-daemon/src/recovery.rs`, `classify_in_flight`), and when the
   broker redelivers the trigger it finds a `claimed` row under another
   id, adopts it and starts a second invocation. Nothing on the claim
   row says the first one started. The window is the time between two
   SQLite commits, normally milliseconds. Derived from the code; not
   reproduced by a test. This is the residual the
   [2026-07-25 cleanroom review](../../reviews/2026-07-25-factor-q-cleanroom-review.md)
   named in its §4.2 and left to the inbox plan, where the claim is the
   first durable write and carries a pre-allocated invocation id. It
   does not need the inbox: a claim row that carries the invocation id
   from claim time would let the adopting process look for that
   invocation's WAL row before starting anything. #954 says the worker
   schema needs no trigger column because the claim row already links a
   delivery to its invocation. That holds for forensics, and not for
   this window, where the link is the thing that has not been written
   yet.

2. **A worker that fires `DurableStart` and returns within the same poll
   is treated as one that never started.** `handle` learns of a durable
   start only by polling the signal in its `select!` loop. If the
   invocation future fires the signal and completes in one poll, the
   loop breaks on the result with `durable_start_observed` still false,
   and the trigger takes the pre-WAL path: the claim row is released
   instead of marked, and a transient error would NAK a run that had
   durably started. The first version of the replay's stub worker did
   exactly this and the replay's claim-row assertion caught it. Whether
   any path through `ReducerRunner` can do it was not established; every
   one read for this record awaits the store or the broker between the
   WAL write and its return. The dispatcher does not check.

3. **A live process can hold a claim it will never settle.** The
   same-claimant `Held` arm assumes the first copy is still going to
   settle the delivery. After a panic in the invocation task, or a
   `mark_trigger_started` that is refused on every retry when the ack
   the dispatcher sends in its place is then lost
   ([#967](https://github.com/bricef/factor-q/issues/967)), it never
   will: the row stays `claimed`, the message stays unacked, and every
   redelivery is dropped by that arm until `TRIGGER_MAX_DELIVER` (5) is
   reached and the advisory watch dead-letters the trigger as
   `trigger_exhausted`. In the second case the invocation is running, so
   the verdict is false.
   The same happens to a start that takes longer than the whole retry
   ladder to reach its first WAL write. Derived from the code, and the
   comment on that arm in `claim.rs` describes the hazard; not
   reproduced by a test. #954's scope includes a `fq doctor` check for
   old `claimed` rows, which detects this and does not repair it.

4. **A duplicate dropped before durable start leaves no trace.** The
   `Started` arm logs and counts. The same-claimant `Held` arm does
   neither, and the count (`duplicate_dropped` in the consumer ledger)
   is in memory and resets when the daemon restarts. The plan's
   invariant I5 asked for every duplicate to be dropped loudly, with a
   log line, a counter and a `fq doctor` check. That holds for one of
   the two arms.

5. **The claim table grows without bound.** `durably_started` rows are
   never deleted and `control_plane/retention.rs` does not sweep the
   table: one row per started trigger for the life of the instance. See
   [retention](#6-retention-of-claim-rows) for how long a row has to
   live. Noted on #954 as a Phase 3 bounded-growth item.

## What this means for more than one worker

The design is correct for one daemon because of three facts about one
daemon, and each of them is a fact about the deployment, not something
the code checks:

- one process consumes `fq-triggers`;
- that process is the only writer of the claim table;
- a claimant id other than the process's own belongs to a process that
  has exited.

"Multi-node" is used in this repository for four different things. They
break those facts differently, so the answers below are given per shape.

| Shape | Where it is described | Processes consuming `fq-triggers` |
|---|---|---|
| **A. A graph of agents on one daemon.** "Node" means a node of an execution graph | [#414](https://github.com/bricef/factor-q/issues/414); the [two-node vertical plan](../../plans/active/2026-07-07-graph-executor-two-node-vertical.md) | One |
| **B. One control plane, several workers.** The control plane takes the trigger, picks a worker and hands it the invocation | [data architecture](../../design/committed/data-architecture.md) §5.7, §7.2, §11 | One |
| **C. Several whole daemons on one broker.** "N copies of the same thing" | [agent OS architecture](../../design/aspirational/agent-os-architecture.md); the inbox design's own assumption | Several |
| **D. Remote workers behind a bridge.** Shape B with a second hop | [edge and bus transport](../../design/aspirational/edge-and-bus-transport.md) | One |

Shape A leaves all three facts intact. The two-node plan makes the
executor "the one dispatch path", where "`trigger → traversal` subsumes
`trigger → invocation`", so a traversal is started by the same
dispatcher under the same claim. One thing changes: a claim row records
one `invocation_id`, and a traversal has several. What the row names for
a traversal has to be decided when the executor is designed. No executor
code exists, so nothing here is verified against one.

The six questions that follow are about shapes B, C and D.

### 1. Where the claim lives, and which processes can see it

**Today.** In `control-plane.db`, a SQLite file under the daemon's
`[cache] directory` (`RuntimeDbPaths::under`), opened by
`ControlPlaneStore::open` in WAL mode with no exclusive lock. It is
local to one daemon's filesystem. The only thing that stops a second
daemon is the instance lock, and the instance lock is the edge
listener's address (`EdgeListener::bind` at the top of `run_daemon`),
not a lock on the state.

**With more than one.**

- *Shapes B and D:* one dispatcher and one store. The claim is as sound
  as it is today.
- *Shape C, separate state directories:* each daemon has its own table,
  and a claim made by one is invisible to the others. All of them bind
  the same durable (`CONSUMER_NAME`, `fq-dispatcher`), so the broker
  hands each delivery, and each redelivery, to whichever has a pull
  open. A redelivery that lands on a different daemon finds no row, wins
  its claim and starts. There is no arbitration.
- *Shape C, one state directory:* nothing refuses two daemons with
  different `[edge] bind` addresses on the same cache directory. The
  table is then shared and the insert is atomic across processes, so
  `Won` is exclusive. Adoption makes this the worst case, as the next
  question shows.

None of this is covered by a test: no test runs two dispatchers against
one stream.

**The inbox.** A NATS KV bucket, `fq-trigger-claims`, on the broker
every instance already shares, with the KV revision-0 `create` as the
arbiter (plan §5.1; PR-2).

**Before multi-node.** Choose the shape first. For B and D the table can
stay where it is. For C the claim has to move to a store every consumer
shares, which is PR-2, and a second daemon on an existing state
directory has to be refused outright until it does. The doc comment on
`ensure_trigger_stream` (`bus.rs`) already says horizontal dispatchers
would mean revisiting the stream's shape; it should point here.

### 2. Claimant identity and adoption

**Today.** The claimant is the daemon's `worker_id`, which is its
`runtime_id`: a UUIDv7 minted at the top of `run_daemon`, new on every
start. `claim_delivery` adopts a claim when the row is `claimed` and its
claimant is any id other than its own. It does so at once, on the first
such delivery.

The evidence that the previous holder is gone is the id being different,
and nothing else. `claimed_at` is not compared with anything. The
`coordination_worker` table, where heartbeats land and the sweep marks a
silent worker stale after `DEFAULT_STALE_THRESHOLD_MS` (30 s), is not
read. The comment on the arm gives the argument: "One worker is deployed
today. A different id therefore names a dead previous process".

`take_over_trigger_claim` is a compare-and-set on the claimant and on
`state = 'claimed'`, so of several processes adopting at once, one wins.
It decides between adopters. It does not establish that the holder is
dead.

**With a previous holder that may be alive, this rule is unsafe.** Take
two processes that can both see the row, A holding a trigger it has
claimed and not yet started, behind a cap or in a slow start. The
trigger is redelivered and the broker gives the copy to B. B reads
`Held` with A's id, adopts and starts the invocation. A starts it too
when its hold ends, because nothing tells A it lost the claim. The claim
did not merely fail to prevent the duplicate; it handed B the trigger.

The other writes on a row do not check the claimant either:

- `mark_trigger_started` updates by key alone, so A and B both mark, and
  the row keeps whichever invocation id was written last.
- `release_trigger_claim` deletes by key and `state = 'claimed'`, so an
  `ack`, `nak` or `requeue_held` on A deletes a row B has adopted and
  not yet started, and a third delivery then wins a fresh claim.

**The inbox.** Adoption is attempted only for a claimant the
`coordination_worker` sweep has marked stale, and it is a CAS on the
claimant field (plan §5.2; PR-4). The plan's phrase is "Staleness gates
only *when* adoption is attempted; the CAS keeps timing out of safety".
That holds between adopters. The plan does not say that the holder's own
later writes are conditional on still being the claimant, and they have
to be: a worker marked stale by a partition is still running. Open
question 7 of the edge and bus transport document, leases with fencing
tokens, is the same point.

**Before multi-node.**

- Adopt on evidence that the holder is gone, not on a different id.
- Make every write on a claim row conditional on the claimant, so a
  process that has lost its claim finds out at its next write and stops
  before its first WAL write.
- Decide what a live process does when it finds it has lost a claim.
- Before any of that: `claimed_at` is already on the row. An age check
  on adoption costs one comparison.

### 3. The window between claim and durable start

**Today.** In that window the row is `claimed`, the message is unacked,
and the trigger is in one of two places. It may be in a hold (a paused
model, a full agent, no free worker permit), kept alive every 250 ms by
`hold_tick` or `acquire_run_permit`; a hold behind a build-bound agent
can last for the length of the invocation ahead of it. Or it is inside
`run_invocation` before the first WAL write, where nothing keeps it
alive. A copy redelivered into the window reaches the same process and
is dropped.

**With a second process.** In shape C a copy redelivered into the window
may reach another daemon, which starts it (question 1) or adopts and
starts it (question 2). The window is not short, so this is the common
case, not a race.

In shapes B and D the dispatcher is still one process, but the end of
the window is reported by another. `DurableStart` is a
`tokio::sync::oneshot` inside one process today
(`worker/mod.rs`). Over a network the report can be late or lost. Until
it arrives the row stays `claimed`, the message stays unacked, and
redeliveries are dropped by the same-claimant arm; if it never arrives,
the trigger is dead-lettered as exhausted while its invocation runs (the
third gap above, made routine). The edge and bus transport document
states the rule for that hop: "Never ack upstream until the worker has
durably accepted downstream".

**The inbox.** The window does not exist on the broker's side. The claim
is created, the message is acked, and an `accepted` claim is startable
work that belongs to its claimant, started from the claim's own payload
with a pre-allocated invocation id (plan §5.1, §5.2; PR-3). A process
that restarts scans its own `accepted` claims.

**Before multi-node.** Decide whether the ack stays at durable start
once the durable start is reported by another process. If it stays, the
report has to be durable, and the claim row has to carry the invocation
id and the assigned worker from claim time so that "did it start?" can
be asked instead of inferred. If it moves to claim time, that is PR-3.
The pre-allocated invocation id is common to both, and it is also what
closes the first gap above for one daemon.

### 4. Recovery of a dead worker's started invocations

**Today.** A started invocation is an in-flight row in `worker.db`, a
`durably_started` claim row and an acked trigger. It is recovered by the
next process on the same state directory: `classify_in_flight` scans the
local worker store at startup and resumes what is safe to resume. The
claim plays no part, and the broker will not redeliver.

**With more than one worker.** Nothing recovers a started invocation
whose worker does not come back with its `worker.db`. The coordination
sweep marks the worker stale and publishes `worker.orphaned`
(`CoordinationConsumer::sweep_stale_workers`), and nothing consumes that
event ([#475](https://github.com/bricef/factor-q/issues/475)). The claim
row reads `durably_started` for good, so a redelivery can never restart
the trigger. That is right for safety, and it means recovery has to come
from somewhere other than the trigger stream. The committed design says
as much: an invocation does not migrate, and for a host that is lost
"the operator manually marks affected invocations failed" (data
architecture §5.7).

**The inbox.** PR-4 adopts the claims of a stale claimant. Its crash
table (plan §5.2) gives "crash after start" to WAL resume by the
existing recovery, which is the same worker restarting. So the inbox
does not define recovery of *started* work by a different worker either.
It covers claims that were accepted and never started.

**Before multi-node.** Decide #475: whether a started trigger whose
worker is permanently gone is ever run again, by whom, and what happens
to the side effects of the partial run. If it can be, `durably_started`
stops being a final state and the row's invocation id has to be
replaceable under the fencing rule of question 2.

### 5. Pull discipline under saturation, and detection

**Pull discipline today.** The loop pulls without holding a worker
permit, one message per pull request (`max_messages_per_batch(1)`),
bounded only by the consumer's `max_ack_pending`, which
`run_on_consumer` sets to the larger of twice the worker cap and 1000.
A trigger that cannot start is parked in the process that pulled it and
kept alive. This is the opposite of the plan's invariant I4 ("no pull
request is outstanding unless a permit is held"), and it was chosen
deliberately in #733: with a permit taken before each pull, a held
trigger at a worker cap of 1 either blocked the fleet or wedged (the
module doc of `dispatcher/admission.rs` has the account).

**With more than one consumer.** A daemon pulls and parks triggers it
has no capacity for while another sits idle, because a parked trigger
belongs to the process that pulled it until it starts or that process
drains (`requeue_held`). The state admission decides on is also per
process: the per-agent counts (`AgentConcurrency`), the model pauses
(`ModelThrottle`) and the worker-permit semaphore. An agent declared
`max_concurrent: 1` would run one invocation per daemon.

**The inbox.** PR-5: one bounded pull request per held permit, so work a
daemon cannot start stays on the server for one that can. The plan calls
it hygiene, not safety, and that is right for one daemon. For several it
is how work reaches a worker with capacity.

**Detection today.** `fq status` and `fq doctor` report
`duplicate_dropped`, with the limits in the fourth gap above. The
watcher adapter logs how many open pull requests close an issue and acts
only when there are none (`adapters/github-watcher/outcome.go`,
`openDeliverables`); it does not flag more than one. Nothing reports a
`claimed` row that has been sitting.

**With more than one consumer.** A duplicate across daemons with
separate stores increments nothing anywhere. The only signal is the
outcome: two invocations, two pull requests.

**The inbox.** PR-7: the watcher warns and labels an issue with more
than one pull request, and `fq doctor` checks for duplicate sequences,
old accepted claims and claims held by stale claimants.

**Before multi-node.** Build PR-7's watcher alarm first. It is the one
detector that does not depend on the claim being right, so it is the one
that would notice if this record's assumptions were broken. Then choose
between holding in the process and leaving on the server for the
multi-consumer case, and decide whether an agent's cap is per daemon or
global.

### 6. Retention of claim rows

**Today.** None. See the fifth gap above.

**How long a row must live.** For as long as its message can be
delivered again. `fq-triggers` is not a work-queue stream, whatever the
dispatcher's module doc says: `ensure_trigger_stream` creates it with
`RetentionPolicy::Limits` and a 24-hour `max_age`
(`DEFAULT_TRIGGER_MAX_AGE`), so a message stays in the stream for a day
after it is acked. The `fq-dispatcher` durable will not redeliver an
acked message. But a durable that is deleted and recreated, or whose
state is lost in a broker restore, starts again from the oldest message
in the stream (`trigger_consumer_with_filter` sets no deliver policy, so
the default, all, applies), and the claim rows are then the only thing
that stops a day of triggers from running a second time. That is read
from the consumer's configuration and was not tried. So the floor is the
stream's `max_age`, plus the span of the retry ladder for a message
published just before the cut. Rows from an earlier stream incarnation
can go at any time.

**With more than one worker.** The same floor, in whichever store the
claim moves to. Adoption adds a second reason to look at a row's age:
telling a `claimed` row whose holder is working from one whose holder is
gone.

**The inbox.** No `max_age` on the bucket; terminal claims are collected
after 48 hours, chosen to exceed the stream's 24 (plan §5.2).

**Before multi-node, and before then.** Add retention to the table with
its floor tied to `DEFAULT_TRIGGER_MAX_AGE` in code, the way
`stale_worker_retention_default_dwarfs_the_stale_threshold` ties the
worker window to its threshold. How much longer than the floor to keep a
row is a question about the forensic link (#954), and the two should be
decided together.

### Which plan slice answers which gap

The slices are in §6 of the closed plan. None is scheduled.

| Slice | What it was | The gap it answers |
|---|---|---|
| PR-2, claim registry in NATS KV | A claim every instance can see | Question 1, for shape C |
| PR-3, inbox dispatch | Claim, ack, start from the claim; pre-allocated invocation id; own-claim startup scan; one duplicate-drop path that logs and counts | Question 3; and the first and fourth single-daemon gaps |
| PR-4, multi-worker recovery | Adoption gated on staleness; bounded start attempts; dead-letter disposition | Question 2, with the fencing addition noted there. It does not answer question 4 |
| PR-5, pull discipline | One pull request per held permit | Question 5, distribution of work |
| PR-7, watcher alarm and doctor checks | A warning on more than one pull request per issue; claim health checks | Question 5, detection; and detecting the third single-daemon gap |
| No slice | | Question 4 (#475) and question 6 as it applies to the table that landed |

PR-1's remainder is #954 and PR-6's remainder is #953.

### The condition for reopening this decision

**Before a second process consumes the trigger stream.** That is the
exit condition for this record, in the sense STATUS.md uses the phrase:
anything held carries a named exit condition.

In practice the question comes up when #414 resumes. #414's own first
pass is shape A and does not trip the condition by itself. The condition
is tripped by the first design that puts a second consumer on
`fq-triggers`, a second writer on the claim table, or a claimant id that
can belong to a live process: shape C at once, shapes B and D at the
point where the durable start is reported across a process boundary. At
that point questions 1 to 3 have to be answered before the code is
written, and this record is superseded or refined by the one that
answers them.

## The deferred design: the trigger inbox

Kept because it is where multi-node work starts from. The full text is
§4 to §7 of the
[closed plan](../../plans/closed/2026-07-18-exactly-once-trigger-dispatch.md):
target invariants I1 to I5, the claim record, the lifecycle and the
crash-window table, the invariant-preservation map, the seven slices and
the test plan. In brief, as it was proposed on 2026-07-19:

1. **Identity.** `(stream epoch, stream sequence)`. This part landed.
2. **The inbox.** A claim registry in a NATS KV bucket,
   `fq-trigger-claims`, on the broker that carries the triggers.
   Acquisition is the KV revision-0 `create`. The record is the
   trigger's durable intent: identity, subject, payload, delivery count,
   a pre-allocated invocation id, the claimant, a start-attempt counter
   and a disposition (`accepted → started → completed | failed |
   dead_letter`).
3. **Handover at the ack.** Claim first, then ack. From then on the
   broker is out of the trigger's story, and the inbox owns scheduling,
   retry, recovery and dead-lettering. A lost ack is harmless.
4. **Idempotent redelivery.** A delivery whose identity is claimed is
   acked and dropped, loudly.
5. **Invocations start from the claim.** Recovery is extended one step
   earlier: an accepted claim that never started is startable work, as
   an in-flight WAL row is resumable work. A claim whose claimant is
   stale may be adopted by a CAS on the claimant.
6. **Dead-lettering splits at the handover.** Before the claim, the
   broker's `max_deliver` and the advisory watch apply. After it, the
   inbox bounds start attempts and moves the claim to `dead_letter`.
   Expiry is a disposition, never a timeout.
7. **Pull discipline.** One pull request per held permit.
8. **Deletions.** The `DurableStart` channel, the ack `select!` in
   `handle` and the post-run ack and NAK choreography.

The plan's one-line principle still describes the claim that landed:
"JetStream gives at-least-once *delivery*; nothing built on delivery
timing can give exactly-once *processing*." What landed applies it to
the duplicate, and leaves the pre-start window to the broker.

## Alternatives considered

The three options weighed on #835:

- **(a) Accept the store claim as the design.** Taken.
- **(b) Build PR-3 as specified.** The largest single change in the
  review, for a deployment that does not have a second worker. Not
  taken; kept as the deferred design above.
- **(c) Finish PR-1 and PR-6 only.** Checked against `main` and found
  out of date on two points: the consumer drift repair
  (`EventBus::durable`, `bus/consumers.rs`) already reconciles
  `ack_wait`, and the worker-schema column is not needed for the
  forensic link. What was left became #953 and #954.

From the draft of 2026-07-19, still standing:

- **Tune the ack window and the backoff.** It shrinks the window and
  keeps timing in the safety argument. The 30 s window (#810) and the
  keepalive are kept as pacing; the claim is the argument.
- **Publish-side deduplication.** `Nats-Msg-Id` covers a publish that is
  retried (in place since #825, with a 120 s window). It does nothing
  for a message the broker delivers twice.
- **Fix the client.** The async-nats pull behaviour of July no longer
  matters to the argument, because the loop no longer waits for a permit
  before pulling. A client's internals were never a foundation.
- **Claims as projected events.** A projection is eventually consistent
  and cannot arbitrate a race.

## Consequences

- A running daemon refuses a duplicate delivery of a trigger it has
  claimed or started, whatever caused the duplicate and however late it
  arrives, for as long as the claim row exists. No timing parameter
  appears in that argument. Across a restart the argument rests on
  adoption, and the first gap is where it fails.
- The dispatcher keeps its most delicate code: the `DurableStart`
  channel, the ack `select!` in `handle` and the choreography after it.
  The inbox would have deleted them.
- The duplicate-start guarantee is narrower than "exactly once" by the
  first two gaps in
  [What this design does not close](#what-this-design-does-not-close):
  a crash between the first WAL write and the claim update, and a worker
  that fires and returns in one poll. Neither is tracked by an issue at
  the time of writing.
- The claim table is unbounded until retention is added.
- A second process consuming the trigger stream is not supported, and
  with the adoption rule as it stands it is worse than unsupported: on a
  shared store it turns a redelivery into a duplicate. Nothing in the
  code refuses that deployment.
- The retry ladder (`TRIGGER_RETRY_BACKOFF`, `[30s, 5s, 30s, 2m]` on
  `main`) still describes itself as a stopgap for #327; #953 finishes it.
