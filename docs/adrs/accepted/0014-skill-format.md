# ADR-0014: Skill Format and Discovery

## Status
Accepted

## Context
Skills are reusable bundles of prompt instructions and domain knowledge that shape how an agent approaches work. factor-q needs a skill format that is human-readable, composable, and manageable at scale across a swarm of agents.

At scale (hundreds to thousands of skills), loading skill metadata into agent prompts is not viable — even 100 tokens per skill becomes prohibitive. Skill discovery must be a runtime capability, not prompt content.

## Decision

### Skill format: AgentSkills specification

Skills follow the [AgentSkills specification](https://agentskills.io/specification).

A skill is a directory containing a `SKILL.md` file with YAML frontmatter and Markdown body:

```
code-reviewer/
├── SKILL.md              # Required: metadata + instructions
├── scripts/              # Optional: executable code
├── references/           # Optional: detailed documentation
└── assets/               # Optional: templates, resources
```

Required frontmatter fields: `name` and `description`. Optional: `license`, `compatibility`, `metadata`, `allowed-tools`.

### Skill discovery: tool-based, not prompt-based

Skill discovery is a tool the agent calls, not content injected into the prompt. The agent's system prompt includes only a brief instruction that a skill registry is available (~30 tokens), regardless of registry size.

The skill registry is an MCP service that agents query through standard tool calls:

| Tool | Purpose |
|---|---|
| `skill.search(query, [namespace])` | Semantic search over skills the agent has access to |
| `skill.list([namespace])` | Browse available namespaces and skills |
| `skill.activate(name)` | Load full skill instructions into agent context |
| `skill.deactivate(name)` | Unload a skill to free context space |

### Discovery via embedding-based semantic search

Skill descriptions are embedded at registration time and stored in a vector database. Search queries are embedded and matched by similarity. This provides natural language discovery ("how do I handle database migrations safely?") rather than requiring agents to know exact skill names or tags.

The vector database is a shared primitive with the memory MCP service (ADR-0013) — the same underlying infrastructure with different collections. This avoids duplicating embedding and search infrastructure.

### Namespace-based access control

Agent definitions declare which skill namespaces they can access:

```yaml
---
name: senior-engineer
model: claude-sonnet
skills:
  access:
    - "engineering.*"
    - "security.*"
    - "writing.technical-docs"
---
```

```yaml
---
name: ops-responder
model: claude-sonnet
skills:
  access:
    - "ops.*"
    - "infrastructure.*"
---
```

- `"*"` grants access to the full registry
- No `skills` field means no skill access
- The registry enforces access filters — a search from an agent with `engineering.*` access never returns `ops.*` skills, even if they match the query

### Progressive disclosure

Even after discovery, skills are loaded progressively:

1. **Search results** (~50 tokens per result) — name + description for matched skills
2. **Activation** (< 5000 tokens recommended) — full `SKILL.md` instructions loaded into context
3. **Resources** (on demand) — files in `scripts/`, `references/`, `assets/` loaded only when the agent references them
4. **Deactivation** — agent can unload a skill to reclaim context space

### Rationale

- **Zero per-skill prompt cost** — scales to thousands of skills without context window impact
- **Semantic search** — agents describe what they need in natural language rather than memorising skill names
- **Shared infrastructure** — embedding and vector search are the same primitive used by the memory service, amortising the infrastructure cost
- **Namespace access control** — coarse-grained permissions that are easy to reason about, without listing individual skills
- **Open standard** — AgentSkills format is portable across systems
- **Active discovery** — agents search when they need a skill, not passively loaded with everything

## Consequences
- The skill registry is an MCP service with embedding-based search
- Skills are embedded at registration time; the registry maintains a vector index
- The vector database is shared infrastructure between the skill registry and memory service
- Agent definitions use namespace patterns for access control, not individual skill names
- Agent prompts carry ~30 tokens of skill registry instructions regardless of registry size
- Agents actively discover and activate skills at runtime through tool calls
- Skills can be activated and deactivated during an invocation to manage context budget
