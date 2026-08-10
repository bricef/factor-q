# Trigger wire contract

## Status

Committed (2026-07-07). Documents the **trigger transport as a stable,
language-agnostic public interface**, so an external trigger source — in any
language — can trigger a factor-q agent without depending on `fq-runtime`'s
Rust types. This is the boundary an external *trigger adapter* is built
against; its first consumer is the Go `github-watcher`
([`adapters/github-watcher`](../../../adapters/github-watcher/)). It describes
existing behaviour (`EventBus::publish_trigger` and the trigger dispatcher);
this doc makes that behaviour a contract rather than an implementation
detail.

Related: [event schema](event-schema.md) (the *event* wire format, a separate
contract).

## Why this exists

An external adapter that reused `fq-runtime`'s `EventBus` and payload types
would be coupled to the runtime's *internals* — an "external" component bound
to internal Rust. A wire contract makes the boundary a construction, not a
convention (design principle 3, applied to integrations): a different-language
adapter can only ever use what is written here. This contract is therefore
also the seed of a trigger-source SDK.

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
event's `trigger_id` annotation when a trigger exhausts its deliveries.

### The payload

The message body is the JSON-serialised trigger payload — an **opaque JSON
value**, interpreted by the target agent, not the transport. The dispatcher's
rules:

- **Any valid JSON value** is accepted and handed to the agent as its trigger
  input (`null`, a string, a number, an object, …).
- An **empty body** is treated as JSON `null`.
- A body that is **not valid JSON is dropped** (acked and discarded, with a
  warning) — it never reaches an agent.

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

Delivery is **at-least-once**. The trigger dispatcher acks a trigger on
*dispatch*, not on completion (the reducer WAL owns in-flight durability and
crash recovery), so a slow invocation does not cause redelivery. A producer
should publish each logical trigger once; de-duplication of a re-seen source
event is the *producer's* responsibility (e.g. the github-watcher relabels an
issue out of `ready` before publishing, so it cannot re-trigger).

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
