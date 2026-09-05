# `llm.failure` — a consumer contract

**Status:** accepted (2026-08-05). Every open question below was decided by
the maintainer on [#450](https://github.com/bricef/factor-q/pull/450); the
answers are folded into the sections they affect, and §8 records them
together. Implementation follows this document.
Resolves [#447](https://github.com/bricef/factor-q/issues/447) by closing the
hole in code rather than weakening the doc; scheduled by
[Phase 4 cohort 4.2](../../plans/closed/2026-07-28-phase-4-call-point-inventory.md).
Adopting this means an ADR and a move to `committed/`, plus the amendment to
[`event-schema.md`](../committed/event-schema.md) drafted in §5.

Filed here rather than in a `proposed/` folder because this repo has two
design states, not three (see [the folder README](../README.md)): aspirational
is where a proposal lives until it is accepted.

## The hole

`llm.request` is published unconditionally, before the provider is called
(`worker/reducer/runner.rs:2692`). Every other outcome of that call publishes
something. A provider error does not: the error arm closes the WAL row
(`write_llm_dispatched`, then `write_llm_completed(..., is_error = true, 0.0,
...)`) and returns at `runner.rs:2729` without touching the bus. There is a
second, quieter instance of the same shape at `runner.rs:2763` — a response
with no content and no tool calls is *synthesised* into an
`LlmError::RequestFailed` and takes the same silent exit.

So the event log records an intention with no outcome, and a consumer folding
the pair cannot distinguish "the call failed" from "the event was lost" —
which are the two cases an operator most needs told apart. The fix is a second
terminal event type, `llm.failure`, a sibling of `llm.response` rather than a
nullable-fields variant of it. The reason is not aesthetic: `LlmResponsePayload`
requires `stop_reason` and `usage`, and every consumer that reads it today
reads them unconditionally. Making them `Option` would push a "did this
actually happen?" branch into six consumers that currently have no such branch,
and would make the *type* stop telling you which case you are in. A separate
variant moves that decision to the match site, where the compiler can see it.

## 1. Terminality — what one `llm.request` guarantees

**Settled by code.** Retries exist, but all of them sit on the far side of the
event boundary, so none of them are event-visible:

- `RetryingLlmClient` (`llm.rs:105–143`) is a decorator on `LlmClient::chat`.
  It burns up to `max_attempts` (default 4) provider attempts with jittered
  exponential backoff *inside* the single `llm.chat(...)` call at
  `runner.rs:2704`. `dispatch_llm` cannot see the attempts and does not count
  them.
- `run_structured_completion` (`runner.rs:2900+`) does retry — but on *parse
  and validation* failure, and each retry issues a fresh `dispatch_llm` with a
  fresh `call_id`. A model failure inside it is not retried at all
  (`Err(_) => return Ok(None)`, `runner.rs:2935`).
- The trigger/redelivery path replays a *trigger*, producing a new invocation
  chain; it never re-enters an existing `call_id`.

Therefore the invariant is exact when keyed on `call_id`, and only when keyed
on `call_id`:

> A published `llm.request` is followed by exactly one of `llm.response` or
> `llm.failure`, bearing the same `call_id`.

It is **not** exact per invocation. An invocation may contain several
`llm.failure` events, because an LLM failure is only invocation-terminal for
an agent turn: `run_model_with_llm` emits `failed` and aborts
(`runner.rs:2579`), whereas a sampling or elicitation failure merely declines
the server's request and the agent continues (`runner.rs:3109`, and ADR-0018's
"the failure is the server's, not the agent's"). Consumers must not assume at
most one failure per invocation, nor that a failure ends the invocation.

*Recommendation, needs a decision:* the payload should not attempt to describe
the hidden retry attempts. The count exists only inside the decorator, and
surfacing it means changing `LlmClient::chat`'s signature — a bigger change
than this one, and properly part of [#278](https://github.com/bricef/factor-q/issues/278),
which already wants the retry path rebuilt around `Retry-After`. Noted as
deliberately excluded in §3 so it is not mistaken for an oversight.

## 2. Cost on a failed call

This is the question with a real defect behind it, not a hypothetical.

**A failed call can carry usage today, and we throw it away.** The
empty-response path at `runner.rs:2739–2763` holds a fully-parsed
`ChatResponse` — `response.usage` is populated with the provider's real token
counts — and deliberately discards it, writing `0.0` to the WAL. Its own
comment admits the exposure: *"Deliberately skips `totals.total_llm_calls` and
drops the usage: the turn produced nothing to bill against an outcome. If a
provider ever bills tokens for empty completions this undercounts — revisit if
that matters."* A provider that returns 200 with an empty completion has done
the prefill and will bill the input tokens. Against this project's rule that
cost information is never lost, that is a standing leak, and `llm.failure` is
the event that plugs it.

**The provider-error path structurally cannot carry usage.** `LlmError`
(`llm.rs:49–68`) has no usage field, the adapter is non-streaming
(`exec_chat`, `llm/genai.rs:224`), and a transport failure yields no parsed
body. Where the provider billed anyway — a request that completed server-side
whose response we lost — the spend is real but unobservable to us. We must not
paper over that with `0.0`.

That distinction is the whole design point, so the payload must be able to say
all three things:

| Case | `usage` | envelope `cost` |
|---|---|---|
| Empty completion (200, no content) | `Some(real counts)` | present, priced |
| Provider error, transport/4xx/5xx | `None` | absent |
| Refused pre-dispatch (`UnpricedModel`) | `None` | absent, and no WAL row |

`Option<TokenUsage>` rather than `TokenUsage::default()`: `0` means "the
provider billed nothing", `None` means "we do not know what the provider
billed". Writing the second as the first is exactly the class of loss the cost
principle forbids. The same reasoning says the envelope's `CostMetadata` must
be **absent**, not zeroed, when usage is unknown — the projection's retention
sweep exempts rows on `total_cost IS NOT NULL`
(`control_plane/projection/store.rs:170`), so a zero-cost failure row would be
retained forever as a fake cost record, while an absent-cost failure row is
swept with the rest of the trail, which is right.

*Consequence, needs a decision:* a cost-bearing `llm.failure` must be added to
the cost queries' `event_type IN ('llm_response', 'invocation_summary')` lists
(`projection/store.rs:398, 453, 499, 534, 570`), or the recovered spend will be
projected and then never reported. Recommendation: add it. `totals.total_cost`
should likewise accumulate it, which means a failed call can now trip the
budget check — correct, but a behaviour change worth naming.

## 3. `LlmFailurePayload`

```rust
pub struct LlmFailurePayload {
    pub round: u64,                    // #[serde(default)], as LlmResponsePayload
    pub call_id: Uuid,                 // correlates with llm.request
    pub model: String,                 // not derivable from the envelope when cost is absent
    pub error_kind: LlmErrorKind,      // the taxonomy below
    pub error_message: String,         // provider text, already the operator's only handle on a 429
    pub duration_ms: u64,              // wall time including hidden retries — the 429 tell
    pub usage: Option<TokenUsage>,     // §2; None means unknown, never zeroed
    pub origin: LlmCallOrigin,         // #[serde(default)]; agent turn vs sampling vs elicitation
}
```

**`LlmErrorKind` is a projection of `LlmError`, not a new taxonomy.** The
existing enum (`llm.rs:49`) already has the right joints — `Auth`,
`RateLimited`, `InvalidResponse`, `RequestFailed`, `UnpricedModel` — and
`is_transient` already partitions them the way an operator cares about. It
cannot be serialised as-is (`thiserror`, non-`Serialize`, carries payload
strings that belong in `error_message`), so the payload carries a `Copy`,
`snake_case`-serialised unit-variant mirror, converted by a single `From<&LlmError>`.
Add one variant with no `LlmError` counterpart: `EmptyResponse`, for the
synthesised failure at `runner.rs:2744`, which is a genuinely different
condition from a transport error and is currently indistinguishable from one.

Do **not** reuse `FailurePhase` (`events.rs:1060`). It answers "where in the
lifecycle", not "what went wrong", and the 2026-07-14 code review already flags
`FailurePhase::LlmResponse` as a catch-all for three unrelated conditions. Do
not reuse `ToolErrorKind` either — a different domain.

Mirroring `LlmError` also gives [#278](https://github.com/bricef/factor-q/issues/278)
somewhere to land: when `map_error` learns to produce `RateLimited` with a
`Retry-After`, `error_kind: rate_limited` becomes queryable in the projection
without another schema change. *(Landed with
[#546](https://github.com/bricef/factor-q/issues/546): a 429 now arrives as
`rate_limited`, and the same change added `rejected` and `timeout`.)*

**Deliberately excluded**, with reasons:

- *Retry-attempt count* — invisible above `LlmClient::chat`; see §1.
- *`stop_reason`* — a failed call has none, and inventing one would be a lie.
- *The request messages* — already on `llm.request` with the same `call_id`.
- *`is_error: bool`* — the variant *is* the error. A boolean would be dead
  weight that only ever holds one value.
- *`retry_after`* — belongs with #278's `RateLimited` rework, not ahead of it.

**Subject:** `fq.agent.{agent_id}.llm.failure`, added beside
`agent_llm_response` in `events/subjects.rs:49`. **Schema id:**
`factor-q/llm_failure@1`, hence projection `event_type = "llm_failure"`, hence
wire tag `"event_type": "llm_failure"`. Those three names must be minted
together — the same triple the cleanroom review calls out as easy to desync.

## 4. Per-consumer obligations

The full production consumer set for `EventPayload::LlmResponse`, and what each
owes `llm.failure`:

| Consumer | File:line | Obligation |
|---|---|---|
| `EventPayload::subject` | `events.rs:288` | **Must** add an arm — exhaustive, will not compile. New subject helper. |
| `EventPayload::schema_id` | `events.rs:322` | **Must** add an arm — exhaustive. `event_type()` derives from it. |
| Projection `extract_fields` | `control_plane/projection/store.rs:761` | **Must** add an arm — the tail arm is an explicit variant *list*, not `_`. Map onto existing columns: `error_kind`, `error_message`, `duration_ms`, token columns from `usage`, `total_cost` from the envelope when present. **No migration needed** — every column already exists. |
| Cost queries | `projection/store.rs:398, 453, 499, 534, 570` | **Should** include `llm_failure` in the `event_type IN (...)` filters (§2). Owner's call. |
| `fq events tail` renderer | `fq-cli/src/events.rs` (`print_event`) | **Must** add an arm — exhaustive. Render kind, model and message; render cost inline only when the envelope carries it. |
| `fq events query --event-type` help | `fq-cli/src/cli.rs` (`EventCommands::Query`) | **Should** list `llm_failure`. Query itself is string-keyed and needs no change — the event appears automatically once projected. |
| Turn fold | `turn.rs:123`, catch-all `_ => None` at `:210` | **Must** fold, and will *silently* not, since it compiles. Emit `TurnAction::Assistant { is_error: Some(true), content: Some(error_message), tool_calls: [], model, cost_usd }`. This needs **no new `TurnAction` variant**: `is_error` already exists and `transcript.rs:360` already renders `" [error]"` from it — which is precisely the `[error]` entry the WAL-backed transcript produced and the Turn-backed one lost. Also delete the comment at `turn.rs:165–172` asserting the old invariant. |
| Summary consumer | `control_plane/summary_consumer.rs:167`, catch-all at `:250` | **Recommend ignore.** A failed call has no assistant output to summarise, and summarising errors would spend model budget on noise. If it is to react, `FILTER_SUBJECTS` (`:64`) needs the new subject. |
| Trace oracle | `test_support/oracle.rs:192, 218` | **Must** extend the grammar, or an `llm.failure` in any trace becomes a spurious `TraceViolation`. This is where the §5 invariant is actually enforced — and its absence is why the original hole survived. |
| `event_kind_of` | `test_support/events.rs:42` | **Must** add an arm — exhaustive. |
| Dashboard | `services/fq-dashboard/src/render.rs:876, 803` | **Nothing required.** It reads `EventView` rows over tarpc (`main.rs:503`) keyed on the `event_type` *string*, so the row appears with no code change. `render.rs:803`'s failure banner keys on `"failed"` and will not pick it up — correct, a failed call is not a failed invocation. Styling is optional polish. |
| `github-watcher` adapter | `adapters/github-watcher/outcome.go` | **Nothing required** — routes on subject and ignores LLM subjects. Worth a negative case in its test table. |
| `transcript.rs:474` `assistant_entry` | `transcript.rs:474` | **Nothing** — dead surface, no callers; retired by cohort 4.4. |

**Invocation status does not change.** A failed *call* is not a failed
*invocation*: the agent-turn case already publishes `failed` separately
(`runner.rs:2579`), and the sampling case is deliberately non-fatal.
`llm.failure` must not touch invocation status, or a declined sampling request
would start killing invocations that currently survive it.

## 5. Amended Invariant 3 — ready to paste

Replace item 3 of `event-schema.md` §Invariants (line 511) with:

```markdown
3. **Every `llm.request` is followed by `llm.dispatched`, then exactly one
   terminal outcome — `llm.response` on success, `llm.failure` on a provider
   or validation error** — all three bearing the same `call_id`.
   Provider-level retries happen below this boundary (`RetryingLlmClient`)
   and are not event-visible, so one request yields one outcome. The
   invariant is keyed on `call_id`, not on invocation: an invocation may
   contain several `llm.failure` events, because a failed *sampling* or
   *elicitation* call declines the server's request without ending the
   agent's invocation, while a failed *agent turn* additionally emits
   `failed`. The legacy executor path skips `llm.dispatched` (no WAL).
```

Invariant 5 needs a matching qualification, since a failure can now bill:

```markdown
5. **`envelope.cost` is present on `llm.response` events that bill, and on
   `llm.failure` events where the provider's usage was recoverable** (an
   empty completion). It is *absent* — never zeroed — when usage is
   unknown, so `total_cost IS NULL` continues to mean "no known spend"
   rather than "zero spend". There is no separate cost event.
```

## 6. Compatibility — the important one

**Adding a variant is safe forwards and unsafe backwards, and the codebase has
no fallback.** `EventPayload` is adjacently tagged with no `#[serde(other)]`
escape hatch (`events.rs:449`):

```rust
#[serde(tag = "event_type", content = "payload", rename_all = "snake_case")]
```

An unknown tag is a hard deserialisation error, not an ignored field. So:

- **New code, old events: safe.** Every historical tag stays known. Old
  `llm.request` events with no terminal partner remain in the log; the invariant
  in §5 is honest only from the deploy forward, and consumers folding history
  must tolerate a dangling request. Say so in the changelog.
- **Old code, new events: degrades, and not uniformly.** Three distinct
  behaviours, all verified:
  - `control_plane/durable_consumer.rs:334` — the shared harness behind *both*
    the projection consumer and the summary consumer logs a warning and
    **acks** ("acking to avoid a redelivery loop"). No poison-pill stall, but
    the event is **dropped, not deferred**: an old control plane never projects
    it, so it never appears in `fq events query` even after an upgrade, because
    the ack is permanent.
  - `event_tail.rs:55` — `?` propagates, so `fq events tail` **terminates the
    stream with an error** on the first `llm.failure`. An old CLI against a new
    daemon breaks visibly.
  - `advisory_watch.rs:256`, `operator.rs:221` — `let Ok(..) else`, silent skip.

The exposure window is bounded by the event stream's 30-day `max_age`
(`bus.rs:56`), so a rollback within 30 days of the first `llm.failure` hits
this. The mixed-version window is otherwise short — worker and control plane
ship in one binary — but a rollback is exactly when one is least keen on
surprises.

*Recommendation, needs a decision:* land a separate, tiny PR **before** this
one adding an `Unknown` unit variant with `#[serde(other)]` to `EventPayload`.
Serde permits `other` on a unit variant of an adjacently tagged enum, so it is
a two-line change plus arms. It does nothing for the rollback described above —
already-deployed binaries lack it — but it converts *every future* event type
from a backwards-breaking change into a transparent one, and this is the last
cheap moment to do it, since the alternative is paying this analysis again for
each new type. The cost is that exhaustive matches gain an `Unknown` arm, which
is a fair price for the forward compatibility.

## 7. Blast radius and sequencing

**The file-size ratchet must be paid, and cannot be waived quietly.**
`events.rs` is pinned at 1203 production lines in `.file-size-baseline`; the
file is 1204 lines total, so essentially all of it counts. `LlmFailurePayload`,
`LlmErrorKind`, the `From<&LlmError>` conversion and two match arms add roughly
50–70 lines. Baselines may only go down, so this PR must extract at least as
much as it adds, by hand-editing the baseline in the diff where a human sees it.
The natural extraction is the LLM-call payload cluster — `LlmRequestPayload`,
`LlmDispatchedPayload`, `LlmResponsePayload`, `Message`, `MessageRole`,
`MessageToolCall`, `ToolSchema`, `RequestParams`, `Effort`, `StopReason`,
`TokenUsage`, `LlmCallOrigin` (`events.rs:683–831`, ~150 lines) — into
`events/llm.rs`, re-exported from `events`. `events/` is already a real module
directory (`subjects.rs`, `tests.rs`), so this is a genuine module move, not a
splice, and it pays for the new type twice over. Doing the extraction as its
own no-behaviour-change PR would make the substantive diff readable.

**Ordering.** The deadline is cohort **4.4**, which retires the WAL-backed
dashboard path and the read service — after which the WAL-backed transcript's
`[error]` rendering is gone for good and the failure becomes unobservable
anywhere. Cohort **4.2** builds `event.list` / `event.stream`, which is where an
operator would go looking, and the plan already carries this item into it.
Ideal order:

1. `#[serde(other)]` fallback (§6) — smallest, unblocks everything after it.
2. `events/llm.rs` extraction — pays the ratchet, no behaviour change.
3. `llm.failure` itself: type, subject, emit site, and every §4 obligation in
   one PR, since the exhaustive matches make it atomic anyway.
4. Coverage — the oracle grammar extension plus a golden for a failing call.
   Its absence is the whole reason #447 exists; the loss passed every acceptance
   criterion we wrote.
5. Then cohort 4.2's `event.list` flip, so the new atom is visible the day the
   surface ships.

## 8. Decisions, settled

Determined from code, not open: terminality per `call_id` (§1); that the
empty-completion path holds and discards real usage (§2); that unknown tags
hard-fail and durable consumers ack-and-drop them (§6); the consumer list and
which sites are compile-forced (§4); the ratchet arithmetic (§7).

Decided on [#450](https://github.com/bricef/factor-q/pull/450), 2026-08-05:

1. **Cost: yes to both.** `llm_failure` joins the cost-query filters, and
   recovered failure spend accumulates into `totals.total_cost` where it can
   trip a budget. The maintainer's reasoning goes further than the question
   asked: *"anything that incurs costs and isn't accounted for should be
   considered an error."* That promotes §2's finding from context to a defect
   this work must close — the empty-completion path holds a fully-parsed
   `response.usage` and writes `0.0`, and the implementation captures and
   prices it rather than merely carrying it in a new payload.
2. **The `#[serde(other)]` fallback is folded in as a dedicated commit**,
   not a separate PR.
3. **A failed call consumes a round number** — in its own commit, because
   round numbers shift for any invocation containing a failed-but-survived
   sampling call, which is a reviewed golden change rather than a mechanical
   one.
4. **The summary consumer stays deaf to failures** for summarisation, with
   the explicit qualification that nothing about that deafness may
   compromise cost accounting — decision 1 governs.
5. **`EmptyResponse` is a distinct `error_kind`.**
6. **`llm.failure`** is the subject leaf.

The commit boundaries these imply: the serde fallback; the `events/llm.rs`
extraction that pays the ratchet; `llm.failure` itself with every §4
obligation; the round-number change; and the §5 invariant amendments.
