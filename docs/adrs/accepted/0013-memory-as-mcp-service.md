# ADR-0013: Memory as an MCP Service

## Status
Accepted

## Context
Agents need persistent memory that outlasts a single invocation — the ability to store what they've learned and retrieve relevant context in future runs. The architecture document identifies three memory layers: working memory (in-context), long-term per-agent memory, and collective memory across agents.

The question is whether memory is a core runtime concern built into factor-q, or an external capability that agents access as a tool.

## Decision: Memory is an independent MCP service

Persistent memory (storage, retrieval, search) is implemented as one or more MCP servers that agents access through standard tool calls. factor-q's core runtime does not own memory storage or retrieval — it provides MCP client support, and agents declare which memory services they need in their definitions.

### Rationale

**Memory is a tool, not a runtime feature.** An agent decides when to store and retrieve memories through the same tool-calling mechanism it uses for file operations or shell commands. This keeps the runtime focused on orchestration.

**Nothing by default.** Agents that need memory declare the MCP tool in their definition. Agents that don't need memory don't get it. This aligns with the sandboxing principle.

**Independent evolution.** Memory backends can be developed, versioned, and deployed without touching the core runtime. A simple KV memory service can ship first; RAG-backed retrieval, vector search, or collective memory services can follow as separate MCP servers behind the same interface.

**Reusable beyond factor-q.** Any MCP client can use the same memory service — it's not locked to factor-q.

**Forcing function for MCP support.** Making memory depend on MCP ensures that MCP client integration is built and proven early, which is needed for the broader extension model.

### What stays in the runtime

**Working memory / context window management** — compaction, summarisation, and context window budgeting are executor concerns. The executor manages what fits in the LLM's context window; the MCP memory service manages what persists across invocations.

### Scope implications

- MCP client support is required before persistent memory is available
- Memory and MCP client support are phase 2 scope (phase 1 is a stateless walking skeleton)
- Different memory backends (KV, RAG, collective) can be different MCP servers with the same tool interface

## Consequences
- factor-q must implement MCP client support (discover, connect to, and invoke tools from MCP servers)
- Agent definitions can reference MCP servers in their tool list
- Memory tool calls (store, retrieve, search) are standard tool invocations from the agent's perspective
- Multiple memory services can coexist — per-agent memory, shared memory, domain-specific memory
- The runtime does not need a memory subsystem, storage schema, or retrieval logic in its core
