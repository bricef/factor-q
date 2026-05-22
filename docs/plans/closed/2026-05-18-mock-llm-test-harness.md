# Plan: Mock LLM Test Harness

**Date**: 2026-05-18
**Status**: Closed 2026-05-18. All four steps shipped:
`a8fdfb6` (base_url plumbing), `22e1bc3`
(MockAnthropicServer), `2cbab1d` (full-pipeline acceptance
test against the mock), `3759b1a` (manual drift detector +
`just acceptance-drift` recipe). The parent plan's step-8
status block notes the previously-deferred live acceptance
test now runs in every CI build via the mock.
**Design references**:
- [`docs/design/event-schema.md`](../../design/event-schema.md) — event types under test.
- [`docs/design/data-architecture.md`](../../design/data-architecture.md) §9.3 — the canonical end-to-end flow.

## Goal

Add a self-contained mock of the Anthropic API that runs in
every CI build, plus a manually-invocable real-Anthropic
drift detector. Close the test gap between
`FixtureClient`-level logic tests (in-process, no HTTP) and
the deferred live-Anthropic acceptance test (needs a key,
costs money, non-deterministic).

After this lands:

- The step-8 acceptance test (`completed_invocation_archives_and_worker_cleans_up`)
  runs every build, deterministic and free.
- A separate `just acceptance-drift` recipe exercises the
  real Anthropic API on demand to catch protocol changes.

## Context

Today's testing pyramid:

- **`FixtureClient`** — most reducer/runner tests. Bypasses
  HTTP entirely; injects `ChatResponse` values directly.
- **Real Anthropic** — gated on `ANTHROPIC_API_KEY`. Hits the
  real API; only runs when the operator has a key.

The gap is the wire layer: nothing exercises the
`GenAiClient` → HTTP → response-parsing → `ChatResponse`
path in CI. That's where Anthropic protocol drift and
genai-adapter bugs would hide.

## Decisions taken on 2026-05-18

- **`base_url` is operator-facing.** Added to
  `AnthropicConfig` and documented in `fq.toml`. Use cases:
  testing (this plan), internal LLM proxies, future
  Bedrock-compatible endpoints. Default is `None`, in which
  case `genai` uses the public Anthropic URL.
- **Mock style is sequenced-response.** Mirrors
  `FixtureClient::push_response`. Tests push canned
  `Response`s in order; the mock serves them on each
  successive request. Lower-friction than a request-matcher
  DSL for our usage patterns.
- **Streaming is not in scope.** `GenAiClient` uses
  `client.exec_chat` (non-streaming) today. If we adopt
  streaming later, the mock can be extended; nothing here
  precludes it.
- **Anthropic-only.** We only use Anthropic today.
  Extension to other providers is straightforward (genai's
  `ServiceTargetResolver` handles them the same way) but
  out of scope for this plan.
- **Drift detector is manual-only.** Gated on a key + an
  env-var or `#[ignore]` switch. Run via
  `just acceptance-drift`. No nightly cron in this plan.
- **Request capture is opt-in.** The mock records received
  requests in an internal buffer; tests can assert on them
  via `mock.received_requests()` but don't have to. Catches
  "we forgot to send X" bugs when tests want that level.

## Approach: TDD per step

Same shape as the archive hand-off plan:

1. Acceptance test (red).
2. Integration tests (red).
3. Unit tests (red).
4. Implement until all three tiers pass.
5. Refactor with all tests green.

## Implementation Steps

### Step 1 — `base_url` plumbing

**Goal.** `AnthropicConfig` gains an optional `base_url`;
`GenAiClient` honors it.

The genai 0.4.4 client uses `ServiceTargetResolver` to
customise per-model endpoints. The integration is a small
closure in `GenAiClient::new`: when `base_url` is set, build
the client with a resolver that overrides the endpoint for
the Anthropic adapter.

#### Integration tests

- **`anthropic_config_parses_base_url_from_toml`** — load a
  config containing `[providers.anthropic]
  base_url = "http://127.0.0.1:0"`; verify the parsed value.
- **`anthropic_config_base_url_defaults_to_none`** — load a
  config without `base_url`; verify `None`.

#### Unit tests

- **`genai_client_with_base_url_uses_override`** — pure:
  construct `GenAiClient::new` with a base URL; resolve the
  service target for an Anthropic model; assert the endpoint
  is the override.

#### Done when

- [x] `AnthropicConfig.base_url: Option<String>` exists.
- [x] `GenAiClient::new(...)` accepts the base URL (signature
      TBD — likely a separate constructor `with_base_url` to
      keep `new()` unchanged for callers).
- [x] `fq.toml` template documents the new field with a
      commented example.
- [x] All listed tests green.

---

### Step 2 — `MockAnthropicServer` in `test_support`

**Goal.** Stand up an axum-backed mock that speaks the
Anthropic `/v1/messages` API and serves sequenced responses.
Captures requests for opt-in assertions.

#### API sketch

```rust
let mock = MockAnthropicServer::start().await;
mock.push_response(MockResponse::text("hello", input_tokens: 50, output_tokens: 10));
mock.push_response(MockResponse::tool_use("file_read", json!({"path": "x"}), id: "call_1"));
// build_client(&mock) returns a GenAiClient pointed at mock.base_url()
let client = build_client(&mock);
// ... run reducer ...
let requests = mock.received_requests();   // opt-in assertions
mock.shutdown().await;
```

`MockAnthropicServer` is `Send + Sync`. Multiple tests can
run in parallel; each spawns its own ephemeral-port mock.

#### Integration tests

- **`mock_returns_canned_text_response`** — push a text
  response; have a `GenAiClient` point at the mock; call
  `exec_chat`; assert the parsed `ChatResponse` matches.
- **`mock_returns_tool_use_response`** — same but
  `tool_use` block; assert the parsed shape includes a
  `ToolCallRequest`.
- **`mock_serves_responses_in_order`** — push two; make two
  requests; assert each got its turn.
- **`mock_captures_request_bodies`** — make a request;
  assert `received_requests()` contains the JSON body with
  the expected model + messages.
- **`mock_returns_400_when_responses_exhausted`** —
  protective behaviour: tests should not silently get empty
  responses if they under-push.

#### Unit tests

- **`mock_response_serialises_to_anthropic_shape`** — pure:
  `MockResponse::text(..)` → JSON matches Anthropic's
  documented `messages.create` response.
- **`mock_tool_use_response_serialises_to_anthropic_shape`** —
  pure: same for the tool-use shape.

#### Done when

- [x] `test_support::mock_anthropic::MockAnthropicServer`
      exists with `start`, `push_response`,
      `received_requests`, `base_url`, `shutdown`.
- [x] All listed tests green.
- [x] No new production dependencies — axum / hyper /
      reqwest stay test-only (`dev-dependencies` or
      `[dependencies] features = ["test"]`).

---

### Step 3 — Convert step-8 acceptance test to use the mock

**Goal.** The step-8 deferred acceptance test from
`docs/plans/closed/2026-05-16-archive-hand-off.md` —
`completed_invocation_archives_and_worker_cleans_up` —
becomes a real test that runs every build, against the mock.

#### Acceptance test

```text
TEST: completed_invocation_archives_and_worker_cleans_up

Setup:    Live NATS. MockAnthropicServer with a 1-turn
          conversation (canned text response). fq run as a
          single-node daemon configured with the mock URL.
Action:   Trigger sample-agent.
Assert:   - Worker's invocation_state row is removed within
            5 seconds of completion.
          - Control-plane's invocation_archive has the row
            with the final state.
          - NATS shows invocation.archived then
            invocation.archive_acked.
          - mock.received_requests() shows exactly one
            request with the expected model and system prompt.
```

Gated on `FQ_NATS_URL` only — no `ANTHROPIC_API_KEY` needed.

#### Done when

- [x] Test lives next to the other step-8 acceptance tests
      and runs by default in `cargo test`.
- [x] Parent plan
      ([`2026-04-28-data-architecture-v1.md`](./2026-04-28-data-architecture-v1.md))'s
      step-8 status block is updated to mark this previously
      "deferred" acceptance test as shipped.

---

### Step 4 — Manual drift detector

**Goal.** A real-Anthropic test that hits the live API to
catch protocol drift, runnable on demand.

The test is small and cheap: one short prompt to Haiku,
asserting that the response parses and contains a
non-empty text block. No tool-use, no streaming. Cost on
the order of fractions of a cent per run.

#### Mechanism

```rust
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live network; run via `just acceptance-drift`"]
async fn anthropic_real_api_basic_response_parses() {
    let Some(_) = std::env::var("ANTHROPIC_API_KEY").ok() else { return };
    // build GenAiClient with default (no base_url override)
    // call exec_chat with a minimal prompt against haiku
    // assert: parses without error, response.text is non-empty
}
```

`just acceptance-drift` runs `cargo test -- --ignored anthropic_real_api`.

#### Done when

- [x] `anthropic_real_api_basic_response_parses` test
      exists and passes against live Anthropic.
- [x] `just acceptance-drift` recipe runs it.
- [x] `services/fq-runtime/README.md` (or equivalent
      operator doc) documents the recipe: when to run it,
      cost expectation, what to do when it fails.

---

## Cross-cutting concerns

- **No regression on existing tests.** Each step's
  "Done when" includes a full `cargo test -p fq-runtime`
  pass.
- **Mock dependencies are test-only.** Axum / hyper / etc.
  must not leak into the production `fq-runtime` crate's
  dependency graph. `[dev-dependencies]` in `Cargo.toml`,
  or a feature-gated module if it's awkward to keep them
  separate.
- **Documentation lands with the code.** Step 1 updates
  `fq.toml`. Step 4 updates the operator README with the
  drift-detector recipe.

## Risks

| Risk | What would tell us | Mitigation |
|---|---|---|
| genai's `ServiceTargetResolver` doesn't cleanly support per-call endpoint override | Step 1's unit test fails or requires hacky workarounds | Fall back to constructing the client with an explicit auth + endpoint resolver pair. Worst case, fork the small piece of genai needed; size is bounded. |
| Mock and real Anthropic diverge on response shape | Step 4's drift detector starts failing; mock-based tests pass | This is exactly what the drift detector is for. When it trips, update the mock's response builders to match. |
| Test setup ergonomics push toward request-matcher style anyway | Tests grow ugly setup boilerplate to push sequenced responses for branched flows | Document the pattern in `test_support`; revisit the style decision if more than ~3 tests get awkward. |

## Closing condition

This plan closes when:

- All 4 steps' "Done when" boxes are ticked.
- Parent plan ([`2026-04-28-data-architecture-v1.md`](./2026-04-28-data-architecture-v1.md))'s
  step-8 status block notes the live acceptance test is now
  in CI (via the mock).
- This plan moves to `docs/plans/closed/`.
