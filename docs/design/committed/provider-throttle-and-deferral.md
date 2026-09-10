# Provider throttle and rate-limit deferral

## Status

Committed (2026-09-07). Describes the throttling, deferral and per-model
circuit-breaker halves of
[#278](https://github.com/bricef/factor-q/issues/278), built in the same
change as this note. The classification half — a 429 becomes
`LlmError::RateLimited { model, retry_after }`, honoured in place up to
`[worker.llm_retry] max_retry_after_ms` — landed in
[#606](https://github.com/bricef/factor-q/pull/606) and is assumed here.

The maintainer's decisions on the issue (2026-09-04) are taken as given and
not reopened: a 429-exhausted invocation is a **deferral**, re-queued with
backoff, not a failure and not counted against the trigger source's
retries; the grain is per `(provider, model)`, keyed by the model string a
request targets.

Related: [event schema](event-schema.md) (`invocation.deferred`),
[operating the daemon](../../guide/operating-the-daemon.md) (what an
operator sees).

## The problem in one paragraph

`max_concurrent_invocations` bounds invocations, not calls to a provider,
so a fleet of N invocations on one model fires N calls at the same endpoint
in lockstep. When the provider throttles, every call sees a 429, every
invocation burns its retry budget on the same wall, and the watcher marks
real issues `status:failed` for what was infrastructure backpressure. The
runtime needs one place that knows, per model, "the provider is saying
no", and every path that would send that model work needs to ask it.

## Entities

Everything below is a value the worker holds; the trait surface is the
existing `LlmClient` and `Worker`.

- **Model key** — the `model` string on a `ChatRequest`. Routing already
  maps a model to exactly one provider, so the key names the
  `(provider, model)` pair the issue asks for without a second field.
- **`ModelThrottle`** — one per daemon, shared by the LLM client stack, the
  runner and the dispatcher. It holds a `ModelState` per model key.
- **Pause** — `paused_until`, set by a 429: the provider's `Retry-After`
  when it sent one, else an escalating default (`default_pause_ms`,
  doubling per consecutive 429 *wave*, capped at `max_retry_after_ms`).
  A pause is only ever extended, never shortened; it ends by expiring.
- **Wave** — the 429s that arrive while a model is already paused are one
  wave: they are answers to calls that were in flight when the first one
  landed, and they carry no new information. A 429 that arrives with no
  pause in force opens a new wave. A success clears the wave count.
- **Permit** — the in-flight cap is a count of permits per model,
  adjusted AIMD-style: halved on a wave (floor 1), raised by one after
  `success_window` consecutive successes without a 429 (ceiling
  `max_concurrent_invocations`). A call holds a permit for its duration
  and settles it with a verdict — succeeded, rate-limited, or failed for
  another reason — which is what moves the cap.
- **Hold** — a trigger the dispatcher has pulled for a paused model and is
  keeping alive, un-acked and un-started, until the pause ends. The
  trigger durable's ack window is one second
  (`TRIGGER_RETRY_BACKOFF[0]`), so a held delivery is kept open with
  JetStream in-progress acks rather than left to be redelivered.
- **Deferral** — an invocation that was already running when the retry
  layer gave up on a 429 (the wait exceeded the cap, or the attempts ran
  out). It suspends at its step boundary with the WAL row left in flight
  and `phase = "deferred"`, stamps the call's own `llm_dispatch` row
  `deferred_at`, emits `invocation.deferred`, and is resumed by the
  dispatcher after the delay. No `failed` event, no terminal, no
  archive, no retry consumed anywhere.

## Invariants

1. **A paused model starts no invocation.** The dispatcher checks the
   agent's model before publishing `triggered`; a held trigger emits no
   event, provisions no workspace and is `attempt: 1` when it finally
   starts.
2. **In flight never exceeds the cap.** Every call through the stack
   acquires a permit first; the cap can only fall while permits are held,
   and a fallen cap admits nothing until enough permits settle.
3. **`1 ≤ cap ≤ max_concurrent_invocations`**, always. Halving floors at
   one; increase ceilings at the configured concurrency.
4. **A deferral is never a failure and never consumes a retry.** No
   `failed` event, so the watcher's retry count is untouched; no NAK, so
   JetStream's delivery count is untouched (the trigger was acked at the
   first WAL write, before any model call could be made).
5. **A deferred invocation resumes from where it stopped.** Its WAL row
   stays in flight, so the same recovery path that follows a drain or a
   crash continues it — after the dispatcher's timer, or at the next
   daemon start if the process stops first.
6. **With `enabled = false` the throttle is inert**: every permit is
   granted at once, no pause is set, no trigger is held. Deferral is not
   part of the throttle — it is what a 429-exhausted invocation *is*, per
   the decision — and stays on.

## How the three layers compose

The client stack is `RetryingLlmClient<ThrottledLlmClient<GenAiClient>>`.
The throttle sits *inside* the retry layer so it sees every raw provider
outcome, and the retry layer's per-attempt sleeps overlap the model's
pause rather than adding to it.

```text
call ──▶ permit (waits: pause over AND in_flight < cap) ──▶ provider
                                                              │
   200 ◀── settle(Succeeded): successes += 1, maybe cap += 1 ◀─┤
   429 ◀── settle(RateLimited): new wave? cap /= 2; pause ◀────┤
   other ◀─ settle(Failed): release only ◀─────────────────────┘
```

1. **Pause (the breaker).** A 429 pauses the model. Every waiting
   permit — from every invocation on that model — sleeps until the pause
   ends, so one `Retry-After` is honoured once, fleet-wide, instead of
   once per call.
2. **Adaptive cap.** The permit count follows the provider's signal:
   down fast on a wave, up slowly on sustained success. A fleet at 8 that
   is told no once runs at 4 until it has earned 5 back.
3. **Admission.** The dispatcher asks the throttle before starting an
   invocation. For a paused model it holds the trigger — in-progress acks
   every 400 ms, checking the pause and the drain signal each tick — and
   starts it when the pause lifts. A drain or shutdown during a hold
   returns without acking, exactly as a drain that lands after a pull
   does today.

**Why hold rather than NAK.** A `Nak(delay)` returns the permit at once,
but each redelivery counts toward `TRIGGER_MAX_DELIVER` (5): four pauses
of a persistently throttled model would dead-letter a trigger as
`trigger_exhausted` — a `failed` event the watcher counts — which is the
outcome this issue exists to prevent. It also stamps `attempt: N` into
the transcript preamble, the tell the redelivery-storm notes rely on
(`docs/reviews/2026-09-03-production-readiness-review.md`, B1). A held
trigger consumes nothing and starts as `attempt: 1`. The cost is
head-of-line blocking: a held trigger occupies a dispatcher permit for the
pause, so at `max_concurrent_invocations = 1` a trigger for an unpaused
model queued behind it waits too. The pause is bounded by
`max_retry_after_ms` when it is ours and by the provider's own number
when it is theirs, and a paused model would have parked that permit on
its first call anyway.

**Mid-flight deferral.** When `RateLimited` reaches the runner — the
retry layer gave up — the agent turn is not failed. The call's WAL row is
closed as an error like any other (`llm.dispatched`, `llm.failure` with
`error_kind: rate_limited`, so the trail invariant holds), the state row
is marked `phase = "deferred"`, the call's `llm_dispatch` row is stamped
`deferred_at`, `invocation.deferred` is published, and
`InvocationOutcome::Deferred { resume_after }` returns to the dispatcher,
which schedules `Worker::resume_invocation` after the delay under its own
concurrency permit. On resume, an errored LLM row stamped `deferred_at`
is the recorded 429 and is skipped rather than reproduced as a failure;
the step re-issues the model call with a fresh `call_id`. The stamp is
per row because the state row's `phase` is one column the next step
boundary overwrites: a deferral, a resume, a real failure whose terminal
was lost, and a second resume must reproduce the real failure, not the
deferral. The delay is the
larger of what the provider asked for and the model's escalating default,
so an invocation that keeps meeting a two-second `Retry-After` backs off
anyway. A resume that meets another 429 is deferred again; a daemon that
stops first resumes the invocation at startup like any in-flight row.

## Configuration

`[worker.throttle]` in `fqd.toml`, read at start (hot reload is
[#114](https://github.com/bricef/factor-q/issues/114)):

| key | default | meaning |
|---|---|---|
| `enabled` | `true` | Off makes the throttle inert (invariant 6). |
| `default_pause_ms` | `30000` | The pause a 429 without `Retry-After` sets; doubles per wave. |
| `success_window` | `10` | Consecutive successes that earn one more permit. |

The pause cap and the permit ceiling are not new keys: they are
`[worker.llm_retry] max_retry_after_ms` and
`[worker] max_concurrent_invocations`, the numbers they must agree with.

## Observability

`control.status` and `control.doctor` carry `throttled_models`: every
model that is paused, running under its ceiling, or has seen a 429 in the
current window — with `paused_until_ms`, `cap`, `ceiling`, `in_flight`
and `rate_limited_in_window`. `fq status`, `fq doctor` and the dashboard's
health page render the list; it does not change the doctor verdict,
because a throttled model is the runtime doing its job, not something an
operator has to fix. The event trail shows the cause on `llm.failure`
(`rate_limited`, from #606) and the decision on `invocation.deferred`.

## Out of scope

- A fleet-wide cost cap
  ([#42](https://github.com/bricef/factor-q/issues/42)) — a different
  lever on a different axis.
- Hot-reloading the throttle keys
  ([#114](https://github.com/bricef/factor-q/issues/114)); they are read
  once at start.
- Per-agent model fallback chains (the candidate remedy on #278): a
  paused model could fall through to a priced alternative, but that is a
  routing decision, and it needs this groundwork first.
- Persisting throttle state across restarts. A restart forgets every
  pause; the first 429 after it re-learns the pause in one round trip.
- A `deferred` status on `fq invocation list`: a deferred invocation
  reads as in flight there, which it is. The WAL phase says `deferred`.
