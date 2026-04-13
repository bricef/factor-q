# Plan: MCP Client Support (Phase 2, Workstream 1)

## Context

Phase 1 delivered a working single-agent runtime with built-in tools (shell, file_read, file_write), event-driven execution via NATS, and cost controls. Phase 2 begins with MCP client support — enabling agents to use tools provided by external MCP servers. This is the foundation for the memory and skill registry services (phase 2 workstreams 3 and 4), which are themselves MCP servers.

The goal: an agent can declare MCP servers in its definition, the runtime starts them, discovers their tools, and the agent can call those tools during execution. Everything flows through the existing `Tool` trait, `ToolRegistry`, executor, and event system unchanged.

## Approach

New module `mcp.rs` in `fq-runtime` with two types: `McpTool` (wraps an MCP server tool into the `Tool` trait) and `McpClientManager` (manages server process lifecycle). Agent definitions get a new `mcp:` frontmatter field. Both `fq run` and `fq trigger` start MCP servers at init and register their tools alongside built-ins.

Uses the `rmcp` crate (official Rust MCP SDK) for the protocol layer.

## Implementation Steps

### Step 1: Add `rmcp` dependency

**File:** `crates/fq-runtime/Cargo.toml`

```toml
rmcp = { version = "1.4", default-features = false, features = ["client", "transport-child-process"] }
```

### Step 2: Create `crates/fq-runtime/src/mcp.rs`

**`McpServerConfig`** — plain data struct for server declarations:
```rust
pub struct McpServerConfig {
    pub name: String,           // human-readable, for logging
    pub command: String,        // executable to spawn
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}
```

**`McpTool`** — adapts one MCP server tool to `fq_tools::Tool`:
- Fields: `tool_name: String`, `tool_description: String`, `tool_input_schema: Value`, `client: Arc<RunningService<RoleClient, ()>>`
- `name()` / `description()` / `parameters_schema()` return the stored metadata
- `execute()` calls `client.call_tool(CallToolRequestParams)`, extracts text content from the response, maps `is_error`, returns `ToolResult`. On RPC failure, returns `ToolError::ExecutionFailed`.
- Ignores `ToolContext` (sandbox) — MCP servers manage their own isolation

**`McpClientManager`** — owns server processes:
- `start_server(config) -> Result<Vec<Arc<dyn Tool>>, McpError>` — spawns child process via `TokioChildProcess`, performs MCP handshake via `().serve(transport)`, calls `list_all_tools()`, wraps each in `McpTool`, retains the client handle
- `shutdown()` — calls `client.cancel()` on each, best-effort
- Deduplicates by `(command, args)` tuple — same server declared by multiple agents is only started once

**`McpError`** — `ServerStart`, `ToolDiscovery`, `ToolCall` variants.

**Export from `lib.rs`:** `pub mod mcp;`

### Step 3: Extend agent definition with `mcp:` field

**File:** `crates/fq-runtime/src/agent/definition.rs`

Add to `Frontmatter`:
```rust
#[serde(default)]
mcp: Vec<McpFrontmatter>,
```

New struct:
```rust
#[derive(Debug, Deserialize)]
struct McpFrontmatter {
    server: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}
```

Map to `McpServerDeclaration` in `parse_agent()` and pass to builder.

### Step 4: Add `mcp_servers` to `Agent` and `AgentBuilder`

**File:** `crates/fq-runtime/src/agent.rs`

New public type:
```rust
#[derive(Debug, Clone)]
pub struct McpServerDeclaration {
    pub server: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}
```

Add field `mcp_servers: Vec<McpServerDeclaration>` to `Agent`, `AgentBuilder`. Add builder setter `.mcp_servers(vec)`, accessor `agent.mcp_servers()`. Wire into `build()`.

### Step 5: Wire MCP into `trigger_agent()` (CLI)

**File:** `crates/fq-cli/src/main.rs`, `trigger_agent()` function (~line 424)

Replace `let tools = Arc::new(ToolRegistry::with_builtins());` with:
1. `let mut tools = ToolRegistry::with_builtins();`
2. `let mut mcp_manager = McpClientManager::new();`
3. For each MCP server in the agent's declarations, call `mcp_manager.start_server(config)`, register returned tools
4. `let tools = Arc::new(tools);`
5. After executor completes, `mcp_manager.shutdown().await`

MCP server start failures are logged and non-fatal.

### Step 6: Wire MCP into `run_daemon()` (CLI)

**File:** `crates/fq-cli/src/main.rs`, `run_daemon()` function (~line 793)

Same pattern as step 5, but iterating over all agents in the registry. The `McpClientManager` is kept alive for the daemon's lifetime and shut down during the existing shutdown sequence (after dispatcher/projector stop, before `system.shutdown` event).

### Step 7: Integration test with `server-everything`

**File:** `crates/fq-runtime/tests/mcp_integration.rs`

- Start `npx @modelcontextprotocol/server-everything` via `McpClientManager`
- Verify tool discovery finds `echo` (and others)
- Call `echo` tool through the `Tool` trait, verify output
- Skip if `npx` not available
- Add parser test for `mcp:` frontmatter round-trip in `definition.rs`

### Step 8: Example agent

**File:** `agents/examples/mcp-echo.md`

```yaml
---
name: mcp-echo
model: claude-haiku-4-5
tools:
  - echo
mcp:
  - server: everything
    command: npx
    args: ["@modelcontextprotocol/server-everything"]
budget: 0.10
---
You are a test agent. Echo back whatever the user says using the echo tool.
```

## Critical files

| File | Change |
|---|---|
| `crates/fq-runtime/Cargo.toml` | Add `rmcp` dependency |
| `crates/fq-runtime/src/lib.rs` | Add `pub mod mcp;` |
| `crates/fq-runtime/src/mcp.rs` | **New** — McpTool, McpClientManager, McpError |
| `crates/fq-runtime/src/agent.rs` | Add McpServerDeclaration, extend Agent/AgentBuilder |
| `crates/fq-runtime/src/agent/definition.rs` | Add McpFrontmatter, extend parse_agent() |
| `crates/fq-cli/src/main.rs` | Wire MCP startup into trigger_agent() and run_daemon() |
| `crates/fq-runtime/tests/mcp_integration.rs` | **New** — integration test with server-everything |
| `agents/examples/mcp-echo.md` | **New** — example agent using MCP |

## Verification

1. `cargo build` — compiles with rmcp dependency
2. `cargo test` — unit tests pass (definition parsing, builder)
3. `cargo test --test mcp_integration` — echo tool works via server-everything (requires npx)
4. `fq trigger mcp-echo "hello world"` — agent calls echo tool, output visible in events
5. `fq events tail fq.agent.>` — tool.call and tool.result events appear for the MCP tool call
