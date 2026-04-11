# Phase 2: MCP, Memory, and Skills

## Goal

Extend the phase 1 walking skeleton with MCP client support, persistent agent memory, and a skill registry. Phase 2 makes agents stateful across invocations, proves the extension model via MCP, and enables reusable domain knowledge via skills.

A shared vector database underpins both memory search and skill discovery, establishing a composable semantic search primitive that future services can build on.

## Prerequisites

Phase 1 complete — a working single-agent runtime with event-driven execution, cost controls, and CLI inspection.

## Scope

### 1. MCP client support
factor-q becomes an MCP client, able to discover, connect to, and invoke tools from external MCP servers.

- Connect to MCP servers via stdio and SSE transports
- Discover available tools from connected MCP servers
- Route agent tool calls to the appropriate MCP server
- MCP servers declared in agent definitions alongside built-in tools
- MCP tool calls emit events to NATS like any other tool call
- Cost and sandbox controls apply to MCP tool invocations

### 2. Vector database and embedding infrastructure
Shared semantic search primitive used by both the memory and skill registry services.

- Embedding generation for text content (skill descriptions, memory entries)
- Vector storage with collection-based separation (skills collection, memory collection per namespace)
- Similarity search with filtering (namespace, metadata, tags)
- Choice of vector DB deferred to implementation (candidates: Qdrant, LanceDB, SQLite with vector extensions)
- Designed as a shared library/service consumed by other MCP services, not a user-facing component

### 3. Memory MCP service
A standalone MCP server providing persistent memory for agents, backed by the shared vector database.

- `memory.store` — persist a memory with optional metadata and tags; auto-embedded for future search
- `memory.retrieve` — retrieve a specific memory by key
- `memory.search` — semantic search over memories using embedding similarity
- `memory.list` — list memories with filtering
- `memory.delete` — remove a memory
- Scoped by agent ID — each agent has its own memory namespace (vector collection) by default
- Shared namespaces available for cross-agent memory (opt-in)

### 4. Skill registry MCP service
A standalone MCP server providing skill discovery and activation, backed by the shared vector database.

- `skill.search(query, [namespace])` — semantic search over skill descriptions the agent has access to
- `skill.list([namespace])` — browse available namespaces and skills
- `skill.activate(name)` — load full SKILL.md instructions, returned as tool output for context injection
- `skill.deactivate(name)` — signal to unload a skill and reclaim context space
- Skills embedded at registration time; re-embedded when SKILL.md changes
- Namespace-based access control enforced per-agent — search results filtered by the agent's declared access patterns
- Skills follow the AgentSkills specification (ADR-0014)
- CLI commands: `fq skill list`, `fq skill validate <path>`, `fq skill search <query>`

### 5. Context window management
Working memory management in the executor (distinct from persistent memory).

- Track context window usage per agent invocation
- Compaction strategy when context approaches the model's limit
- Summarisation of conversation history to free context space
- Integration with persistent memory — agent can store important context before compaction
- Integration with skill deactivation — agent can drop skills it no longer needs

### 6. Agent definition extensions
Extend the Markdown agent definition format to support MCP servers and skill access.

```yaml
---
name: researcher
model: claude-haiku
tools:
  - read
  - web_search
skills:
  access:
    - "research.*"
    - "writing.academic"
mcp:
  - server: memory
    transport: stdio
    command: fq-memory-server
    namespace: researcher
---
```

## Out of scope

- Multi-agent graph execution
- Visual UI
- Skill authoring by agents (phase 3+ — continuous learning)

## Success criteria

1. An agent can store memories during execution that persist across invocations
2. An agent can semantically search memories from previous runs ("find what I learned about X")
3. MCP servers can be declared in agent definitions and their tools are available to the agent
4. MCP tool calls appear in the event stream like built-in tool calls
5. Context window compaction allows long-running agent invocations without exceeding model limits
6. An agent can search the skill registry by natural language query and discover relevant skills
7. An agent can activate a discovered skill, receiving its full instructions in context
8. Skill search results respect the agent's namespace access control
9. Memory search and skill search use the same underlying vector infrastructure
