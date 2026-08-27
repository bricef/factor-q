# Trigger wire contract

## Status

Committed (2026-07-07). Documents the trigger transport as a **language-agnostic
internal SPI for co-located, first-party adapters**, so an adapter written
outside the Rust workspace can trigger a factor-q agent without depending on
`fq-runtime`'s Rust types. Its first consumer is the Go `github-watcher`
([`adapters/github-watcher`](../../../adapters/github-watcher/)). It describes
existing behaviour (`EventBus::publish_trigger` and the trigger dispatcher);
this doc makes that behaviour a contract rather than an implementation
detail.

It is **not a public interface for arbitrary external callers**, and has not
been one since [ADR-0006](../../adrs/accepted/0006-registry-first-api.md)'s D8
was accepted on 2026-07-20: the bus is internal infrastructure, and remote
ingress goes through `trigger.publish` on the authenticated edge. This
paragraph claimed otherwise until 2026-08-13 (#457). Only the framing was
wrong — every mechanical detail below was accurate throughout, and a reader
who built against the contract rather than the preamble built the right
thing.

Related: [event schema](event-schema.md) (the *event* wire format, a separate
contract).

**Direction (2026-08-13): the edge, not the broker.** Publishing straight to
`fq.trigger.*` is **deprecated** in favour of `trigger.publish` on the
authenticated edge. ADR-0006 (D8, and Appendix C) settles that NATS is
factor-q's internal event bus and coordination substrate rather than a public
interface, and a trigger is the last write that still arrives on it from
outside. Nothing described below has been removed and nothing stops working:
the Go adapters publish this way today and continue to. What changes is where
new work goes — an adapter written from here on should target the edge, and
guarantees the runtime makes about a trigger (see [Payload
size](#payload-size)) are made at `trigger.publish`, which is the only place
the runtime can make them.

## Why this exists

An adapter that reused `fq-runtime`'s `EventBus` and payload types would be
coupled to the runtime's *internals* — a separately built, separately deployed
component bound to internal Rust. A wire contract makes the boundary a
construction, not a convention (design principle 3, applied to integrations):
a different-language adapter can only ever use what is written here.

Note what that argument is and is not. It is about **language** coupling, and
it holds for a first-party adapter exactly as it did when this was framed as a
public interface. It is not an argument about trust: writing here does not
make a publisher external, and the contract carries no authentication, which
is why the direction above sends new adapters to the edge instead.

## The contract

To trigger an agent, a producer publishes a message to a subject on a
JetStream stream:

| Field | Value |
|---|---|
| **Transport** | NATS **JetStream** (not core NATS pub/sub) |
| **Stream** | `fq-triggers` — subjects `fq.trigger.>`, file storage, `Limits` retention, max age **24h** |
| **Subject** | `fq.trigger.<agent_id>` — one subject per agent |
| **Body** | a single **JSON value** — the trigger *payload* (see below) |
| **Headers** | `Fq-Trigger-Id` — optional, the trigger's identity (see below) |
| **Publish** | JetStream publish, **await the ack** — the ack confirms the trigger is durably persisted |

`<agent_id>` must be a valid agent id (the same id the agent's definition
declares). Producers should validate it locally before publishing; a trigger
for an unknown agent is durably stored but never dispatched.

### Trigger identity (`Fq-Trigger-Id`)

Every trigger the runtime acts on has an identity: a **UUIDv7**, in
canonical hyphenated text. It travels as a NATS **header**, never in the
body — the body is contractually the payload and nothing else, so there
is nowhere in it a runtime-owned field could go without breaking every
producer that writes the payload directly.

The identity exists from the moment the system takes responsibility for
the trigger:

| Case | Who names it |
|---|---|
| `trigger.publish` (the daemon's own command) | The daemon mints the id as it publishes, and returns it to the caller. |
| An inbound trigger carrying `Fq-Trigger-Id` | **Honoured verbatim** — never re-minted. |
| An inbound trigger with no header | The dispatcher assigns one when it first handles the message. |

For a producer this header is **optional**, and omitting it costs
nothing: the trigger is dispatched exactly as before. Setting it buys two
things — the producer can name the trigger it published in its own
records, and the identity is *stable across redeliveries* (an assigned id
is chosen per handling, so a redelivered header-less trigger is named
afresh each time). A producer that sets it should use a UUIDv7 and must
not reuse one across logically distinct triggers. A value that is not a
readable UUID is treated as absent rather than rejected — a typo must not
silence an agent.

Downstream, the identity appears on the invocation's `triggered` event as
`trigger_id` (see [event schema](event-schema.md)), which is what links
an invocation to the trigger that caused it, and on the dead-letter
event's `trigger_id` annotation when a trigger exhausts its deliveries —
**usually, not always.** Two cases produce a dead letter that names no
trigger: one recorded before triggers were named, and one the advisory path
reconstructs after the original has already aged off the trigger stream,
which reads the identity rather than inventing one. Such a dead letter still
lists and still carries its payload, but it cannot be requeued idempotently —
there would be nothing to refuse a second attempt on — so `dead_letter.requeue`
refuses it and re-running it means `trigger.publish`, as new work with a new
name.

It is also the identity the operator surface knows a trigger by: `trigger.get`
takes it, `trigger.publish` returns it in its receipt, and the record it
resolves — source, subject and payload — is retained indefinitely.

A trigger the runtime has not yet acted on has no record yet, and answers
`Unlocatable` rather than "not found" — saying "no such trigger" about a name
a receipt has just issued would be a lie. But it does **not** read distinctly
from an id that names nothing: a primary-key miss cannot tell "queued, not yet
dispatched" from "recorded before triggers were kept" from "never real", so
all three get the one answer, and the message names all three causes rather
than the daemon guessing between them. A producer that needs to know whether
its trigger ran has to watch for the invocation, not poll `trigger.get`.

**A requeue is a new trigger, and says which one it re-ran.**
`dead_letter.requeue` publishes a **fresh** id rather than the original's, and
the requeued trigger's record carries `requeued_from` naming the trigger it
came from. Nothing about this crosses the wire — lineage is a fact of the
record, and the header carries the identity alone — but it is what makes the
verb idempotent: one requeue per original, and a second attempt is refused
with the name of the trigger the first one made. Re-publishing under the
original's id would have recorded nothing, since the trigger's row is keyed on
the identity, so there would have been nothing to refuse on.

### The payload

The message body is the JSON-serialised trigger payload — an **opaque JSON
value**, interpreted by the target agent, not the transport. The dispatcher's
rules:

- **Any valid JSON value** is accepted and handed to the agent as its trigger
  input (`null`, a string, a number, an object, …).
- An **empty body** is treated as JSON `null`.
- A body that is **not valid JSON is dropped** (acked and discarded, with a
  warning) — it never reaches an agent.

### Payload size

**A trigger accepted through `trigger.publish` is at most 512 KiB (524,288
bytes) of JSON body.** A larger one is *refused* — never truncated, because a
shortened payload is a different task and the agent would run it as though it
were the original. The refusal names the limit and the actual size, so the next
attempt is an edit rather than a guess.

The limit exists because a trigger is now **retained indefinitely**: its
payload is kept in the runtime's projection long after both this stream (24h)
and the event log (30 days) have aged past it, and unbounded retention of an
unbounded field is the one combination that needs a ceiling. 512 KiB is roughly
sixteen hundred times a real task payload, sixteen times under the 8 MiB frame
one `trigger.get` answer has to fit inside, and half a stock `nats-server`'s
default `max_payload`, so an accepted trigger is never one the transport then
refuses.

**This is a property of `trigger.publish`, not a rule this stream enforces.**
A producer publishing straight to `fq.trigger.*` — the deprecated path — is
bounded only by the broker's own `max_payload`, and gets a publish-ack failure
rather than a named refusal if it exceeds it. That asymmetry is one more reason
to move to the edge; it is not a gap the runtime intends to close on the
broker side.

### Task-oriented payload convention

Task-oriented trigger producers should use a JSON object with these fields:

| Field | Type | Meaning |
|---|---|---|
| `task` | string | The scoped work to perform. |
| `refs` | array of strings | Relevant URLs or repository paths. |
| `constraints` | array of strings | Boundaries the work must respect. |
| `done_criteria` | array of strings | Observable conditions for completion. |

Producers may add source-specific fields (for example, `github: { repo, issue }`)
without changing the shared fields. Consumers must tolerate unknown extras. The
[github-watcher](../../../adapters/github-watcher/) emits this shape.

A JSON string remains valid transport payload, including `fq trigger <agent>
"<task>"`, which parses JSON when possible and otherwise wraps the argument as
a JSON string. It is suitable for manual, ad-hoc triggering; adapters should use
the object convention so task semantics do not drift between sources.

This convention is temporary: typed trigger signatures in the graph-executor
track supersede it. Until then it supplies one interoperable semantic shape above
the opaque transport contract.

### Delivery semantics

Delivery is **at-least-once**, and where the ack lands is the one thing a
producer has to know, because it defines the only window in which the same
trigger runs twice.

The dispatcher acks on **durable start** — the moment the invocation's first
WAL write lands, signalled back through the `Worker` seam — not at dispatch
and not at completion. Both other choices were tried and are wrong in
opposite directions. Acking at completion re-ran anything longer than the
30s ack-wait, one trigger producing N invocations; that is the redelivery
storm the M0 dogfood loop found on 2026-07-06. Acking at dispatch would drop
a trigger whose process died before writing anything, with no record that it
had ever arrived.

So the redelivery window is *dispatch → first WAL write*, seconds wide, and a
crash inside it is exactly the case where redelivery is what you want: nothing
durable happened, so the redelivered trigger is the run, not a duplicate of
it. From the first WAL write on, in-flight durability is the reducer WAL's
job and a crash resumes rather than re-runs.

A producer should still publish each logical trigger once; de-duplication of a
re-seen source event is the *producer's* responsibility (e.g. the
github-watcher relabels an issue out of `ready` before publishing, so it
cannot re-trigger).

## Minimal producer (any language)

1. Connect to the daemon's NATS URL.
2. Open a JetStream context.
3. Publish to `fq.trigger.<agent_id>` with a JSON-value body, and await the
   publish ack.

```
subject = "fq.trigger.m0-issue-fix"
body    = "\"Implement the fix described in GitHub issue #6 (bricef/factor-q). Today is 2026-07-07.\""
js.Publish(subject, body)   // await ack
```

Optionally, name the trigger so you can refer to it later:

```
headers = { "Fq-Trigger-Id": uuidv7() }
js.PublishMsg(subject, body, headers)   // await ack
```

That is the whole contract. A producer needs nothing from `fq-runtime` — only
a NATS client and this document.

## Stability

This is a committed interface. The subject scheme, the stream name, the
JetStream transport, and the opaque-JSON-payload rule are stable; changes are
versioned and announced. The task-oriented convention is recommended semantics
for adapters, pending typed trigger signatures; individual agents still own their
payload meaning.

`Fq-Trigger-Id` was added on 2026-08-10. It is an **additive** change: it
is optional on the producing side and the body is untouched, so every
producer written against the earlier contract keeps working unchanged and
un-recompiled. Header names are part of this contract from here on — a
future header follows the same rule, and the payload stays opaque.

The transport described here is **deprecated but supported** as of 2026-08-13
(see [Status](#status)). Deprecated means no new guarantees are added to it —
the payload limit above is the first one made at `trigger.publish` and not
here, and later ones will follow it — and that new adapters should target the
edge. It does not mean scheduled removal: a date, and a migration for the
existing adapters, are separate decisions not taken here.
