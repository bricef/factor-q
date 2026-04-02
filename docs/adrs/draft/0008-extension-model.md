# ADR-0008: Extension and Plugin Model

## Status
Draft

## Context
Power users need to extend factor-q with custom tools, skills (prompt + tool bundles), agent types, and trigger sources. MCP (Model Context Protocol) provides one integration path for external tool providers, but the broader question of how users package, distribute, and version extensions is open.

## Options

### Option A: File convention
Extensions are files in well-known directories (e.g. `tools/`, `skills/`, `agents/`). Discovery is by file presence. Simple, version-control friendly, no registry needed. Limited metadata and dependency management.

### Option B: Plugin API
Extensions implement a defined interface and are loaded at runtime. More powerful, supports lifecycle hooks and complex behaviour. Ties extensions to the host language and requires a stable API contract.

### Option C: MCP as the universal extension point
All custom tools are MCP servers. factor-q is an MCP client. Leverages an emerging standard. But MCP may not cover all extension types (custom triggers, skills, agent types).

### Option D: Package registry
Extensions are versioned packages in a registry (like npm, crates.io). Enables sharing, discovery, and dependency resolution. Significant infrastructure to build and maintain.

## Decision
Not yet taken.

## Considerations
- MCP is already identified as an integration path — how much weight does it carry vs a native extension model?
- Skills (prompt + tool bundles) are a different shape from tools (executable capabilities) — do they share an extension mechanism?
- Extensions must be scoped — an extension installed for one agent graph shouldn't affect another
- Versioning matters — agent behaviour should be reproducible, which means pinning extension versions
- The extension model is tightly coupled to the agent definition format (ADR-0005) and the isolation model (ADR-0009)
