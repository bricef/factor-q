# Reasoning round-trip probe

Does omitting reasoning blocks on a tool-result turn cost us anything on Anthropic models?

- **Issue:** <https://github.com/bricef/factor-q/issues/437>
- **Harness:** [`harness/run.sh`](harness/run.sh) — wrapper that sources the key
- **Probe:** [`harness/probe.py`](harness/probe.py) — stdlib only, no dependencies

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
