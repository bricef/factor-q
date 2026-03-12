# Crush Architecture: Prompt Construction & Agent Composition

## Prompt Construction Pipeline

### 1. Template System

System prompts are Go templates (`text/template`) embedded at compile time via `//go:embed`:

| Template | File | Purpose |
|---|---|---|
| `coder` | `internal/agent/templates/coder.md.tpl` | Main interactive coding agent |
| `task` | `internal/agent/templates/task.md.tpl` | Read-only sub-agent for search/context |
| `initialize` | `internal/agent/templates/initialize.md.tpl` | Generates AGENTS.md for a project |
| `agentic_fetch` | `internal/agent/templates/agentic_fetch_prompt.md.tpl` | Web content analysis sub-agent |

Factory functions in `internal/agent/prompts.go:20-42` wrap these into `prompt.Prompt` objects.

### 2. Template Data (`PromptDat`)

Defined at `internal/agent/prompt/prompt.go:29-40`, this struct is injected into every template at render time:

| Field | Source |
|---|---|
| `Provider` / `Model` | From config's selected model |
| `Config` | Full application `config.Config` |
| `WorkingDir` | CWD or override |
| `IsGitRepo` | Checks for `.git` directory |
| `Platform` | `runtime.GOOS` |
| `Date` | Current date |
| `GitStatus` | Branch name + `git status --short` + last 3 commits |
| `ContextFiles` | Contents of AGENTS.md, CRUSH.md, CLAUDE.md, etc. |
| `AvailSkillXML` | XML listing of discovered `SKILL.md` files |

The `Build()` method at `prompt.go:79-94` calls `promptData()` (lines 151-203) to assemble all this, then executes the template.

### 3. Context Files

Default context paths are defined in `internal/config/config.go:29-46` and include: `.github/copilot-instructions.md`, `.cursorrules`, `.cursor/rules/`, `CLAUDE.md`, `CRUSH.md`, `AGENTS.md`, and their variants. These are merged with user-specified paths from `crush.json` at `internal/config/load.go:406-408`.

Each file is read and injected into the coder template wrapped in XML:

```xml
<memory>
  <file path="AGENTS.md">...content...</file>
</memory>
```

### 4. Tool Descriptions

Tools are **not** injected into the system prompt text. Instead, each tool carries its own description (embedded from `.md` files or rendered from `.tpl` templates) and is registered as a `fantasy.AgentTool`. The `fantasy` library serializes tool schemas into the provider's function-calling format.

Example: `internal/agent/tools/edit.go:51-71` embeds `edit.md` as the tool description. The bash tool at `internal/agent/tools/bash.go:54-60` uses a Go template for its description, injecting banned commands and config.

### 5. Final Message Assembly

The conversation sent to the LLM provider has this structure:

```
1. [System Message]  provider.SystemPromptPrefix (if configured)
2. [System Message]  Rendered coder.md.tpl + MCP server instructions
3. [User Message]    <system_reminder> about todo list state (injected)
4. [History...]      Previous messages (user/assistant/tool results)
                     (with Anthropic cache-control on last system msg + last 2 msgs)
5. [User Message]    Current prompt (with text attachments in <file> tags)
6. [File Parts]      Binary attachments (images, etc.)
   + [Tool Schemas]  Serialized by fantasy per provider format
```

Key code: `sessionAgent.Run()` at `agent.go:155-580`, `preparePrompt()` at `agent.go:730-766`, and the `PrepareStep` callback at `agent.go:261-313`.

---

## Agent Composition Dataflow

### Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│  crush.json files (global → ancestor dirs → workspace)             │
│  Deep-merged via jsons.Merge, workspace has highest priority       │
└──────────────────────────────┬──────────────────────────────────────┘
                               │
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│  ConfigStore  (internal/config/)                                     │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────┐  ┌────────────┐ │
│  │ Providers()  │  │SelectedModel │  │ Agent defs │  │ MCP/LSP    │ │
│  │ catwalk sync │  │ large/small  │  │ coder/task │  │ configs    │ │
│  └──────┬──────┘  └──────┬───────┘  └─────┬──────┘  └─────┬──────┘ │
└─────────┼────────────────┼─────────────────┼───────────────┼────────┘
          │                │                 │               │
          ▼                ▼                 ▼               ▼
┌──────────────────────────────────────────────────────────────────────┐
│  App  (internal/app/app.go:78-143)                                   │
│  Wires: DB, Sessions, Messages, Permissions, FileTracker, LSP,       │
│         History, PubSub brokers, Telemetry                           │
│                          │                                           │
│                          ▼                                           │
│  ┌───────────────────────────────────────────────────────────────┐   │
│  │  Coordinator  (internal/agent/coordinator.go:76-90)           │   │
│  │                                                               │   │
│  │  buildAgentModels() ──► fantasy.Provider ──► LanguageModel    │   │
│  │  buildAgent()       ──► SessionAgent (coder)                  │   │
│  │  buildTools()       ──► []fantasy.AgentTool (filtered)        │   │
│  │  prompt.Build()     ──► rendered system prompt string         │   │
│  │                                                               │   │
│  │  ┌─────────────────────────────────────────────────────────┐  │   │
│  │  │  SessionAgent  (internal/agent/agent.go:103-119)        │  │   │
│  │  │                                                         │  │   │
│  │  │  largeModel / smallModel  (thread-safe)                 │  │   │
│  │  │  systemPrompt / tools     (thread-safe)                 │  │   │
│  │  │  messageQueue             (per-session queueing)        │  │   │
│  │  │  activeRequests           (cancel tracking)             │  │   │
│  │  └─────────────────────────────────────────────────────────┘  │   │
│  └───────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────┘
```

### The Run Loop (User Message → LLM Response)

```
User types message in TUI
         │
         ▼
coordinator.Run()                          [coordinator.go:136]
  ├── Wait for async init (readyWg)
  ├── UpdateModels() — refresh from latest config
  ├── Merge provider options (catwalk + config overrides)
  └── sessionAgent.Run(SessionAgentCall)
         │
         ▼
sessionAgent.Run()                         [agent.go:155]
  ├── If session busy → queue prompt, return nil
  ├── Snapshot tools, model, systemPrompt (thread-safe copies)
  ├── Append MCP server instructions to system prompt
  ├── fantasy.NewAgent(model, systemPrompt, tools)
  ├── Load session + message history from SQLite
  ├── Spawn title generation goroutine (small model, concurrent)
  ├── Persist user message → pubsub event → TUI renders
  ├── preparePrompt() → convert DB messages to fantasy.Message[]
  │
  └── fantasy.Agent.Stream()               [charm.land/fantasy]
         │
         │  ┌─── AGENT LOOP (managed by fantasy) ───────────────┐
         │  │                                                    │
         │  │  PrepareStep callback:                             │
         │  │    • Drain queued messages                         │
         │  │    • Provider media workarounds                    │
         │  │    • Anthropic cache-control headers               │
         │  │    • Prepend systemPromptPrefix                    │
         │  │    • Create assistant message in DB                │
         │  │                                                    │
         │  │  Send to LLM provider (streaming HTTP)             │
         │  │                                                    │
         │  │  Stream callbacks:                                 │
         │  │    OnReasoningDelta → update DB → pubsub → TUI     │
         │  │    OnTextDelta     → update DB → pubsub → TUI      │
         │  │    OnToolCall      → update DB → pubsub → TUI      │
         │  │                                                    │
         │  │  If tool calls:                                     │
         │  │    tool.Run(ctx, toolCall)                          │
         │  │      ├── Check permissions (pubsub → TUI dialog)   │
         │  │      ├── Execute (bash/edit/MCP/etc.)              │
         │  │      └── Return ToolResponse                       │
         │  │    OnToolResult → persist → pubsub → TUI           │
         │  │                                                    │
         │  │  OnStepFinish → update session tokens/cost         │
         │  │                                                    │
         │  │  StopWhen:                                          │
         │  │    • Context window exhaustion → auto-summarize     │
         │  │    • Loop detection (>5 repeated tool calls)        │
         │  │    • LLM stops calling tools                        │
         │  │                                                    │
         │  └────────────────────────────────────────────────────┘
         │
         ▼
  ├── Publish notify.AgentFinished → pubsub → TUI
  ├── Auto-summarize if context window near limit
  └── Drain messageQueue → recursive Run() for next prompt
```

### Sub-Agent Architecture

When the coder agent uses the "agent" (task) tool, a **child agent** is spawned:

- Uses the `task.md.tpl` template (shorter, read-only focused prompt)
- Gets only read-only tools: `glob`, `grep`, `ls`, `sourcegraph`, `view`
- No MCP or LSP tools
- Creates a child session (`CreateTaskSession`) with `ParentSessionID`
- Costs roll up to the parent session via `updateParentSessionCost()`

Similarly, `agentic_fetch` spawns a sub-agent with `web_fetch`, `web_search`, and a few read-only tools, running on the small model.

### Pub/Sub Event System

All components communicate through `internal/pubsub/broker.go`:

```
Session events ──┐
Message events ──┤
Permission req ──┤──► app.events channel ──► program.Send() ──► Bubble Tea TUI
Agent finished ──┤
MCP state ───────┤
LSP state ───────┘
```

Each service (sessions, messages, permissions) embeds a `pubsub.Broker[T]` and publishes `Created/Updated/Deleted` events. The app's `setupEvents()` at `app.go:423-440` subscribes to all brokers and fans events into a unified channel that the TUI consumes.

### Provider Resolution

Providers are resolved at startup via a multi-layer fallback:

```
catwalk.charm.sh API (remote) → cached providers.json → embedded defaults
```

Then merged with user overrides from `crush.json` (`configureProviders` at `load.go:153-368`). Custom providers require `Type`, `BaseURL`, `Models`, and `APIKey`. The coordinator builds the appropriate `fantasy.Provider` via a provider-type switch in `buildProvider()` at `coordinator.go:797-845`.
