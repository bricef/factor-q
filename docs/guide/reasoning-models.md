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
| Claude, extended thinking | native, `[providers.anthropic]` | `thinking` blocks with a `signature`; `redacted_thinking` | `signed`; `opaque` | yes — the block goes back verbatim, ahead of the turn's tool calls, and Anthropic verifies it | live 2026-09-04 and 2026-09-05 (Opus 5); wire goldens |
| Kimi, DeepSeek and other `reasoning_content` models | native OpenAI-compatible endpoint | `reasoning_content` text | `plain` | yes — as `reasoning_content`, which is those APIs' own field | wire goldens |
| The same models through OpenRouter | `[providers.openrouter]`, `api_shape = "openai-compatible"` | `reasoning` text, plus an unsigned `reasoning_details` entry | `plain` | yes — as `reasoning_content`, which OpenRouter documents as the mechanism for raw-string reasoning | live 2026-09-04 and 2026-09-05 (kimi-k3) |
| Gemini, thinking | native, `api_shape = "gemini"` | a `thoughtSignature` on the function-call part; a thought summary when requested | `opaque` (a `thought_signature` token); `plain` for the summary | yes — the token goes back as a signature part that genai attaches to the call it came with | **hermetic only**: Gemini mock and wire goldens (#600); no Gemini key is held, so no live run |
| Claude, Gemini or OpenAI encrypted reasoning through OpenRouter | `[providers.openrouter]` | `reasoning` text plus a signed or encrypted `reasoning_details` entry | `plain` — the signed or encrypted entry is **dropped** | **no**. The text goes back as `reasoning_content`, which OpenRouter cannot turn back into a signed block. The provider accepts the turn and continues without its prior reasoning; nothing errors | live probe 2026-09-05; open as [#603](https://github.com/bricef/factor-q/issues/603) |
| OpenAI o-series and gpt-5 on chat completions | native | no reasoning text; only `reasoning_tokens` in usage | nothing but the token count | nothing to carry | not verified live |

Two things are true of every row:

- **A different model never sees it.** The cross-model strip is at the
  adapter, pinned by wire goldens for each provider shape.
- **Parallel tool calls keep it.** A turn that calls several tools is
  answered by one tool-results turn, and the reasoning part stays ahead
  of the calls (#511; live-verified on all three arms on 2026-09-05).

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
  provider reports a split. It reaches no operator surface yet
  ([#536](https://github.com/bricef/factor-q/issues/536)), and a provider
  that reports no split (Anthropic) records `0`, which is not the same as
  none.

## Asking for more or less reasoning

`effort:` in the agent definition sets the per-request reasoning effort
and maps to each provider's own control — see
[agent definitions](agent-definitions.md#iteration-cap-and-reasoning-effort).
The Claude 5 family thinks adaptively by default when no effort is set.

## Known gaps

- **#603** — signed and encrypted reasoning through OpenRouter, above. The
  fix needs genai's OpenAI adapter to carry `reasoning_details`; the data
  model here is ready for it.
- **Gemini is not live-verified**, and genai's Gemini adapter approximates
  signature placement when a turn has both text and a call (it attaches
  the signature to the next part it meets). The wire golden
  `gemini_text_and_signed_call` pins that approximation so a change in it
  is visible.
- **#536** — the reasoning-token split is recorded and not shown.

## Verifying a provider yourself

The wire goldens under `fq-runtime/tests/snapshots/reasoning_wire/` are
the hermetic proof: each drives a conversation through the real adapter
against an in-process mock of the provider and pins both what was
recorded and the bytes sent back. The live proof is
[`experiments/reasoning-round-trip/`](../../experiments/reasoning-round-trip/),
whose harness runs three models through a scratch daemon and checks, from
the event log, that every reasoning part reappears in the following
request. Adding a provider means adding both.
