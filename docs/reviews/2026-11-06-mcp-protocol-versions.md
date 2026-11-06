# MCP protocol versions — 2026-11-06

Taken against `main` @ `40417719` (2026-11-06). This is a survey of every
published revision of the Model Context Protocol: what each one added,
changed and removed, and what the current revision means for factor-q's
MCP client. It was written as input to moving factor-q off rmcp 1.x, which
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
  `2025-11-25`.
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

## The rmcp line

- **rmcp 1.8** is what factor-q builds today; the manifest asks for `1.7`.
  `ProtocolVersion::LATEST` is `V_2025_11_25`, and `KNOWN_VERSIONS` already
  includes `2026-07-28`. Both were read from the crate source.
- **rmcp 2.0.0** (2026-06-27) aligned its model types with `2025-11-25`,
  deprecated the roots, sampling and logging types, and relaxed the type of
  `structuredContent`.
- **The rmcp 3.0 line** began with betas on 2026-07-23. It added MRTR
  (SEP-2322), discovery and modern lifecycle modes (SEP-2575), the tasks
  extension (SEP-2663), cache hints (SEP-2549) and the standard HTTP
  headers (SEP-2243).
  - The server option `stateful_mode` became `legacy_session_mode`. The
    changelog says it only affects legacy versions, and that `2026-07-28`
    is always served statelessly.
  - The MCP launch post listed the Rust SDK's support for `2026-07-28` as
    beta on release day.
- **Later 3.x releases** carry entries such as "keep initialize on legacy
  protocol versions" (3.2.0) and "update LATEST and add
  LATEST_WITH_INITIALIZE" (3.5.0). These suggest rmcp 3.x is dual-era on
  the client side. **This has not been checked in the source.**
- **rmcp 3.9.0** shipped on 2026-11-05.
  <https://github.com/bricef/factor-q/pull/927> bumps to 3.8.0 and is red,
  failing Code quality, Rust CI, Runtime CI, Dashboard E2E and Container
  image: the tree does not compile against it.

## What changes for factor-q

The sections below cover modern servers only. A legacy server keeps today's
behaviour exactly, and a dual-era client has to keep both paths.

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
  `max_discovery_pages` caps a cursor that never ends.
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

rmcp 3.9's changelog has an entry "point on_*_list_changed callbacks to
Peer::listen". It is not yet clear whether that is a behaviour change or a
documentation pointer.

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

1. How does the rmcp 3.9 client surface MRTR? Two possibilities:
   - it keeps the `create_message` / `create_elicitation` / `list_roots`
     callbacks on `ClientHandler` and re-sends the request itself;
   - it hands the `input_required` result back to the caller.

   The answer decides how much of the runner changes. It also confirms
   whether the rmcp 3.x client really falls back to `initialize` for
   legacy servers.
2. Does factor-q keep sampling after the deprecation window? The suggested
   migration, "integrate directly with LLM provider APIs", moves the model
   call into the server. That gives up the budget, attribution and
   validation that ADR-0018 placed in the runner.
3. Can a crash between MRTR legs be resumed by re-sending the retry, or is
   it refused as an in-flight tool call is today?
4. Does a modern grant-bearing server still need its own process per
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
