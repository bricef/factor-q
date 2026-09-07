# Reasoning round-trip probe

Does omitting reasoning blocks on a tool-result turn cost us anything on Anthropic models?

- **Issue:** <https://github.com/bricef/factor-q/issues/437>
- **Harness:** [`harness/run.sh`](harness/run.sh) — wrapper that sources the key
- **Probe:** [`harness/probe.py`](harness/probe.py) — stdlib only, no dependencies
- **Live matrix:** [`harness/live-matrix.sh`](harness/live-matrix.sh) — three models
  through factor-q itself, not a hand-rolled request

**Status (2026-09-04).** The gap described under *Why* is closed: #510 (ADR-0034)
carries thinking blocks with their signatures and replays them. The probe and its
2026-07-28 result stand as the measurement of the *old* behaviour; the
[live run](#live-run-through-factor-q-2026-09-04) below is the measurement of the new.

## Why

Anthropic's contract says thinking blocks are **required** to be passed back within a
tool-use turn, and that when conversation history is incompatible with thinking the API
*"silently disables thinking for that request"* rather than erroring. factor-q never
captures thinking blocks — genai drops the `signature` end to end — so every
continuation turn we send is in exactly that shape.

That raised a specific, alarming hypothesis: **thinking may be silently switched off on
every continuation turn after the first tool call**, invisibly, on the fleet's primary
model. This experiment tests it.

## Method

Identical turn 1, then two arms on the continuation turn:

| Arm | Assistant turn sent back | Represents |
|---|---|---|
| `echo` | verbatim, thinking blocks intact | correct protocol |
| `strip` | `text` + `tool_use` only | factor-q today |

Measured on the continuation response: HTTP status, count of `thinking` /
`redacted_thinking` blocks, and `usage.output_tokens_details.thinking_tokens`.

## Running

```sh
harness/run.sh                                   # factor-q's exact shape (no effort)
harness/run.sh --effort high --repeat 3          # what the recorded result used
harness/run.sh --models claude-opus-4-8 --out r.json
```

The key is sourced from `~/fq-dogfood/.secrets/env` (override with `--secrets`), read
into the process only — never printed, never on a command line, never in the results file.

## Result (2026-07-28, `--effort high --repeat 3`)

| Model | Valid runs | `echo` thinking tokens | `strip` thinking tokens | Silent disable |
|---|---|---|---|---|
| `claude-fable-5` | 3/3 | mean 92.0 | mean 103.7 | **0/3** |
| `claude-opus-5` | 1/3 | mean 158.0 | mean 119.0 | **0/1** |

**The silent-disable hypothesis is refuted.** In every valid run the `strip` arm still
produced a thinking block and spent thinking tokens, and no arm returned a 400. Omitting
thinking blocks does not switch thinking off on the continuation turn, and a *cleanly
absent* history is accepted where a *tampered* one is rejected.

Thinking-token deltas ran in opposite directions across the two models (Fable strip
slightly higher, Opus strip lower) — noise at this sample size, not a signal.

## What this does and does not establish

**Establishes:** no silent disabling, no rejection, no measurable difference in thinking
*effort* between the arms.

**Does not establish:** whether the continuation reasoning is as *good* without the prior
turn's reasoning available. Token counts measure spend, not quality. Answering that needs
a task-outcome benchmark with a gradeable rubric, not a token counter.

## Live run through factor-q (2026-09-04)

The question this time is not what Anthropic does with a stripped history but whether
factor-q, after #510, carries reasoning forward at all — through the reducer, the WAL,
the event log and back onto the wire. So the run goes through the real runtime: a
scratch `fqd` on a private JetStream broker, three agents, one task each.

| Agent | Model | Route | `effort` | Expected reasoning shape |
|---|---|---|---|---|
| `kimi-k3-reasoner` | `moonshotai/kimi-k3` | OpenRouter (openai-compatible) | `medium` | `plain` — text in `reasoning` |
| `opus-5-thinker` | `claude-opus-5` | Anthropic | `high` | `signed` — thinking block + signature |
| `gpt4o-mini-control` | `openai/gpt-4o-mini` | OpenRouter | unset | none — the control |

The task forces two tool calls before the answer (read a file, then `wc -w` it), so a
reasoning model has to have its turn-1 reasoning replayed on turns 2 and 3. All three
completed with the right answer (first line verbatim, 33 words) in 17 s, 13 s and 4 s.

### Per turn, from the event log

Each response was paired with the next `llm.request` by event order, and the reasoning
part it produced was looked up in that request by signature (Anthropic) or text (Kimi).

| Arm | Turn | Response parts | Reasoning | `reasoning_tokens` | Carried into next request |
|---|---|---|---|---|---|
| kimi | 1 | reasoning, tool_call | plain, 10 chars | 6 | **yes, byte-identical** |
| kimi | 2 | tool_call | none returned | 3 | n/a |
| kimi | 3 | reasoning, tool_call | plain, 159 chars | 45 | **yes, byte-identical** |
| kimi | 4 | text | none returned | 3 | n/a |
| kimi | 5 | text, tool_call | none returned | 3 | final turn |
| opus | 1 | reasoning, tool_call | signed, text `""`, signature 704 chars | 0 (not reported) | **yes, byte-identical** |
| opus | 2 | reasoning, tool_call | signed, text `""`, signature 584 chars | 0 (not reported) | **yes, byte-identical** |
| opus | 3 | text, tool_call | none returned | 0 | final turn |
| control | 1–3 | tool_call | none, no `reasoning` key at all | 0 | nothing to carry |

In every replayed assistant turn the reasoning part precedes the tool call, which is
the order Anthropic requires. No `llm.failure` was recorded in any arm: OpenRouter
accepted `reasoning_content` back for Kimi, and Anthropic accepted its own signed
blocks back for Opus — the `echo` arm, produced by factor-q rather than the probe.

**Re-run the same evening on genai `0.7.0-beta.21` (PR #592), after the fork was
retired:** identical outcome. Kimi's plain reasoning and both of Opus's signed blocks
(again empty text, 524- and 496-char signatures) were carried byte-identically into
the following request, the control carried none, no `llm.failure`, $0.057 in total.
The wire goldens recorded on the fork build had already said so; this is the provider
agreeing.

### Where it was checked

- **Transcript.** `fq invocation transcript --reasoning` shows Kimi's text, Opus's
  `[+ an opaque provider token]`, and nothing for the control. `--json` carries
  `reasoning.text` / `reasoning.opaque` (the whole `{type, thinking, signature}` block)
  and omits the key entirely on turns with none — absence, not opacity (I7).
- **Event log.** `llm.response` payloads carry the parts; `llm.request` payloads carry
  them back in `messages`; envelope `cost.reasoning_tokens` is populated.
- **Stores.** `worker.db › llm_dispatch` (the WAL) holds the parts in 8 of 11 rows;
  `control-plane.db › invocation_archive` holds them in the Kimi and Opus final state
  blobs and not the control's; `projection.db › events` is index-only by design.
- **Cost.** $0.0439 (Opus), $0.0216 (Kimi), $0.0005 (control); $0.066 in total.
  `total_cost` is unaffected by the split.

### Observations worth keeping

1. **Opus 5 returned thinking blocks whose `thinking` text is empty** but signed
   (704 and 584 chars). That is Anthropic's data, not a parsing loss — the raw block's
   `thinking` field is `""` too. The transcript renders it as an empty `reasoning:` line
   followed by the opaque note; opaque-only would read better (#537).
2. **Anthropic reports no reasoning-token split**, so `reasoning_tokens` is 0 on every
   Opus turn — indistinguishable from "reported zero" (#536).
3. **Kimi reported 3 reasoning tokens on turns that returned no reasoning.** Recorded
   faithfully; the provider's count, not ours.
4. **`reasoning_tokens` reaches no operator surface.** It is on every response envelope
   and in the WAL, but the projection has no column for it, so `fq costs` and the
   dashboard cannot show it (#536).
5. **Two drive-bys** the run surfaced: the sandbox denial text names the target twice
   and the allowed prefix never, which cost Kimi a round (#534); and every `fq` verb
   prints tarpc INFO spans on stderr (#535).

### Running it

```sh
just build-runtime && just install-nats        # binaries + pinned nats-server
mise exec -- bash harness/live-matrix.sh       # needs raw TCP to localhost
OUT=/somewhere bash harness/live-matrix.sh     # default OUT is $TMPDIR/fq-live-matrix
TASK='Read {work}/notes.txt and {work}/checklist.txt in one turn, then …' \
  mise exec -- bash harness/live-matrix.sh     # another task on the same matrix
```

Keys come from the repo-root `.env` (`ENV_FILE=` overrides; no file at all is fine
when the keys are already in the environment, which is how CI passes them) and are
read into the process only. The run writes `events.ndjson` (every payload, live), and
per arm the transcripts, `fq invocation show`, `fq costs` and the daemon config; the
databases are left under `$OUT/run/cache/` for inspection. Spend is well under $0.20.

The run judges itself. `harness/verify-carry.py` reads `events.ndjson` and, per arm,
requires a `completed` invocation with no `llm.failure`, every assistant turn replayed
verbatim (canonical JSON — reasoning, text and tool calls, in recorded order) in the
request that followed it, at least one reasoning part carried for the two reasoning
arms, and none at all for the control. Readable characters are reported, not asserted.
Its exit status is the harness's, so the nightly `reasoning-matrix` job in
`.github/workflows/live-suites.yml` goes red on a lost token and on a run that proved
nothing. A reasoning part is only ever carried from a turn that another request
follows, so the default task opens with a mental step before the first tool call
(see [the nightly's first run](#the-nightlys-first-run-and-the-task-that-reasons-2026-09-07));
a red that still says "no reasoning part was carried" means the model chose not to
think that night, so rerun once before treating it as a bug. An arm whose invocation
failed on a provider 5xx or 429 through the runtime's retry budget is triggered once more
after a minute, and the verdict judges the arm's latest invocation, naming the superseded
one.
`VERIFY=0` skips the verdict for a `TASK=` probe whose arms are being read some other
way. With `AISTUDIO_API_KEY` set the [Gemini arm](#gemini-live-2026-09-07-hermetic-only-from-2026-09-05)
runs as well.

`TASK=` replaces the default sequential task; a `{work}` token in it expands to the
fixture directory, which holds `notes.txt` and `checklist.txt`. The default task
carries the "one tool call at a time" steer itself (it used to sit in the agent
prompt), so a task that wants parallel calls is free to ask for them — which is how
the [parallel tool-call probe](#parallel-tool-calls-through-factor-q-2026-09-05)
for issue #511 ran on the same three arms.

## Parallel tool calls through factor-q (2026-09-05)

The live check for #511: a turn that issues two tool calls must be answered by **one**
`tool_results` message carrying both results, in call order, and the provider must
accept it. Same daemon, broker and three arms as above, with `TASK=` asking for both
fixture files to be read in the same turn. Two runs, the second with the adapter's
debug logging on (`RUST_LOG=info,fq_runtime::llm=debug`) to rule out a dropped thinking
block; $0.082 in total.

| Arm | Turn 1 response | Next `llm.request` | Accepted | Cost (run 1 / run 2) |
|---|---|---|---|---|
| `kimi-k3-reasoner` | reasoning + 2 `tool_call` | one `tool_results`, 2 results, call order; reasoning replayed ahead of the calls | yes — no `llm.failure` | $0.011 / $0.014 |
| `opus-5-thinker` | 2 `tool_call`, no thinking block returned | one `tool_results`, 2 results, call order | yes — no `llm.failure` | $0.028 / $0.028 |
| `gpt4o-mini-control` | 2 `tool_call` | one `tool_results`, 2 results, call order | yes — no `llm.failure` | $0.0005 / $0.0004 |

All three parallelised on the first try, answered with both first lines verbatim, and
ended with `report_outcome: success`. Before the fix the same conversation would have
gone to Anthropic as two consecutive user messages of one `tool_result` each.

What the run could **not** show: Opus 5 returned no thinking block on either parallel
turn, so a signed block replayed ahead of two `tool_use` blocks was not observed live.
The adapter dropped nothing — its debug-level "dropping a thought signature" line never
fired — the model simply did not think on this task. That shape is pinned hermetically
by the `anthropic_parallel_tool_calls_signed` wire golden.

## Gemini: live (2026-09-07; hermetic only from 2026-09-05)

Gemini's continuity token is a `thoughtSignature` on the function-call part, with no
readable text beside it. Since #600 factor-q records it as an opaque reasoning part and
replays it on the call it came with; the wire goldens `gemini_*` under
`fq-runtime/tests/snapshots/reasoning_wire/` pin both directions against an in-process
Gemini mock. Until 2026-09-07 that was the only proof, this repository holding no key.

**The fourth arm.** `gemini-3-thinker` runs `gemini-3.8-flash` natively
(`[providers.gemini]`, `api_shape = "gemini"`, a Google AI Studio key as
`AISTUDIO_API_KEY`). The arm switches on when the key is set and is announced as off
when it is not, so a three-arm run can never pass for a four-arm one; the nightly job
passes the secret through and warns while it is absent. No effort is set: Gemini 3
thinks by default and signs every function call, and the readable summary rides on the
adapter's capture flag (`includeThoughts`). The free tier is enough — the 3.1 Pro
previews are quota-blocked on it, and the 2.5 generation is retired for new users — but
it answers `503 UNAVAILABLE` ("high demand") for minutes at a time, and it allows 5
requests a minute and 20 a day per model (measured 2026-09-07; the daily count resets at
07:00 UTC, after the nightly), which is why the harness re-triggers an arm once when the
provider had no capacity — a 5xx or a 429 — through the runtime's whole retry budget,
and the verdict judges an arm's latest invocation. Probing by hand on the arm's model
spends the nightly's day; probe on a sibling model instead.

**Two runs, same task as the other arms** (`~/factor-q-live-runs/2026-09-07-gemini-arm/`
and `-2/`):

| run | turns | reasoning produced / carried | kinds | readable chars | outcome |
|---|---|---|---|---|---|
| 1 | 2 | 4 / 4 | opaque 2, plain 2 | 1884 | failed on turn 3: Gemini 503 through four attempts (82 s) |
| 2 | 3 | 6 / 4 | opaque 3, plain 3 | 2970 | completed, verdict green, $0.0103 |

What each turn looks like, from the event log: Gemini returns a thought part and a
`functionCall` carrying a signature (912 and 528 characters on run 1), recorded as
`[opaque, plain, tool_call]` with `reasoning_tokens` from `thoughtsTokenCount` (234,
119); the next request replays the assistant turn verbatim, genai puts the signature
back on the `functionCall` part, and Gemini accepts it and continues — three times in a
row on run 2. "Carried" is produced minus the final turn's parts. Run 1's failure was
the provider's, not the round trip's: the two turns before it carried everything.

## Readable thinking, and arrival order (2026-09-06)

**Why every Opus 5 thinking block was empty.** The 2026-09-04 run recorded Opus 5's
thinking blocks with an empty `thinking` and a 700-character signature, and this file
called that "their data". It was ours: Anthropic's adaptive-thinking request takes
`thinking.display: "summarized"`, and without it the block comes back signature-only.
genai writes that field from its `capture_reasoning_content` option, which factor-q never
set. A probe replaying the recorded request shape with a question that forces thinking,
with and without the field (`probe/` beside the run below):

| request | thinking text | signature | thinking tokens |
|---|---|---|---|
| as recorded, no `display` | 0 chars | 936 chars | 284 |
| `display: "summarized"` | 175 chars | 692 chars | 182 |

The same option is Gemini's `includeThoughts`, without which a thought summary is never
returned, and is inert on the OpenAI-shaped wire (non-streaming). factor-q sets it on
every call since this date. The wire goldens moved by exactly those two keys and nothing
else, with every recorded turn byte-identical.

**Live run on that build, same matrix, same task** (`~/factor-q-live-runs/2026-09-06-readable-thinking/`):

| arm | status | turns | reasoning parts produced / carried | kinds | readable chars | cost |
|---|---|---|---|---|---|---|
| kimi-k3-reasoner | completed | 3 | 3 / 2 | plain | 247 | $0.0159 |
| opus-5-thinker | completed | 3 | 1 / 1 | signed | 162 | $0.0391 |
| gpt4o-mini-control | completed | 4 | 0 / 0 | — | 0 | $0.0007 |

Opus 5's one block carries 162 readable characters and a 512-character signature in the
same `thinking` block, recorded as `signed`, replayed verbatim on the next request and
accepted; the transcript's `--reasoning` view shows the text where it showed
`[opaque — carried, not readable]` before. "Carried" is one less than "produced" because
the final turn is never replayed. Adaptive thinking engaged on the first turn only, which
is the model's choice; a run where it never engaged fails the Opus arm's verdict by
design (see [Running it](#running-it)).

**Arrival order.** The adapter used to record a turn's reasoning parts ahead of its text
and tool calls whatever order the provider sent them. Harmless for Anthropic, whose
thinking blocks lead anyway; wrong for Gemini the moment genai stops hoisting signatures
itself (the adjacency fix proposed upstream), because a signature that arrived between
the text and the call would be replayed ahead of the text and attached to it. Parts are
now recorded in arrival order; a sibling-field summary goes after whatever reasoning
already leads the turn and before the first spoken part; none of the fourteen goldens'
recorded turns moved.

## The nightly's first run, and the task that reasons (2026-09-07)

The first CI run of the `reasoning-matrix` job (workflow run 34128058055, once the
Anthropic key was in place) went red with both reasoning arms at zero: Kimi produced
no reasoning part on any of four turns, Opus 5 none on any of three, and the verdict
said what it is built to say — the run proved nothing. Everything else worked: keys,
the private broker, the scratch daemon, three completed invocations at the usual cost.

**It was the task, not the pipeline.** Replaying the recorded request shapes:

- Opus 5's adaptive thinking never engaged on the first turn of the old task — 0 of 9
  across `effort: high`, `xhigh` and `max`, and 0 of 9 more on three harder variants
  of the same mechanical task. The 2026-09-06 run's one thinking block was a lucky
  first turn. Forcing thinking is not available: `thinking.type: enabled` is rejected
  by both Opus 5 and Sonnet 5 (`use adaptive and output_config.effort`).
- Kimi K3 through OpenRouter returns reasoning for such turns as a few words or an
  empty string (`"Read file."`, `""`), on Moonshot AI, DeepInfra and Sail Research
  alike; an empty string records as no part. Routing was a red herring: pinned to
  either provider, the genai-shaped request behaves the same.
- A reasoning part is only ever *carried* from a turn that another request follows,
  so reasoning on the final answer never counts. The turns that count are the
  tool-calling ones, and those were the mechanical ones.

**The change.** The default task now opens with a mental step — the smallest prime
above 40 and its digit sum, no tool — before the two tool steps, and the answer gains
a line. On that first turn Opus 5 thought 4 times in 4 (128–146 readable characters,
52–56 thinking tokens) and Kimi returned 25–271 characters of reasoning 7 times in 7,
pinned to DeepInfra, pinned to Moonshot AI, and unpinned. A full local run on the new
task (`~/factor-q-live-runs/2026-09-07-task-that-reasons/`) passes the verdict: Kimi
carried two plain parts (272 readable characters), Opus 5 one signed block (213), the
control none. Nothing in the pipeline changed; the 2026-09-06 verdict on the old task
stands as recorded.

## Traps this harness already hit

Each of these produced a plausible-looking but meaningless result before being fixed.
They are guarded now; keep the guards.

1. **Null experiment.** At default effort, turn 1 went straight to the tool call without
   thinking, so there were no blocks to strip and both arms sent byte-identical requests
   (identical `input_tokens` was the tell). `verdict()` now returns `INVALID` when turn 1
   emits no thinking block. **A run with no turn-1 thinking is a failed experiment, not a
   null result.**
2. **Parallel tool calls.** Opus 5 emitted two `tool_use` blocks; returning only one
   `tool_result` 400s *both* arms and reads like a protocol finding. The probe now returns
   one `tool_result` per `tool_use`.
3. **Safety refusal.** An earlier scenario framed as reactor coolant temperature was
   refused outright by Fable 5 (`stop_reason: "refusal"`, empty content). Keep the
   scenario free of any safety surface; the probe now reports refusals distinctly.
4. **Non-determinism.** Whether turn 1 thinks varies run to run even at `--effort high`
   (Opus 5 was valid in only 1 of 3). Never conclude from a single run.
