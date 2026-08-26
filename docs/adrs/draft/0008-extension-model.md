# ADR-0008: Extension and Plugin Model

## Status

Draft

Implementation: pending as a decision, but reality has answered most of it
without this draft moving. Option C is decided and built — MCP is the
extension point for tools (ADR-0013/0017/0018, `mcp:` is first-class agent
frontmatter, [the MCP guide](../../guide/mcp.md) is the live reference).
Option A is what agent definitions actually use: `.md` files in well-known
directories, hot-reloaded through the daemon's registry. Options B and D
have no trace in the tree. What is genuinely open is skills — and they are
more open than this draft implies: [ADR-0019](../accepted/0019-skill-format.md)
fixed a format, and none of it is built. Packaging, versioning and scoping
are the residue.

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
- The extension model is tightly coupled to the agent definition format
  ([ADR-0005](../accepted/0005-agent-definition-format.md)) and the isolation
  model ([ADR-0010](../accepted/0010-agent-execution-isolation.md), whose
  unit of isolation is superseded by
  [ADR-0028](../accepted/0028-tool-scoped-isolation-and-workspace.md)) —
  originally written as ADR-0009, which is *Technology Choices*
