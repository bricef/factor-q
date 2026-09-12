# Reasoning models

What factor-q does with a model's reasoning — the working a model produces
before its visible answer — per provider and per route, and how each row
was verified. The design is [ADR-0034](../adrs/accepted/0034-reasoning-as-a-content-part.md);
this guide is the operator's view of what that design delivers today and
where it does not yet reach.

## The one rule

**Within an invocation, a model's reasoning is handed back to it on the
next turn. Across a model edge, it is stripped.** Reasoning is tied to the
model that produced it (every recorded part names that model), so a
multi-agent graph with different models on each side never replays one
model's working to another.

## Support matrix

"Route" is how the model is wired in `fqd.toml`. "Recorded" is the shape
of the reasoning part in the event log and the transcript: `plain` is
readable text, `signed` is readable text plus a continuity token, `opaque`
is a continuity token with no readable text (see
[absence versus opacity](#absence-versus-opacity)).

| Model family | Route | The provider returns | Recorded as | Carried to the next turn | Verified |
|---|---|---|---|---|---|
| Claude, extended thinking | native, `[providers.anthropic]` | `thinking` blocks with a `signature`, their text a summary because the request asks for one (`thinking.display: summarized` on adaptive models — without it every block comes back empty, signature only); `redacted_thinking` | `signed`; `opaque` | yes — the block goes back verbatim, ahead of the turn's tool calls, and Anthropic verifies it | live 2026-09-04 and 2026-09-05 (Opus 5, empty-text blocks: the display flag was not yet set); probe 2026-09-06 (summaries with it); wire goldens |
| Kimi, DeepSeek and other `reasoning_content` models | native OpenAI-compatible endpoint | `reasoning_content` text | `plain` | yes — as `reasoning_content`, which is those APIs' own field | wire goldens |
| The same models through OpenRouter | `[providers.openrouter]`, `api_shape = "openai-compatible"` | `reasoning` text, plus an unsigned `reasoning_details` entry | `plain` | yes — as `reasoning_content`, which OpenRouter documents as the mechanism for raw-string reasoning | live 2026-09-04 and 2026-09-05 (kimi-k3) |
| Gemini, thinking | native, `api_shape = "gemini"` | a `thoughtSignature` on the function-call part; a thought summary, which the request asks for (`includeThoughts`) | `opaque` (a `thought_signature` token); `plain` for the summary | yes — the token goes back as a signature part that genai attaches to the call it came with, and Gemini accepts it | live 2026-09-07 (`gemini-3.8-flash` via AI Studio, two runs: every signature and summary carried, three tool turns in a row accepted); Gemini mock and wire goldens (#600) |
| Claude, Gemini or OpenAI encrypted reasoning through OpenRouter | `[providers.openrouter]` | `reasoning` text plus signed, encrypted or summary `reasoning_details` entries | `signed`, with the whole entry as the token (`format`, `id` and `index` included); `opaque` for an encrypted entry; an unsigned entry stays `plain` | yes — the entries go back verbatim and in sequence as `reasoning_details` (genai ≥ 0.7.0-beta.23, upstream #301); an unsigned entry goes back as `reasoning_content`, the gateway's own mechanism for raw-string reasoning | wire goldens (#603); live 2026-09-08 (`anthropic/claude-sonnet-4-6` through OpenRouter, a harness arm) |
| OpenAI o-series and gpt-5 on chat completions | native | no reasoning text; only `reasoning_tokens` in usage | nothing but the token count | nothing to carry | not verified live |

Two things are true of every row:

- **A different model never sees it.** The cross-model strip is at the
  adapter, pinned by wire goldens for each provider shape.
- **Parallel tool calls keep it.** A turn that calls several tools is
  answered by one tool-results turn, and the reasoning part stays ahead
  of the calls (#511; live-verified on all three arms on 2026-09-05).
- **Order is the provider's.** Parts are recorded in the order they
  arrived, reasoning included, and go back in that order — so a
  signature stays next to the part it belongs to, which is what Gemini
  checks and what Anthropic's leading thinking blocks already satisfy.

## Absence versus opacity

The transcript distinguishes four honest states, because a provider that
withholds reasoning is not the same as a model that produced none:

| The turn had | `fq invocation transcript --reasoning` shows |
|---|---|
| no reasoning | nothing |
| readable reasoning | `reasoning:` and the text |
| a token and no readable text | `reasoning: [opaque — carried, not readable]` |
| a reasoning part with neither | `reasoning: [empty — present, nothing to read]` |

`--json` carries the reasoning unconditionally, including the raw token
under `reasoning.opaque`. The dashboard renders the same states as a
collapsed disclosure, with "opaque — click to see raw" for a token.

## Where it is recorded

- The event log: every `llm.response` payload carries the turn's parts,
  reasoning included; every following `llm.request` carries the replayed
  conversation, so the round trip is auditable from the log alone
  (`fq events get`).
- The WAL and the invocation archive carry the same parts, so reasoning
  survives a crash and resume.
- Cost metadata on each response carries `reasoning_tokens` when the
  provider reports a split, and omits it when the provider does not:
  an unreported split is not a `0`, and the two stay apart all the way
  down. Anthropic reports one since genai 0.7.0-beta.23 (upstream #303)
  whenever thinking engaged; a turn with no thinking reads as unreported,
  since the library maps a zero to none for every usage counter
  (upstream #305). `fq costs` has a `reasoning` column on
  both its tables, by agent and by model — `n/a` where no call reported
  a split, `0` where a provider reported zero — `fq costs --json` and
  `fq invocation show --json` carry `total_reasoning_tokens` as `null`
  against `0` on every row, the per-model rows included, and the
  dashboard's cost pages render the same column with the same `n/a` on
  the by-agent, by-model and by-invocation tables. The per-model split
  is the telling one: a reasoning-first model's bill is mostly
  thinking, and comparing models on one agent (`fq costs --agent <id>`,
  or the agent's drill-down) is where that shows. The figure is a
  decomposition of the output tokens, never an addition to the bill.

## Asking for more or less reasoning

`effort:` in the agent definition sets the per-request reasoning effort
and maps to each provider's own control — see
[agent definitions](agent-definitions.md#iteration-cap-concurrency-cap-and-reasoning-effort).
The Claude 5 family thinks adaptively by default when no effort is set.

## Known gaps

- **Gemini turns with visible text before a signed call, live.** Fixed
  upstream (#302, in genai 0.7.0-beta.23): the signature now rides back
  on the part it arrived on, and the wire golden
  `gemini_text_and_signed_call` pins that. No live run has produced the
  shape yet — Gemini 3 returned a thought summary, not visible text,
  before each call — so it stays hermetically verified.
- **Streaming.** factor-q does not stream, and none of the upstream
  changes touch the streaming paths.

## Verifying a provider yourself

The wire goldens under `fq-runtime/tests/snapshots/reasoning_wire/` are
the hermetic proof: each drives a conversation through the real adapter
against an in-process mock of the provider and pins both what was
recorded and the bytes sent back. The live proof is
[`experiments/reasoning-round-trip/`](../../experiments/reasoning-round-trip/),
whose harness runs three models through a scratch daemon and judges the
event log itself (`verify-carry.py`): every arm completed, every assistant
turn was replayed verbatim into the following request, the reasoning arms
carried a reasoning part and the control carried none. It runs nightly as
the `reasoning-matrix` job of the Live suites workflow, with the evidence
kept as a workflow artifact for two weeks. Adding a provider means adding
both.
