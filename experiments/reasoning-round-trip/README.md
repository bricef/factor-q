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
```

Keys come from the repo-root `.env` (`ENV_FILE=` overrides) and are read into the
process only. The run writes `events.ndjson` (every payload, live), and per arm the
transcripts, `fq invocation show`, `fq costs` and the daemon config; the databases are
left under `$OUT/run/cache/` for inspection. Spend is well under $0.20.

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
