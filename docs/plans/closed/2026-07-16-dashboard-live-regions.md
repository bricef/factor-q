# Dashboard live regions — datastar poll-and-morph instead of page reloads

**Status:** closed (2026-07-20) — shipped 2026-07-16 with this plan's PR (#245).

## Why

Every dashboard page except the transcript refreshed via a whole-page
`<meta refresh>` every 5s. A reload rebuilds the DOM, so anything the
server HTML does not dictate resets each tick: open `<details>` folds
(the costs one-shot table, the agents load-errors, the agent page's
system prompt) slam shut, scroll position jumps to the top, text
selection vanishes. Reading a long fold on a live page was effectively
impossible — the original complaint.

This amends the v0 decision ("zero client-side JS, `<meta refresh>`
for liveness", operator-dashboard plan layer 3). The transcript page
already crossed that line with the vendored datastar client and a
`<noscript>` fallback; this plan generalises exactly that pattern.

## Design

**Poll-and-morph, same render path.** Pages stay fully server-rendered
by the same pure `render::*` functions. The shell change
(`render::live_page`) wraps the body in a `#main` region that datastar
re-fetches on the `--refresh` cadence:

- The poll target is the page's own URL — path *and* query, read from
  `location` in the datastar expression — so `/costs?window=7d` polls
  itself and no handler threads its URI into the shell.
- Content negotiation keys on the `Datastar-Request` header the
  vendored client stamps on every `@get`: one URL, two
  representations. Implemented as a single axum middleware
  (`datastar_negotiation`) that reduces the handler's full HTML page
  to a one-event SSE patch of `#main` (mode `inner`, so the region's
  own polling attribute is never morphed away). Handlers are entirely
  unchanged.
- Folds emit through one `fold(id, …)` helper:
  `<details id=… data-preserve-attr="open">`. The stable id pairs old
  and new nodes across the morph; `data-preserve-attr` keeps the
  reader's open/closed choice.
- Every live body ends with an `updated HH:MM:SS UTC` line, morphed
  each tick — a frozen time is the honest signal polls stopped. The
  unreachable and skew banners render inside `#main`, so an outage
  still goes loud within one tick.
- No-JS browsers (and curl) keep working: the shell's `<noscript>`
  meta refresh preserves the old full-page behaviour.

**Out of scope:** the transcript page (already SSE-streamed; its
`@get` responses are `text/event-stream` and pass through the
middleware untouched), and the screenshot/fixture pipeline (renders
the same live shell; `file://` pages never poll).

## Verified

Unit + router tests cover the negotiation (SSE shape, pass-throughs,
outage morph) and the shell/fold contracts. End-to-end with a real
chromium against a live daemon: a fold opened by the operator stayed
open across multiple ticks while the freshness line advanced, and
scroll position held.
