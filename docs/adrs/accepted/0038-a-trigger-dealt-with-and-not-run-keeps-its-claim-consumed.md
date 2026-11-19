# ADR-0038: A trigger dealt with and not run keeps its claim, consumed, and the write comes before the ack

## Status

Accepted (2026-11-19). Refines
[ADR-0032](0032-exactly-once-trigger-dispatch.md): its §2 gains a third
claim state, and its §4, "Every other ack gives the claim back", is
replaced for every answer that deals with a trigger without starting
its run. The rest of ADR-0032 stands. The third state was decided by
the maintainer on 2026-11-18 on
[#1055](https://github.com/bricef/factor-q/issues/1055); the rule for
requeue was settled there in review the same day, and the rule for an
operator's drop on 2026-11-19, replacing an earlier one under which a
drop relabelled the claim as halted in setup.

Implementation: complete, in the change that adds this record
([#1055](https://github.com/bricef/factor-q/issues/1055)); what it leaves
open is tracked in
[#1113](https://github.com/bricef/factor-q/issues/1113).

## Context

Under ADR-0032 §4 a delivery answered without a run released its claim
and was acked. If the ack did not reach the broker, because the
connection dropped or the daemon stopped between the two, the delivery
came back to a claim that was gone, took a fresh one and was handled a
second time: a trigger that failed for good failed again, one requeued
from a hold ran beside its replacement, a run an operator halted in
setup could start, and on a last delivery the dead-letter watch wrote a
second dead letter. One fault, a lost ack, was enough.

An operator's drop of a run that never made its first WAL write writes
the run's ownership row. To every reader of the records, that row is
what a run that started leaves behind.

## Decision

1. **A claim has a third state, `consumed`, with a reason.** A trigger
   dealt with and not run keeps its claim row as `consumed`, with why
   (`consumed_why`), beside `claimed` and `durably_started`. The row is
   kept as long as a started one. A consumed row never goes back to
   `claimed`, and a started one never becomes consumed.
2. **Which answers consume.** A refusal (a subject the dispatcher cannot
   address, an agent id that is not valid, an agent not loaded, a
   payload that does not parse); a requeue from a hold, its replacement
   published; an agent removed during a hold; a run that fails for good,
   is halted by an operator, or returns, before its first WAL write; and
   the dispatcher's own dead letter on a last delivery, once published.
   These do not: a transient failure, which releases the claim and NAKs;
   a dead letter the dispatcher could not publish, which releases the
   claim and hands the trigger to the watch; a drain, an interrupted
   pause and a failed requeue publish, which leave the delivery unacked
   and the claim `claimed`; and the acks after a durable start, which
   keep the claim as it is.
3. **The write comes before the ack.** The consume is awaited, then the
   delivery is acked. A lost ack, or a stop between the two, leaves a
   consumed claim and an unacked message, and the copy the broker brings
   back meets the claim and is acked and dropped. A consume that matches
   nothing, or that the store refuses, is logged, and the delivery is
   acked all the same.
4. **Every reader honours it, and none takes it on trust.** A copy that
   meets a consumed claim is acked, dropped and counted; the watch writes
   no dead letter for it; startup recovery, adoption and `fq doctor`
   leave it. A start on record for the run the claim names is the truth:
   the claim is marked started, with an error line. When the records
   cannot be read the watch writes no dead letter, and requeue cannot
   tell.
5. **Requeue goes by the reason.** A trigger consumed as `dead_lettered`
   is requeued: nothing ran, and that dead letter is its own. One
   consumed as `requeued_from_hold` is refused whatever the operator
   says, since its replacement went out under the same trigger id. Any
   other reason writes no dead letter, so the dead letter and the claim
   disagree, and requeue waits for the operator's word that the dead
   letter's own work did not run.
6. **An operator's drop is recorded beside the reason, never in its
   place.** The claim records, with a time, that an operator's drop
   wrote the run's ownership row. Both writers set it: the consume, when
   the run already has that row, and the drop's handler, on a claim
   already consumed, before it writes the row and again after. Neither
   changes the reason. On a claim that records the drop, only a start in the
   worker's WAL contradicts it, whatever its reason; on any other, an
   ownership row is a start as before. `halted_in_setup` stays the reason
   the dispatcher records for a run an operator halted before its first
   WAL write.

## What this replaces in ADR-0032

- §2: the row's `state` is "`claimed` or `durably_started`"; it now has
  three values, and a consumed row carries its reason and, when there
  was one, the drop.
- §4: "Every other ack gives the claim back", and its list of answers
  that leave no row behind. Only the answers in Decision 2 that NAK give
  the claim back now; the others keep it, consumed. The test it names
  for a requeue from a hold, "a requeue releases the old sequence",
  becomes one that consumes it.

## Consequences

- The claim table keeps a row for every trigger dealt with, not only for
  every one started, for the same retention window.
- `fq invocation show`'s trigger line is what tells a run an operator
  dropped before its first WAL write from one that started: a
  coordination status alone no longer does.
- What stays open is tracked in
  [#1113](https://github.com/bricef/factor-q/issues/1113): a consume the
  store refuses, with a lost ack on top, still lets a copy take the
  claim again, which only holding the ack on a store write would close;
  a stop between a hold's replacement publish and the consume leaves the
  original to be adopted and run beside its replacement; dead letters
  the watch wrote before this state existed stay requeueable; and the
  drop's two records leave one instant, two statements wide: a consume
  that lands between the handler's first record and the ownership row's
  write records nothing, and until the second record a reader takes the
  row for a start and marks the claim started for good. That errs on
  the cautious side, and writing the row and the claim's record in one
  transaction would close it.

## Alternatives considered

- **Ack before releasing.** Alone it changes nothing: a lost ack then
  leaves a `claimed` row with nothing on record, which a returning copy
  takes again.
- **Keep the order, and count and log each second handling.** Cheapest,
  but a trigger requeued from a hold still runs twice and a run an
  operator halted can still start.
- **Let a halt in setup win over every other reason.** Rewriting the
  reason on a drop made the drop's ownership row agree with the claim,
  but it lost what happened: a trigger the dispatcher dead-lettered and
  whose run was then dropped read as halted, and requeue refused its own
  dead letter with a refusal that was not true of it.
