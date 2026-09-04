# MCP dual-era client — migration to protocol `2026-07-28` on rmcp 3

> **Opened 2026-09-04.** Written as a hand-off: a session with no prior
> context should be able to read this file, then
> [ADR-0018](../../adrs/accepted/0018-mcp-server-initiated-execution.md),
> and start work. Line numbers and counts below are as of `main@ca3e8bc`;
> treat them as pointers, not facts. Every claim about the spec or the
> crate was read from the spec repository's `2026-07-28` revision and
> from the `rmcp 3.2.0` sources, not from memory.

## Goal

The runtime's MCP client speaks both protocol eras:

- **Legacy** (`2025-11-25` and earlier): the `initialize` handshake we
  ship today, unchanged on the wire.
- **Modern** (`2026-07-28` and later): no handshake, protocol version and
  client capabilities on every request, `server/discover` for version
  selection, and the Multi Round-Trip Request (MRTR) pattern in place of
  server-initiated requests.

A server of either era works without per-server configuration; the era
is detected, cached, and visible to the operator. Sampling, elicitation,
and roots keep working in both eras through their deprecation window,
because the runner-arbitrated bridge that ADR-0018 defines does not
depend on how the request reaches the client.

## Context

### Where we are

- `fq-runtime` speaks `2025-11-25` through `rmcp 1.8`, whose default
  negotiated version is that revision (`ProtocolVersion::LATEST`).
- The client surface is `src/mcp.rs` (1,843 lines, pinned in
  `.file-size-baseline` at 1,844 — it may only shrink), `src/mcp/{server_config,stdio,tests}.rs`,
  the server-request bridge `src/worker/reducer/runner/server_request.rs`
  (737 lines), and the validators in `src/policy.rs` (414 lines). The
  integration suite `tests/mcp_integration.rs` has 43 tests against the
  pinned reference server `@modelcontextprotocol/server-everything@2026.1.26`,
  over stdio and Streamable HTTP.
- Dependabot's `rmcp 1.8 → 3.2` PR (#585) fails with 51 errors: most are
  deprecation warnings (clippy runs with `-D warnings`) on the sampling,
  logging, and subscription types under SEP-2577; the rest are real
  breaks — the `raw` accessor is gone from `ContentBlock`, `Resource`,
  and `ResourceTemplate` (10 sites in `mcp.rs`), four model structs
  became non-exhaustive, and `Peer::subscribe` is deprecated.

### What changed in the protocol (`2025-11-25` → `2026-07-28`)

From the revision's own changelog:

1. **Stateless.** `initialize`/`notifications/initialized` and the
   session header are removed. Each request carries
   `io.modelcontextprotocol/protocolVersion` and
   `.../clientCapabilities` in `_meta`; a mismatch returns
   `UnsupportedProtocolVersionError` (`-32022`) and the client retries
   with a mutually supported version. `server/discover` is mandatory on
   servers and lets a client pick a version up front.
2. **MRTR replaces server-initiated requests.** `sampling/createMessage`,
   `elicitation/create`, and `roots/list` no longer arrive as requests
   from the server. A server returns `resultType: "input_required"` with
   `inputRequests`; the client fulfils them and retries the original
   request with `inputResponses` and the echoed `requestState`. Every
   result carries `resultType`; an absent one means `"complete"`.
3. **Deprecated under SEP-2577:** roots, sampling, and logging. They stay
   fully functional for at least twelve months after the revision
   (feature-lifecycle policy, SEP-2596); new implementations should not
   adopt them. Suggested replacements: tool parameters for roots, direct
   LLM APIs for sampling, stderr or OpenTelemetry for logging.
4. **Subscriptions.** The HTTP GET stream and `resources/subscribe` are
   replaced by `subscriptions/listen`, one long-lived POST stream the
   client opts into per notification type. SSE resumability and
   `Last-Event-ID` are gone. `ping`, `logging/setLevel`, and
   `notifications/roots/list_changed` are removed; log level is
   per-request via `_meta`.
5. **Smaller:** `Mcp-Method`/`Mcp-Name` headers required on HTTP posts;
   `ttlMs`/`cacheScope` on list and read results; resource-not-found is
   `-32602` instead of `-32002`; tasks move to an extension; OAuth
   client-registration changes (not used here).

### Interoperability, per the spec's compatibility matrix

| Client | Server | Outcome |
| --- | --- | --- |
| Legacy (us today) | Legacy or dual-era | Works |
| Legacy (us today) | Modern-only | **Fails** — `initialize` is unknown; no fall-forward |
| Dual-era (target) | Modern | Works; `server/discover` or a modern first request settles it |
| Dual-era (target) | Legacy | Works; the probe gets a non-modern error and the client falls back to `initialize` |

The risk is one-directional and grows as servers go modern-only.

### What rmcp 3.2 already provides

Read from the crate sources; this is what makes the plan tractable:

- `ClientLifecycleMode::{Initialize, Discover, Auto}` with
  `serve_with_lifecycle`. `Auto { preferred_versions, legacy_version }`
  probes `server/discover` and falls back to `initialize` on a
  non-modern error or after a 10 s silence. `KNOWN_VERSIONS` includes
  `2026-07-28`; `LATEST` (the legacy default) is still `2025-11-25`.
- Once the peer reports `2026-07-28`, the client attaches
  `ClientRequestMetadata` (version, client info, capabilities) to every
  request; the Streamable HTTP transport sets `MCP-Protocol-Version`,
  `Mcp-Method`, and `Mcp-Name`.
- MRTR is driven inside `call_tool`, `get_prompt`, and `read_resource`:
  each `InputRequest` is fulfilled through the **same `ClientHandler`
  methods** we already implement — `create_message`, `create_elicitation`,
  `list_roots` — up to `DEFAULT_MRTR_MAX_ROUNDS = 10`; `*_once` variants
  expose the rounds. So the ADR-0018 bridge is reused unchanged; only the
  transport of the request differs.
- `subscriptions/listen` is exposed as `listen(...)`; per-request log
  level via `RequestMetaObject::set_log_level`. The crate's own handler
  module carries `#![expect(deprecated)]` for the SEP-2577 types — the
  posture we adopt too.
- The server half (used by the in-process mock in `mcp/tests.rs`) is
  dual-era: it answers `initialize` with a legacy version and serves
  modern requests statelessly, so both eras are testable without a
  network fixture.
- MSRV 1.88; the toolchain pin is 1.95. Feature names we use are
  unchanged.

## Decisions taken on 2026-09-04

1. **Dual-era, detected, not configured.** Default lifecycle is `Auto`
   with `preferred_versions = [2026-07-28, 2025-11-25]` and
   `legacy_version = 2025-11-25`. A per-server override
   (`protocol: auto | legacy | modern`) exists only for servers that
   misbehave under the probe; it is not required for normal use.
2. **ADR-0018 stands.** The runner remains the sole arbiter; the bridge
   keeps its channel-and-oneshot shape; validators keep operating on the
   sampling and elicitation types. MRTR changes *when* a request arrives
   (inside an in-flight `tools/call`) and *how the answer returns* (as
   `inputResponses` on the retry), not who decides. An addendum to
   ADR-0018 records this; a new ADR records the era-detection and
   deprecation stance.
3. **Sampling, roots, and logging stay through the window.** We keep the
   grants, the capability advertisement, and the bridge, and add a
   dated removal issue keyed to the spec's twelve-month floor. Agent
   definitions that need an LLM inside a server are steered toward
   direct provider integration in the docs, not by removing the feature.
4. **Deprecation warnings are allowed narrowly, never globally.**
   `#[expect(deprecated, reason = "SEP-2577 ...")]` on the exact items
   that touch the deprecated types, so removal upstream fails loudly at
   those sites and nowhere else.
5. **`mcp.rs` shrinks, it does not grow.** All new code lands in new
   `src/mcp/` modules; content and lifecycle helpers move out of
   `mcp.rs` as part of the port, so the ratchet moves down.
6. **Test both eras against the same scenarios.** Every server-initiated
   scenario (sampling, elicitation, roots) and every utility scenario
   (logging, subscriptions, progress, cancellation) gets a modern twin,
   preferably parameterised over era rather than duplicated.

## Implementation steps

Each step is a PR against `main` and must leave `just ci` green. Step 1
is the only one that changes the lockfile.

### Step 0 — Unblock (no code)

- Close #585 with a pointer to this plan; the bump lands in Step 1 on a
  branch where the source changes accompany it.
- Add a Dependabot `ignore` for rmcp major versions with a comment
  naming this plan, removed in Step 8. Without it the weekly PR keeps
  reopening red.

### Step 1 — Mechanical port to rmcp 3.2, wire behaviour unchanged

Scope: compile and pass the existing suite on `rmcp = "3.2"` while still
using `ClientLifecycleMode::Initialize` everywhere. Nothing observable
changes for any server.

- Replace the `raw` accessor reads (`mcp.rs:706, 825, 867, 1788–1799`)
  with the new direct fields; move the content-conversion helpers
  (`prompt_seed_from_rmcp`, `prompt_content_from_rmcp`, `content_meta`,
  `render_resource_contents`) into a new `src/mcp/content.rs` so the
  ratchet on `mcp.rs` moves down instead of up.
- Construct the four now-non-exhaustive model structs through their
  constructors or `..Default::default()`.
- Scope `#[expect(deprecated, reason = "SEP-2577 deprecation window; see the dual-era plan")]`
  to: the `create_message`/`list_roots`/`on_logging_message` handler
  methods in `mcp.rs`, the sampling conversion functions in
  `server_request.rs` (`sampling_to_model_request`,
  `model_response_to_create_message`, `sampling_*_text`), the
  `Validator<CreateMessageResult>` impls and `sampling_output_chain` in
  `policy.rs`, and the affected fixtures in `runner/config.rs` and
  `runner/tests.rs`. No crate-level allow.
- `Peer::subscribe` is deprecated but functional in the legacy era; keep
  it behind the same scoped expectation. Modern subscriptions arrive in
  Step 5.
- Verification: the 43 integration tests against the pinned everything
  server (legacy, stdio and HTTP), `cargo test -p fq-runtime`,
  `just quality` (fmt, clippy with deny-warnings, size ratchets,
  coupling), `just audit` (rmcp 3.2 adds `base64 0.23`, `indexmap`,
  `uuid` to its graph; all already licence-allowed), and
  `cargo metadata --locked`.

### Step 2 — ADR: era detection and the deprecation stance

- **ADR-0036 (proposed): MCP dual-era client.** Decides: `Auto`
  lifecycle with the preference order above; the era and negotiated
  version are recorded per running server and cached for the life of
  the process (stdio) or origin (HTTP), re-probed on failure, as the
  spec asks; the per-server override and when it is legitimate; what we
  keep advertising (`sampling`, `elicitation`, `roots` — all still gated
  by grants per ADR-0017) and what we stop (`logging/setLevel` in the
  modern era, replaced by per-request log level); the twelve-month
  removal issue for sampling/roots/logging and what agent authors are
  told meanwhile.
- **ADR-0018 addendum** under Status: MRTR delivers server-initiated
  requests inside an in-flight `tools/call`/`prompts/get`/`resources/read`;
  the arbiter, validation seam, and result-return semantics (§2–§4) are
  unchanged; the recovery note (§5) gains the MRTR round cap.
- Verification: reviewed and accepted before Step 3 merges.

### Step 3 — Era detection in the client manager

- New `src/mcp/lifecycle.rs`: builds the `ClientLifecycleMode` from the
  per-server config, holds the resolved `Era { Modern(version) | Legacy(version) }`,
  and exposes it on `RunningServer`. `McpClientManager::start_*` switch
  from `serve` to `serve_with_lifecycle`.
- Server config (`server_config.rs`, the agent-definition frontmatter):
  optional `protocol: auto | legacy | modern`, default `auto`; document
  it next to `url:`.
- Operator surface: the negotiated protocol version and era per server
  in `fq status`/`doctor` output and the dashboard's server view, so a
  modern-only server that fails legacy fallback is diagnosable from the
  message the spec says such servers should return.
- Probe latency: `Auto` waits up to 10 s for a silent legacy stdio
  server. Measure against the pinned everything server (it answers
  unknown methods with an error immediately, so fallback is fast) and
  record the number; if any supported server is silent, the override is
  the escape hatch, not a longer default.
- Verification: unit tests in `mcp/tests.rs` with the in-process rmcp
  mock served as legacy-only, modern-only, and dual-era, asserting the
  resolved era and version and that the legacy path is byte-identical to
  Step 1 (same `initialize` params). Integration: the existing
  `negotiates_full_capability_set` gains a modern twin once Step 6's
  fixture exists.

### Step 4 — MRTR for sampling, elicitation, and roots

The bridge is reused; this step proves it and closes the gaps.

- Confirm the runner's `run_tool` select loop services the bridge
  oneshot while the `call_tool` future is parked in the crate's MRTR
  loop (the handler is invoked from inside that future). Add a unit test
  with the in-process modern mock returning `input_required` for a tool
  call so the round trip is exercised without a network.
- Round accounting: the crate caps rounds at 10 per request; the
  runtime already has its own sub-budget and retry limits
  (`sampling_over_subbudget_is_declined_without_a_model_call`,
  `elicitation_retries_exhausted_declines`). Decide which limit wins and
  make the decline reason say which one fired. If the runtime needs
  per-round visibility (cost accounting per sampling round), switch the
  tool call to `call_tool_once` and drive the rounds in
  `server_request.rs`; otherwise keep the crate's loop.
- `requestState` is echoed verbatim by the crate; it is server-owned
  and opaque to us. Note it in the ADR-0018 addendum, no runtime work.
- Elicitation: `notifications/elicitation/complete` and `elicitationId`
  are gone in the modern era; the URL-mode path in `handle_elicitation`
  must not wait for them when the era is modern.
- Verification: modern twins of `sampling_request_bridges_to_the_host`,
  the evaluator matrix, `sampling_permitted_runs_on_the_agent_model`,
  the elicitation scenarios, and `read_roots_list`/roots advertisement,
  parameterised over era where the fixture allows.

### Step 5 — Modern-era utilities

- **Logging:** in the modern era there is no `logging/setLevel`; set the
  agent's configured level through `set_log_level` on request metadata,
  and keep `on_logging_message` forwarding (messages ride the response
  stream of the request that set the level). Legacy keeps `set_level`.
  Twin of `server_log_messages_are_forwarded_after_set_level`.
- **Subscriptions:** modern resource updates and list-changed
  notifications come through one `listen(...)` stream per server. Wire
  it in `drain_server_notifications` with opt-in for
  `resourceSubscriptions` and the three `*ListChanged` types; keep
  `Peer::subscribe` for legacy. Twins of
  `subscribe_delivers_resource_update_notifications` and
  `refresh_tools_rediscovers_the_tool_list`.
- **Caching hints:** `tools/list` and friends now carry `ttlMs`; let
  `McpToolRefresher` honour it as a floor on refresh cadence. Optional,
  small, and it improves prompt-cache hit rates the spec calls out.
- **Error codes:** resource-not-found is `-32602` in the modern era and
  `-32002` in legacy; `McpResourceTool`'s error mapping accepts both.
- **Progress and cancellation:** unchanged in the spec; run the existing
  tests in both eras.

### Step 6 — Fixtures and the HTTP transport

- Find the first `@modelcontextprotocol/server-everything` release that
  speaks `2026-07-28` (the TypeScript SDK is Tier 1 and shipped with the
  GA) and pin it as `EVERYTHING_SERVER_MODERN` alongside the legacy pin;
  run the suite against both. If a modern release is not yet published,
  the in-process rmcp mock is the modern fixture and this item becomes a
  follow-up issue.
- Streamable HTTP, dual-era: the spec's HTTP detection is "attempt a
  modern request, inspect the body of a 400"; confirm what
  `Auto` does over HTTP in the crate (its docs describe the stdio probe)
  and add a twin of `streamable_http_transport_discovers_tools_over_http`
  for each era. No session or resumability assumptions exist in our HTTP
  config today, so nothing to remove.
- Verification: full `just ci`; the live suites unchanged.

### Step 7 — Deprecation posture and agent-author guidance

- Open the dated removal issue: "sampling, roots, logging removal-eligible
  after 2027-07-28"; link it from ADR-0036 and from the `#[expect]`
  reasons.
- Agent-definition docs: sampling still works and is still gated by the
  `sampling` grant, but servers that need an LLM should integrate with
  the provider directly; roots are informational and tool parameters are
  preferred.
- Verification: `just lint-docs` and `just check-links`.

### Step 8 — Documentation and close

- `ARCHITECTURE.md` MCP section: eras, detection, MRTR.
- Remove the Dependabot ignore from Step 0. Move this plan to
  `closed/` with the verification results and the measured probe
  latency.

## Cross-cutting concerns

- **Size ratchets.** `mcp.rs` is pinned at 1,844 lines and may only
  shrink; `server_request.rs` (737) and `policy.rs` (414) are under
  budget but the function ratchet (250 lines) applies. New behaviour
  goes in `src/mcp/{content,lifecycle,subscriptions}.rs`.
- **Clippy with deny-warnings.** Deprecations are expectations with
  reasons, per Decision 4; a global allow would hide the upstream removal
  when it lands.
- **Crate churn.** The Rust SDK's support for `2026-07-28` is described
  as beta by the spec maintainers. Pin an exact `rmcp` version in the
  workspace manifest for the duration of the plan and let Dependabot
  propose minors after Step 8.
- **Coupling.** The bridge already depends on `mcp` types only through
  the `ServerRequest` enum; keep it that way so era-specific code does
  not leak into the reducer.
- **Security seam.** ADR-0018 §4's validators run on the same types in
  both eras; the redactor on the outbound `CreateMessageResult` path is
  exercised by the modern twins too.

## Risks and what we'll learn

- **Probe latency on silent legacy servers** (up to 10 s at startup).
  Measured in Step 3; mitigated by the per-server override and by the
  spec's advice to cache the era per server configuration.
- **MRTR inside a pending tool call** may interact with the runner's
  cancellation and budget accounting differently from a server-pushed
  request. Step 4's unit test is designed to surface this before the
  integration twins.
- **Modern fixture availability.** If no everything-server release
  speaks the new revision yet, modern coverage rests on the in-process
  mock until one appears.
- **Upstream removal of sampling** lands as compile errors at the
  expected sites; that is the designed failure mode.

## Closing condition

- `rmcp 3.x` on `main`; the legacy suite green against the legacy pin;
  the modern twins green against a modern fixture; both eras exercised
  for sampling, elicitation, roots, logging, subscriptions, progress, and
  cancellation.
- Era and version visible per server in the operator surface.
- ADR-0036 accepted, ADR-0018 addendum merged, removal issue open and
  dated, Dependabot ignore removed, this plan closed.

## References

- Spec revision `2026-07-28`: changelog, `basic/versioning.mdx`
  (negotiation, compatibility matrix), `basic/transports/streamable-http.mdx`
  (backward compatibility), `seps/2577-deprecate-roots-sampling-and-logging.md`,
  `seps/2596-spec-feature-lifecycle-and-deprecation.md`, in the
  `modelcontextprotocol/modelcontextprotocol` repository.
- `rmcp 3.2.0`: `src/service/client.rs` (`ClientLifecycleMode`,
  `select_protocol_version`, `fulfill_input_request`, `listen`),
  `src/model/mrtr.rs`, `src/model/meta.rs`, `src/handler/client.rs`.
- [ADR-0017](../../adrs/accepted/0017-mcp-human-in-the-loop.md),
  [ADR-0018](../../adrs/accepted/0018-mcp-server-initiated-execution.md),
  [the full-spec plan](../closed/2026-05-28-mcp-client-full-spec.md),
  [the completion plan](../closed/2026-06-04-mcp-completion.md).
- Dependabot PR #585 (the failing bump).
