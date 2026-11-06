# MCP protocol versions — 2026-11-06

Taken against `main` @ `40417719` (2026-11-06). This is a survey of every
published revision of the Model Context Protocol: what each one added,
changed and removed, and what the current revision means for factor-q's
MCP client. It also covers which revisions servers and clients actually
speak, and which ones each rmcp release implements, read from the crate
source. It was written as input to moving factor-q off rmcp 1.x, which
Dependabot raised as an rmcp 3.x bump in
<https://github.com/bricef/factor-q/pull/927>. It is a snapshot: the
specification and rmcp both move every few months, so check the
[sources](#sources) before relying on a detail here. Decisions that come out
of it belong in ADRs.

## Summary

- MCP has five published revisions: `2024-11-05`, `2025-03-26`,
  `2025-06-18`, `2025-11-25` and `2026-07-28`. The last one is current.
- factor-q is a `2025-11-25` client
  ([MCP guide](../guide/mcp.md)). rmcp 1.8, which it builds against,
  recognises the `2026-07-28` version string, but its `LATEST` is
  `2025-11-25`. Only rmcp 3.x is still released: 1.x ended at 1.8.0 on
  2026-06-23.
- `2026-07-28` is a rewrite. It has no `initialize` handshake and no
  sessions: every request carries its own version and capabilities.
- `2026-07-28` also stops servers from sending requests to the client while
  a call is open. Instead, a call can end early with "input required". The
  client then sends the same call again with the answers. This is
  **multi round-trip requests (MRTR)**.
- **Elicitation is not removed.** It moves onto MRTR and stays an active
  feature. **Sampling, roots and logging are deprecated.** They still work,
  and none can be removed before the first revision released on or after
  2027-07-28.
- A client that speaks only the new revision cannot talk to an older
  server, and the reverse is also true. factor-q will meet both kinds of
  server for a long time, so the rmcp 3.x migration means a client that
  speaks both. Sampling and elicitation then reach the runner in two shapes.
- The largest structural change for factor-q is sampling. Today it happens
  *inside* one open tool call. Under MRTR it happens *between* separate
  tool-call requests, which reaches the WAL, deadlines, Round numbering
  and the resume simulation.
- In practice, almost every server and client still speaks the handshake
  era.
  - About 77% of reachable public servers negotiate `2025-11-25`.
  - About 14% also answer the new protocol.
  - About 0.4% require it.
  - Most major clients other than Claude Code have not moved.
  - Nothing yet forces factor-q onto the new protocol.
- That allows a lower-churn route: upgrade to a maintained rmcp 3.x, but
  pin the handshake era, and leave the new protocol as later, separate
  work. An unchanged bump does not do this. It changes the version
  factor-q offers, does not reach rmcp's MRTR handling, and runs into
  rmcp's deprecation of the sampling, roots and logging types. See
  [Two routes](#two-routes).

## How MCP versions work

A version string is the date of the last *breaking* change. Changes that
stay backwards compatible land in the current revision without a new
version. Revisions are marked Draft, Current or Final.

`2026-07-28` also adopted the first formal feature lifecycle. A feature can
be marked **Deprecated**: it stays in the specification, new
implementations should not adopt it, and it has to stay for at least twelve
months before it becomes eligible for removal. An expedited path allows
ninety days. The specification keeps a
[registry of deprecated features](https://modelcontextprotocol.io/specification/2026-07-28/deprecated).
Nothing has been removed under the policy yet.

How the two sides agree on a version changed in `2026-07-28`:

- **Up to `2025-11-25`** ("legacy" in the specification's terms): the
  client sends `initialize` with the version it wants, and the server
  answers with the version it will use. The session then runs at that
  version.
- **From `2026-07-28`** ("modern"): every request names its version in
  `_meta` (`io.modelcontextprotocol/protocolVersion`), and on HTTP also in
  the `MCP-Protocol-Version` header. The server accepts or rejects each
  request on its own. If it rejects one, it sends
  `UnsupportedProtocolVersionError` with the versions it does support.

## The revisions

### 2024-11-05 — the first release

- JSON-RPC 2.0 messages over **stdio** or **HTTP with SSE**.
- An `initialize` / `notifications/initialized` handshake negotiates
  capabilities and opens a stateful connection.
- Servers offer **tools**, **resources** (with subscriptions and
  list-changed notifications) and **prompts**.
- Clients offer **sampling**, where the server asks the client's model for
  a completion, and **roots**, where the server asks which filesystem paths
  the client's workspace covers. Roots come with `roots/list_changed`.
- Utilities: ping, cancellation, progress, logging (`logging/setLevel`,
  `notifications/message`), argument completion and pagination.
- No authorization framework.

### 2025-03-26 — remote servers

- An **authorization framework** based on OAuth 2.1.
- **Streamable HTTP** replaces HTTP with SSE. It uses one endpoint, an
  optional SSE stream per response, and an `Mcp-Session-Id` header for the
  session.
- **JSON-RPC batching**.
- **Tool annotations** describing a tool's behaviour, such as read-only or
  destructive.
- Audio content, a `message` field on progress notifications, and an
  explicit `completions` capability.

### 2025-06-18 — structured output and elicitation

- **Elicitation**: a server can ask for structured input matching a JSON
  schema while it is handling a call.
- **Structured tool output** (`outputSchema` and `structuredContent`), and
  resource links in tool results.
- JSON-RPC batching is **removed** again.
- Authorization: MCP servers are classed as OAuth resource servers, with
  protected resource metadata, and clients must send RFC 8707 resource
  indicators.
- Smaller changes:
  - The `MCP-Protocol-Version` header becomes required on HTTP after
    `initialize`.
  - `title` fields arrive for display names.
  - `_meta` is allowed on more types.
  - Completion requests gain a `context` field.

### 2025-11-25 — the last revision with a handshake

- **Tasks** (experimental): a request can be tracked durably, and its
  result fetched later by polling.
- **URL-mode elicitation**: the server sends the user to a URL, for example
  to authorise a downstream service, and is told when that is done.
- **Sampling with tools**: a sampling request can carry `tools` and
  `toolChoice`.
- Icons on tools, resources, resource templates and prompts.
- A standards-based enum schema for elicitation (titled, untitled,
  single-select and multi-select), and default values for elicitation
  fields.
- JSON Schema 2020-12 becomes the default dialect.
- Authorization gains OpenID Connect discovery, Client ID Metadata
  Documents (recommended for registering a client) and incremental scope
  consent through `WWW-Authenticate`.
- Invalid tool input is reported as a tool error, not a protocol error, so
  the model can correct itself.
- A server may close an SSE stream at will, and the client polls to get the
  rest.
- The `includeContext` values `thisServer` and `allServers` are
  soft-deprecated.

### 2026-07-28 — stateless (current)

The launch post describes this revision as the move from a stateful,
bidirectional protocol to a stateless request/response one. The goal is
that any request can reach any server instance behind a plain load
balancer, with no shared storage between instances.

**Removed or replaced:**

- The `initialize` handshake and protocol-level sessions, including
  `Mcp-Session-Id`.
  - Every request carries its version, client info and client capabilities
    in `_meta`.
  - Servers must implement `server/discover`, which returns their supported
    versions, capabilities and identity in one call.
  - A server that needs state across calls mints its own handles and passes
    them as ordinary tool arguments.
- Requests sent from server to client are replaced by MRTR (see below).
- The HTTP GET stream and `resources/subscribe` are replaced by
  `subscriptions/listen`.
  - This is one long-lived stream, on which the client opts in to each kind
    of change notification: tool list, prompt list, resource list and
    resource subscriptions.
  - Progress and log messages still arrive on the response stream of the
    request they belong to.
- `ping` is removed.
- `logging/setLevel` is removed. A request sets its own log level in
  `_meta` (`io.modelcontextprotocol/logLevel`), and a server must not send
  log messages for a request that set none.
- `notifications/roots/list_changed` is removed.
- SSE resumability (`Last-Event-ID`) is removed. If a response stream
  breaks, the request is lost and the client must send it again with a new
  id.
- From URL-mode elicitation, the `notifications/elicitation/complete`
  notification and the `elicitationId` field are removed. The client learns
  the outcome by retrying the original request.
- Tasks leave the core and become the `io.modelcontextprotocol/tasks`
  extension. The extension is redesigned around polling `tasks/get`, adds
  `tasks/update` for client input, and drops `tasks/list`.

**Added:**

- `resultType` on every result: `"complete"`, or `"input_required"` for an
  MRTR interim result.
- An `extensions` field in client and server capabilities.
- `Mcp-Method` and `Mcp-Name` headers on Streamable HTTP requests, so
  intermediaries can route.
- Cache hints: `ttlMs` and `cacheScope` on list results and
  `resources/read`.
- `tools/list` should come back in a deterministic order, which helps
  client caching and LLM prompt caching.
- Tool `inputSchema` and `outputSchema` may use any JSON Schema 2020-12
  keyword.
- OpenTelemetry trace context conventions in `_meta`.
- Authorization adds `iss` validation (RFC 9207). Client credentials are
  now bound to the authorization server that issued them.
- Error codes are renumbered into an MCP-reserved range
  (`UnsupportedProtocolVersion` is `-32022`). "Resource not found" becomes
  `-32602`.

**Deprecated**, with the specification's suggested migration:

| Feature | Migration path | Earliest removal |
|---|---|---|
| Sampling | Integrate directly with LLM provider APIs | First revision on or after 2027-07-28 |
| Roots | Pass directories or files as tool parameters, resource URIs or server configuration | First revision on or after 2027-07-28 |
| Logging | Log to `stderr` on stdio; use OpenTelemetry | First revision on or after 2027-07-28 |
| Dynamic Client Registration | Client ID Metadata Documents | First revision on or after 2027-07-28 |
| `includeContext: thisServer` / `allServers` | Omit the field, or use `none` | No later than Sampling itself |
| HTTP with SSE transport | Streamable HTTP | Three months after the lifecycle SEP is final |

#### Multi round-trip requests

Today, a server handling `tools/call` that needs a model completion sends
`sampling/createMessage` back to the client while the tool call is still
open. The client answers, and the server finishes the tool call. Under
`2026-07-28` the same exchange runs like this:

1. The client sends `tools/call` with JSON-RPC id 1.
2. The server answers id 1 with a *result* whose `resultType` is
   `"input_required"`. It carries:
   - `inputRequests`, a map of named elicitation, sampling or roots
     requests;
   - `requestState`, an opaque string only the server can read.
3. Request id 1 is now over. The client gathers the answers, by asking a
   user or calling a model.
4. The client sends `tools/call` again with a **new id** and the same
   parameters. It adds `inputResponses`, keyed by the same names, and
   `requestState` copied back exactly.
5. The server completes, or asks again.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "resultType": "input_required",
    "inputRequests": {
      "capital_of_france": {
        "method": "sampling/createMessage",
        "params": {
          "messages": [
            { "role": "user", "content": { "type": "text", "text": "What is the capital of France?" } }
          ],
          "maxTokens": 100
        }
      }
    },
    "requestState": "AEAD-protected blob"
  }
}
```

The rules that matter for a client:

- MRTR applies to `tools/call`, `prompts/get` and `resources/read` only.
- A server must not ask for anything the client did not declare in its
  capabilities.
- The server keeps nothing between the requests. Whatever it needs travels
  in `requestState`.
- `requestState` should be tamper-proofed, bound to the user and the
  originating request, and given a short expiry. Single use is not
  guaranteed unless the server enforces it.
- A server may answer with `input_required` again and again. It must not
  assume the client will ever retry.

#### Talking across the two eras

The specification calls an implementation that speaks both legacy and
modern revisions "dual-era". Its compatibility matrix comes down to this:

- **A modern-only client cannot talk to a legacy server.**
- **A legacy client cannot talk to a modern-only server.** That server
  rejects `initialize`.
- A dual-era client works with both kinds of server.
  - On stdio it probes with `server/discover`, and falls back to
    `initialize` on any error that is not a recognised modern one.
  - On HTTP it tries a modern request, and falls back when the 400 response
    has no modern error body.
  - The result is cached for the life of the server process or HTTP origin.
- A dual-era server picks the era from how the client opens: modern
  `_meta` on a request, or an `initialize`.

## Feature status at a glance

These are the features factor-q's client touches today, with their status
in each era:

| Feature | Added | In 2025-11-25 (legacy) | In 2026-07-28 (modern) |
|---|---|---|---|
| Tools, resources, prompts | 2024-11-05 | Active | Active; list results gain cache hints |
| Sampling | 2024-11-05 | Request sent by the server during a call | **Deprecated**; arrives via MRTR |
| Elicitation, form mode | 2025-06-18 | Request sent by the server during a call | Active; arrives via MRTR |
| Elicitation, URL mode | 2025-11-25 | Active, with a completion notification | Active; completion notification removed |
| Roots | 2024-11-05 | Request from the server, plus `list_changed` | **Deprecated**; arrives via MRTR; `list_changed` removed |
| Logging | 2024-11-05 | `setLevel`, plus log messages on the session | **Deprecated**; level set per request |
| List-changed and resource-updated notifications | 2024-11-05 | On the session | Only on a `subscriptions/listen` stream |
| Progress, cancellation, pagination | 2024-11-05 | Active | Active; progress stays request-scoped |
| Ping | 2024-11-05 | Active | Removed |
| Tasks | 2025-11-25 | Experimental, in the core | Extension, redesigned |
| Handshake and sessions | 2024-11-05 | Required | Removed |

## The landscape

Most of the ecosystem is still on the handshake era. These figures come
from third-party measurements, so treat them as indicative. They are
consistent with each other where they overlap.

**Servers.** Two independent probes of remote servers listed in the
official registry got almost the same answer three weeks apart. Each sent
an unauthenticated `initialize` and recorded the negotiated version:

| Negotiated version | 2026-11-05 (DigitalApplied, 550 handshakes) | 2026-10-14 (LocalCan, 257 handshakes) |
|---|---|---|
| `2025-11-25` | 76.9% | 76.7% |
| `2025-06-18` | 16.0% | 16.3% |
| `2025-03-26` | 3.3% | 3.5% |
| `2024-11-05` | 3.3% | 3.1% |

- **The new protocol, fourteen weeks after release:**
  - 140 of 970 streamable endpoints (14.4%) answered `server/discover`.
  - 137 of those also completed the legacy handshake.
  - Requiring the new protocol was rare: one server in LocalCan's 257.
- **Many endpoints did not answer at all.** Only 55% of the sampled
  registry endpoints completed a handshake. 34% demanded authentication and
  about 11% were broken or unreachable.
- **Unregistered servers lag further.** Censys's scan of 12,520
  internet-exposed servers on 2026-04-28 found 89.4% on `2025-03-26` and
  0.7% on `2025-11-25`, five months after `2025-11-25` shipped.
- **Most servers are small and local.** The registry lists about 57,000
  servers. 97.1% of the packaged ones run over stdio only.

**Clients.** One community survey, dated 2026-10-17, checked each client
by observation, from source, or from its documentation:

| Client | What it speaks |
|---|---|
| Claude Code 2.1.302 | Tries `server/discover` first, and falls back to `initialize` at `2025-11-25` (observed) |
| Codex 0.163.0 | `2025-06-18` by default; the modern path sits behind a flag that is off (observed) |
| Cursor CLI 2026.10.16 | Nothing newer than `2025-11-25` in the installed bundle |
| VS Code 1.142.0 | `2025-11-25` via `initialize` (source) |
| Gemini CLI 0.64.0 | `2025-06-18`, through an SDK that does not accept `2025-11-25` (source) |
| ChatGPT, claude.ai, Claude Desktop | Not documented for ordinary servers |

- Sampling is barely implemented anywhere. claude.ai lists it as "not yet"
  supported.
- This matches the reason SEP-2577 gave for deprecating it: "low adoption
  relative to their implementation complexity".

**SDK churn.** The cost has fallen mainly on SDK users.

- **Python SDK v2** renamed `McpError` to `MCPError`. That broke the
  official `fetch` and `time` reference servers at startup. They were
  ported by hand, and the merged port (2026-10-07) serves only the legacy
  era.
- **TypeScript SDK v2** split the SDK into new packages and changed handler
  signatures. In the thirty days to mid-October, v1 was downloaded 273.3
  million times against 37.3 million for the v2 server package.
- **Public reaction has been muted.** The Hacker News thread on
  `2026-07-28` drew 127 points and 41 comments, and was mixed. The
  maintainers acknowledged the cost themselves: "If you built your own
  implementation, it's going to be a lot of uplift to make this correct."

## The rmcp line

Read from the source of every rmcp release on crates.io: the version
strings in `ProtocolVersion` (`model.rs`) and the version each release
offers by default (`LATEST`).

| rmcp | Released | Versions recognised | Offered by default |
|---|---|---|---|
| 0.1.x | from 2025-03-16 | `2024-11-05` | `2024-11-05` |
| 0.2 – 0.5 | from 2025-07-02 | + `2025-03-26` | `2025-03-26` |
| 0.6 – 0.16 | from 2025-08-19 | + `2025-06-18` | `2025-03-26` |
| 0.17 – 1.4 | from 2026-02-27 | same | `2025-06-18` |
| 1.5 – 1.7 | from 2026-04-16 | + `2025-11-25` | `2025-11-25` |
| **1.8** (factor-q) | 2026-06-23 | + `2026-07-28`, as a string only | `2025-11-25` |
| 2.0 – 3.4 | 2026-06-29 to 2026-09-23 | all five | `2025-11-25` |
| 3.5 – 3.9 | from 2026-09-28 | all five | `2026-07-28`; `LATEST_WITH_INITIALIZE` stays `2025-11-25` |

- **Recognising a version is not implementing it.**
  - rmcp 1.8 knows the `2026-07-28` string but has no `server/discover` or
    MRTR. Those arrive in 3.0.
  - Features also land ahead of the string. 1.0.0 already carried the
    tasks types from `2025-11-25`, a version it did not recognise until
    1.5.0.
- **rmcp adopts each version as its default late:**

  | Spec version | Default from | Lag |
  |---|---|---|
  | `2025-03-26` | 0.2.0 | about 3 months |
  | `2025-06-18` | 0.17.0 | about 8 months |
  | `2025-11-25` | 1.5.0 | about 5 months |
  | `2026-07-28` | 3.5.0 | about 2 months |

  The MCP launch post listed the Rust SDK's `2026-07-28` support as beta on
  release day; 3.0.0 reached crates.io the same day.
- **Only 3.x is maintained.**
  - rmcp went through seventeen 0.x lines, then three majors in five
    months: 1.0.0 on 2026-03-03, 2.0.0 on 2026-06-29 and 3.0.0 on
    2026-07-28.
  - 1.x stopped at 1.8.0 (2026-06-23), and 2.x at 2.2.0 (2026-07-08).
  - 3.x has had 22 releases since 2026-07-23, the latest 3.9.0 on
    2026-11-05.
- **#927 is red.** <https://github.com/bricef/factor-q/pull/927> bumps to
  3.8.0 and fails Code quality, Rust CI, Runtime CI, Dashboard E2E and
  Container image: the tree does not compile against it.

### How the rmcp 3.9 client picks an era

From `service/client.rs` in rmcp 3.9.0:

- **`ServiceExt::serve()`**, which factor-q calls today, still runs the
  legacy `initialize` handshake. Its own doc comment says "Existing
  `ServiceExt::serve` behavior remains legacy initialization."
  - It offers whatever version the handler's `get_info()` returns.
  - If the server answers with `2026-07-28`, rmcp switches the connection
    to per-request `_meta`.
- **`serve_with_lifecycle(ClientLifecycleMode::Discover { .. })`** speaks
  the modern era only. A legacy server is an error.
- **`serve_with_lifecycle(ClientLifecycleMode::Auto { .. })`** probes with
  `server/discover`. It falls back to `initialize` when the server answers
  as legacy, or when it says nothing for 10 seconds.
- **MRTR, the high-level way.** `call_tool` and `get_prompt` drive
  `input_required` rounds themselves. They answer each input request
  through the same `ClientHandler` callbacks as today (`create_message`,
  `create_elicitation`, `list_roots`), up to `DEFAULT_MRTR_MAX_ROUNDS`
  (10).
- **MRTR, the low-level way.** `call_tool_once` and its siblings return the
  `InputRequiredResult` to the caller and drive no further rounds.
- **Change notifications.** On a modern connection, list-changed
  notifications arrive only on a stream the client opens with
  `Peer::listen`. Anything routed there is not also delivered to the
  `on_*_list_changed` callbacks.
- **Deprecations.** 3.9 marks these `#[deprecated]` ("will be removed in a
  future release"):
  - **Since 1.8:** the sampling, roots and logging capability fields,
    `logging/setLevel`, and the resource subscribe and unsubscribe
    requests.
  - **New in 3.x:** `LoggingLevel`, the `ClientInfo` alias (in favour of
    `ClientConfig`), and the sampling-with-tools content types.

## What changes for factor-q

The first two sections apply to any move to rmcp 3.x. The rest cover modern
servers only: a legacy server keeps today's behaviour exactly, and a
dual-era client has to keep both paths.

### What any rmcp 3.x upgrade runs into

All three were read from source; none has been compiled.

1. **The offered version changes silently.**
   - factor-q's handler builds `ClientInfo::default()`
     ([`mcp/handler.rs`](../../services/fq-runtime/crates/fq-runtime/src/mcp/handler.rs),
     `get_info`), whose protocol version is `ProtocolVersion::default()`,
     that is `LATEST`.
   - On rmcp 3.5 and later that is `2026-07-28`, so an unchanged upgrade
     sends `initialize` offering the modern version.
   - Legacy servers answer with their own version, as handshake
     negotiation allows. A dual-era server may accept `2026-07-28` and move
     the connection to the modern era without factor-q deciding to.
   - To stay in the handshake era, the handler has to set
     `ProtocolVersion::LATEST_WITH_INITIALIZE`.
   - Pinning is not a complete guarantee. In the November census, two
     servers answered an `initialize` that offered `2025-11-25` with
     `2026-07-28` anyway, and rmcp then switches the connection to
     per-request `_meta`.
2. **rmcp's MRTR handling does not reach factor-q's tool calls.**
   - factor-q does not use rmcp's `call_tool`.
     [`mcp/call.rs`](../../services/fq-runtime/crates/fq-runtime/src/mcp/call.rs)
     sends a raw `CallToolRequest` through `send_cancellable_request`. That
     lets it read back rmcp's minted progress token
     (<https://github.com/bricef/factor-q/issues/605>) and cancel at the
     tool deadline.
   - It accepts only `ServerResult::CallToolResult`. On a modern
     connection an `InputRequiredResult` would fail as "unexpected response
     type".
   - So the modern era needs factor-q to drive the rounds itself, or to
     move to `call_tool` and rebuild progress attribution and cancellation
     on top of it.
3. **More deprecations reach factor-q's code.**
   - **Already there in rmcp 1.8:** the roots and sampling capability
     fields, and `logging/setLevel`, are all deprecated.
     - factor-q carries one counted `#[allow(deprecated)]` for
       `set_logging_level`
       ([`mcp/requests.rs`](../../services/fq-runtime/crates/fq-runtime/src/mcp/requests.rs)).
     - Its assignments to the deprecated capability fields pass clippy
       today.
   - **New in rmcp 3.x:** `LoggingLevel` and the `ClientInfo` alias are
     deprecated, and `mcp/handler.rs` uses both.
     - `ClientInfo` only needs renaming to `ClientConfig`.
     - `LoggingLevel` is how log notifications are folded into tracing.
       Each new allow it needs is a new `.allow-baseline` entry, which the
       ratchet refuses.
   - How many warnings actually appear will only be known once the tree
     compiles against 3.x. Per AGENTS.md, how to handle them is a
     maintainer's call, not something to work around.

### Two routes

**Route A: a maintained library, still in the handshake era.**

- What it involves:
  - Upgrade to rmcp 3.9.
  - Keep `serve()`, and pin the offered version to
    `LATEST_WITH_INITIALIZE`.
  - Fix the API breakage that turns #927 red.
  - Settle the deprecated items.
- Protocol behaviour barely changes. Every server factor-q reaches today it
  still reaches, and a modern-only server stays unreachable, as it is today.
  The exception is the rare server that answers a `2025-11-25` offer with
  `2026-07-28`, which still lands in the modern era.
- Given [the landscape](#the-landscape), that costs almost nothing now.
- It moves factor-q onto the only rmcp line that still receives fixes.

**Route B: dual-era.**

- Move to `serve_with_lifecycle(Auto { .. })`, and take on the modern-era
  work in the sections below:
  - drive MRTR, with the WAL, deadline and leg-bound questions it raises;
  - open `subscriptions/listen` streams;
  - set a log level on each request.
- Two behaviours to plan for:
  - The `Auto` probe can add up to 10 seconds to starting a legacy stdio
    server that ignores `server/discover`.
  - Sampling and elicitation reach the runner in two shapes.

Route A can come first, with Route B as its own later decision once
modern-only servers become a real share of what agents connect to.

### Sampling and elicitation

Today the runner services sampling and elicitation in the middle of a tool
call, as [ADR-0018](../adrs/accepted/0018-mcp-server-initiated-execution.md)
decided.
[`await_tool_under_deadline`](../../services/fq-runtime/crates/fq-runtime/src/worker/reducer/runner/deadline.rs)
waits on the tool's future and on the server's request channel at the same
time. The model call is recorded in the WAL inside the tool call's span.
<https://github.com/bricef/factor-q/pull/898> made resume after sampling
equivalent to an uninterrupted run, and gave the sampling call the Round of
the tool call it serves.

Against a modern server, one logical tool call becomes several wire
requests with model calls between them. Each step of the list below is
one of those requests:

1. `tools/call` returns `input_required`.
2. factor-q runs the sampling or elicitation completion through the same
   gates, budget and validation seam as today.
3. factor-q sends `tools/call` again with the answers and the server's
   `requestState`.

Consequences to design for:

- **The WAL.**
  - The tool span holds legs rather than one dispatch.
  - A crash between legs leaves factor-q holding the server's
    `requestState` and its own answers.
  - Resuming by re-sending the retry is a new possibility. The
    specification says the server keeps no state between legs, but nothing
    stops a server acting before it asks for input. A short `requestState`
    expiry can also make a late retry fail.
  - Today a crash with a tool call in flight is refused as ambiguous. That
    stance needs re-deciding for the gap between legs.
- **Deadlines.** Does one tool deadline cover every leg, including the
  model calls in between?
- **A bound on legs.** A server may answer `input_required` indefinitely.
  The client needs a cap per tool call, in the same way
  `max_discovery_pages` caps a cursor that never ends. rmcp's own helpers
  default to 10 rounds.
- **Where the loop lives.** If factor-q moved to rmcp's `call_tool`, the
  legs would stay inside one future. The sampling callback would fire while
  it is pending, much as it does today, and the WAL would keep a single
  tool span. The cost is giving up the raw request factor-q uses for
  progress attribution and cancellation (see
  [What any rmcp 3.x upgrade runs into](#what-any-rmcp-3x-upgrade-runs-into)).
  Driving the loop itself with `call_tool_once` keeps those, but makes each
  leg visible to the runner.
- **Rounds.** The rule from #898 still reads naturally: every leg, and
  every completion between legs, belongs to the tool call's Round.
- **Elicitation** follows the same shape. factor-q advertises form mode
  only (`advertised_capabilities` in
  [`mcp/handler.rs`](../../services/fq-runtime/crates/fq-runtime/src/mcp/handler.rs)),
  so the URL-mode removals do not reach it.

### Roots

factor-q derives its roots from the agent's sandbox and can announce
changes through `roots/list_changed` ([MCP guide](../guide/mcp.md#roots--advertising-the-workspace-scope)).

Against a modern server:

- a roots request arrives as an MRTR input request;
- there is no change notification, so a server learns about new roots only
  when it next asks.

The specification's migration path is to pass paths as tool parameters or
server configuration. factor-q already does the second for stdio servers,
through their declared arguments.

### Logging

factor-q folds `notifications/message` into tracing and onto the bus as an
`mcp.log` event.

Against a modern server:

- a server sends log messages only for requests that set
  `io.modelcontextprotocol/logLevel`;
- the messages arrive on that request's own response stream.

So `mcp.log` keeps working only if factor-q sets a level on its requests.
The deprecation's migration for stdio is `stderr`, which factor-q already
forwards to the daemon log
(<https://github.com/bricef/factor-q/issues/541>). Remote servers have no
`stderr`; for them the specification points to OpenTelemetry.

### Change notifications

The daemon drains each shared server's notifications and refreshes the
tool registry on `tools/list_changed`
([ADR-0020](../adrs/accepted/0020-mcp-notification-handling.md)). Against a
modern server:

- these notifications, and `resources/updated`, arrive only on a
  `subscriptions/listen` stream that the client opens and opts into;
- the drain loop has to open one per modern server.

In rmcp 3.9 this is explicit. On a modern connection the client opens the
stream with `Peer::listen` and a subscription filter. Notifications routed
to it are not delivered to the `on_*_list_changed` callbacks that
ADR-0020's drain is built on, so those callbacks only see legacy servers.

### Lifecycle and startup

- **Process per invocation.** Grant-bearing servers run as their own
  process per invocation, so that a request the server initiates is charged
  to the right invocation ([MCP guide](../guide/mcp.md#lifecycle)). Under
  MRTR every input request comes back on the response to one specific
  `tools/call`, so the request itself carries the attribution. Whether a
  modern grant-bearing server still needs its own process is an open
  question. Legacy servers still do.
- **Startup.** `startup_timeout_secs` bounds the `initialize` handshake. A
  modern server has no handshake, and a dual-era client's stdio probe is a
  `server/discover` call instead. The probe adds a step to starting a
  server. The specification lets its result be cached per server process.

### Streamable HTTP

Modern Streamable HTTP has no session id. It requires the `Mcp-Method` and
`Mcp-Name` headers.

It also drops resumability. If a response stream breaks, the client is told
to send the request again. For a `tools/call`, sending it again means
running the tool again, and the WAL refuses to do that silently. factor-q's
existing answer, reporting the call as ambiguous rather than re-running it,
probably stands. It is worth stating when the transport is migrated.

### Unaffected or new

- **Unchanged:**
  - progress (request-scoped, with tokens minted by rmcp);
  - cancellation;
  - pagination;
  - `includeContext`, which factor-q ignores and which is now deprecated.
- **Not used today:** tasks and ping. Neither appears in the MCP client.
- **Available later:** cache hints and a deterministic `tools/list` order
  could let the daemon cache tool lists. That is a follow-on, not part of
  the migration.

## Open questions

1. **Route A first, or straight to Route B?** See [Two routes](#two-routes).
   The landscape favours A now. It moves factor-q onto a maintained rmcp
   line without a protocol change.
2. **How are the new rmcp 3.x deprecations handled?** `LoggingLevel` is
   used where server logs are folded into tracing, and a new
   `#[allow(deprecated)]` is a new `.allow-baseline` entry. Compiling
   against 3.x will say how many there are.
3. **Under Route B, who drives MRTR?** rmcp's `call_tool` hides the legs
   inside one future but gives up factor-q's raw-request progress and
   cancellation handling. A loop over `call_tool_once` keeps them, but
   exposes each leg to the runner.

   *Answered while writing this review:* rmcp 3.9 offers both. It falls
   back to `initialize` for legacy servers in `Auto` mode, and `serve()`
   stays legacy.
4. Does factor-q keep sampling after the deprecation window? The suggested
   migration, "integrate directly with LLM provider APIs", moves the model
   call into the server. That gives up the budget, attribution and
   validation that ADR-0018 placed in the runner.
5. Can a crash between MRTR legs be resumed by re-sending the retry, or is
   it refused as an in-flight tool call is today?
6. Does a modern grant-bearing server still need its own process per
   invocation?

## Sources

- [Versioning](https://modelcontextprotocol.io/specification/versioning)
- [2026-07-28 changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)
- [2026-07-28 versioning and compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)
- [2026-07-28 multi round-trip requests](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr)
- [2026-07-28 deprecated features registry](https://modelcontextprotocol.io/specification/2026-07-28/deprecated)
- [2025-11-25 changelog](https://modelcontextprotocol.io/specification/2025-11-25/changelog)
- [2025-06-18 changelog](https://modelcontextprotocol.io/specification/2025-06-18/changelog)
- [2025-03-26 changelog](https://modelcontextprotocol.io/specification/2025-03-26/changelog)
- [2024-11-05 specification](https://modelcontextprotocol.io/specification/2024-11-05) and its
  [roots page](https://modelcontextprotocol.io/specification/2024-11-05/client/roots)
- [The 2026-07-28 specification (MCP blog)](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
- [rmcp changelog](https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/CHANGELOG.md)
  and [releases](https://github.com/modelcontextprotocol/rust-sdk/releases)
- rmcp crate source for every release, from
  [crates.io](https://crates.io/crates/rmcp/versions): `src/model.rs`
  (`ProtocolVersion`), `src/service/client.rs` (`ClientLifecycleMode`, the
  MRTR helpers), `src/model/mrtr.rs` and `src/model/capabilities.rs` in
  3.9.0
- [MCP Remote Server Census, November 2026 (DigitalApplied)](https://www.digitalapplied.com/blog/mcp-remote-server-health-census-november-2026)
- [State of MCP Server Development: October 2026 (LocalCan)](https://www.localcan.com/blog/state-of-mcp-server-development)
- [MCP Servers on the Internet (Censys)](https://censys.com/blog/mcp-servers-on-the-internet/)
- [What clients implement today (community survey, 2026-10-17)](https://github.com/allisonmahmood/patchy-cloud/issues/732)
- [SEP-2577 discussion](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2577)
- [Python reference servers to SDK v2](https://github.com/modelcontextprotocol/servers/issues/4851)
- [The Register: MCP prepares to break with its stateful past](https://www.theregister.com/devops/2026/07/23/model-context-protocol-prepares-to-break-with-its-stateful-past/5276722)
- [Hacker News: the 2026-07-28 specification](https://news.ycombinator.com/item?id=49088058)
