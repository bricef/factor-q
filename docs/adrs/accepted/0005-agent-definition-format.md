# ADR-0005: Agent Definition Format

## Status
Accepted

## Context
The agent definition is the primary artefact users author. It must be:
- **LLM-writable** — agents and supervisors must be able to create and modify agent definitions programmatically
- **Human-readable** — a user can open it, understand it, and edit it without specialist knowledge
- **Hot-loadable** — changes take effect without recompiling the system

This rules out formats that require compilation (pure Rust code) or that are awkward for LLMs to produce (complex DSLs). It also rules out formats that separate the system prompt from the configuration, since the prompt is the most frequently edited part of an agent definition.

## Decision: Markdown with YAML frontmatter

Agent definitions are Markdown files where:
- **YAML frontmatter** contains structured configuration — model, tools, sandbox, budget, triggers, and metadata. This is validateable against a JSON Schema.
- **Markdown body** is the system prompt — freeform, expressive, supporting full Markdown formatting. No escaping, no indirection.

### Example

```markdown
---
name: researcher
model: claude-haiku
budget: 0.50
trigger: tasks.research.*
tools:
  - read
  - web_search
  - grep
sandbox:
  fs_read:
    - /project/docs
  network:
    - "*.api.internal"
---

You are a research agent. Your role is to investigate topics
assigned to you and produce structured summaries.

## Guidelines

- Always cite sources
- Prefer primary sources over summaries
- Flag areas of uncertainty explicitly

## Output Format

Produce a structured report with sections: Summary,
Key Findings, Sources, and Confidence Assessment.
```

### Internal representation

The runtime parses Markdown agent definitions into a Rust `Agent` struct via a builder pattern. The builder is the type-safe internal representation with compile-time validation. The Markdown file is the authoring and storage surface.

```rust
let agent = Agent::build("researcher")
    .model(Claude::Haiku)
    .prompt(parsed_markdown_body)
    .tools([Read, WebSearch, Grep])
    .sandbox(Sandbox::new()
        .fs_read("/project/docs")
        .network_allow("*.api.internal"))
    .budget(usd(0.50))
    .trigger(Subject("tasks.research.*"))
    .build();
```

### Rationale

**Markdown body as system prompt** — the most frequently edited part of an agent definition is the prompt. Making the document body the prompt eliminates indirection. The prompt is what you see when you open the file.

**YAML frontmatter for structure** — YAML is widely understood, produces clean diffs in version control, and is natively validateable against JSON Schema. LLMs generate valid YAML reliably.

**LLM-native format** — Markdown is the format LLMs produce most naturally. An agent can write a complete agent definition as a Markdown file without special serialisation logic.

**Ephemeral agent creation** — a supervisor agent can write a Markdown file to a staging directory, the runtime picks it up and instantiates the agent. This enables dynamic agent construction for single-use tasks without any compilation step.

**Self-improvement** — an agent can read its own definition file, modify the prompt or parameters based on performance review, and write a new version. The event bus captures what changed and why.

**Version control** — Markdown files diff cleanly in git. Prompt changes, tool additions, and sandbox modifications are all visible in pull requests.

**Templating** — the Markdown body can support template variables (e.g. `{{task.description}}`, `{{context.files}}`) injected at invocation time, enabling parameterised agents.

### Tradeoffs accepted

- **No compile-time validation of agent definitions** — the Markdown format is validated at load time, not compile time. Invalid definitions are caught when the runtime parses them, not when they're authored. JSON Schema validation and runtime error messages must be clear enough to compensate.
- **YAML's quirks** — YAML has well-known pitfalls (the Norway problem, implicit type coercion). Strict YAML parsing and schema validation mitigate this.
- **Two representations** — the Markdown file and the Rust builder are separate representations of the same concept. They must stay in sync. The builder is the source of truth for what fields exist; the Markdown parser maps into it.

## Consequences
- Agent definitions are stored as `.md` files in well-known directories
- The runtime watches for file changes and hot-reloads agent definitions
- A JSON Schema is published for the frontmatter, enabling editor validation and autocomplete
- The Rust builder pattern remains the internal API for constructing agents programmatically
- Graph and workflow definitions are a separate concern (see ADR for execution graph format)
