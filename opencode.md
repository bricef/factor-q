# OpenCode - Architecture Analysis

This is **OpenCode**, an open-source AI coding agent (similar to Claude Code) built by Anomaly Co. It's a TypeScript monorepo using Bun, with the core engine in `packages/opencode/src/`.

---

## Prompt Construction Pipeline

Prompts are assembled in layers, each adding context before the final LLM call:

### Layer 1: Provider-Specific Base Prompt
**`src/session/system.ts:22-30`** routes models to tailored system prompts:

| Model | Prompt File | Style |
|---|---|---|
| Claude | `prompt/anthropic.txt` | TodoWrite task management, few-shot examples |
| GPT-4o/o1/o3 | `prompt/beast.txt` | Autonomous "agentic" style, web research mandate |
| Gemini | `prompt/gemini.txt` | Detailed workflows with many few-shot examples |
| GPT-5/Codex | `prompt/codex_header.txt` | Editing constraints, git hygiene |
| Trinity | `prompt/trinity.txt` | Single-tool-per-message policy |
| Fallback | `prompt/qwen.txt` | Concise, verbosity-calibrated |

### Layer 2: Dynamic Environment Context
**`src/session/system.ts:32-57`** appends runtime info wrapped in `<env>` XML tags: working directory, git status, platform, date, model ID, and directory listing.

### Layer 3: Instruction Files (AGENTS.md / CLAUDE.md)
**`src/session/instruction.ts:14-141`** loads `AGENTS.md`, `CLAUDE.md`, and `CONTEXT.md` from the project tree. These are also injected contextually when the Read tool accesses files near an instruction file (appended as `<system-reminder>` tags in tool output at `src/tool/read.ts:220`).

### Layer 4: Skills Metadata
**`src/session/system.ts:59-71`** appends available skill descriptions so the agent knows when to load specialized knowledge.

### Layer 5: Agent-Specific Prompt Override
If the active agent has its own `prompt` field (e.g., `plan`, `explore`, `title`), it replaces the provider-specific base prompt. Agent prompts live in `src/agent/prompt/*.txt`.

### Layer 6: Plugin Transforms
Two plugin hooks mutate prompts before the LLM call:
- `experimental.chat.system.transform` at `src/session/llm.ts:83` - modifies system messages
- `experimental.chat.messages.transform` at `src/session/prompt.ts:650` - modifies conversation messages

### Layer 7: Provider Middleware
**`src/provider/transform.ts:55-199`** applies model-specific transforms:
- Claude: normalizes tool call IDs, adds cache control to system + recent messages
- Mistral: truncates tool call IDs, injects synthetic `"Done."` messages between tool/user turns
- Models with `interleaved` capability: extracts reasoning into `providerOptions`

### Final Assembly
**`src/session/llm.ts:225-247`** combines everything into the `streamText()` call:
```
[system messages] + [conversation history as ModelMessage[]] -> streamText()
```

---

## Agent Composition & Dataflow

### Built-in Agents (`src/agent/agent.ts:76-203`)

| Agent | Mode | Purpose |
|---|---|---|
| `build` | primary | Default agent, full tool access |
| `plan` | primary | Read-only analysis mode |
| `general` | subagent | Parallel multi-step work |
| `explore` | subagent | Fast codebase search |
| `compaction` | hidden | Context window pruning |
| `title` | hidden | Session title generation |
| `summary` | hidden | Session diff summarization |

Custom agents can be added via `opencode.json` config or `.opencode/agents/*.md` markdown files with frontmatter.

### Main Orchestration Loop

The core dataflow is driven by **`SessionPrompt.loop()`** at `src/session/prompt.ts:275-732`:

```
User Input
    |
    v
SessionPrompt.prompt()           # Persists user message to SQLite
    |
    v
SessionPrompt.loop()             # MAIN LOOP (while true)
    |
    +---> Check pending subtasks
    |       +---> Compaction subtask?  --> SessionCompaction.process()
    |       +---> Task subtask?       --> Spawn child session (sub-agent)
    |
    +---> Check context overflow    --> Trigger compaction if needed
    |
    +---> Normal processing:
            |
            +---> Resolve agent config (Agent.get())
            +---> Resolve tools (resolveTools())
            |       +---> Built-in tools (15 core tools)
            |       +---> Custom tools (.opencode/tools/*.ts)
            |       +---> Plugin tools
            |       +---> MCP server tools
            |
            +---> Build system prompt (all layers above)
            +---> Plugin hooks: messages.transform, system.transform
            |
            +---> SessionProcessor.create()
                    |
                    v
              LLM.stream()
                +---> Plugin hooks: chat.params, chat.headers
                +---> Provider middleware (ProviderTransform)
                +---> Vercel AI SDK streamText()
                    |
                    v
              Stream Processing (for-await on fullStream)
                +---> Reasoning parts  --> Bus.publish() --> UI
                +---> Text parts       --> Bus.publish() --> UI
                +---> Tool calls       --> Execute tool --> Bus.publish()
                +---> Tool results     --> Bus.publish() --> UI
                +---> Step finish      --> Usage tracking, snapshot
                    |
                    v
              Returns: "continue" | "stop" | "compact"
                    |
                    v
              Loop decides: iterate again or break
                    |
                    v
              Bus events --> Server SSE --> UI / Desktop / ACP clients
```

### Sub-Agent Spawning (Task Tool)
**`src/tool/task.ts:28-166`** implements agent-to-agent composition. When a primary agent calls the Task tool:
1. A child `Session` is created with restricted permissions
2. The specified sub-agent (e.g., `explore`, `general`) is assigned
3. `SessionPrompt.prompt()` is called recursively, triggering a full new `loop()`
4. The sub-agent's text output is returned wrapped in `<task_result>` tags

The Task tool's description template (`src/tool/task.txt`) has an `{agents}` placeholder dynamically replaced with available sub-agent names and descriptions.

### Event Bus
**`src/bus/index.ts`** provides instance-scoped pub/sub. Key events flow through:
- `Session.Event.Created/Updated/Diff/Error`
- `MessageV2.Event.Updated/PartUpdated/PartDelta`
- `SessionStatus.Event.Status/Idle`
- `PermissionNext.Event.Asked/Replied`

Events propagate to: the HTTP server (SSE), TUI, web/desktop apps, and ACP clients.

### Plugin Hook Points (Middleware Chain)

| Hook | When | Location |
|---|---|---|
| `chat.message` | User message created | `prompt.ts:1303` |
| `chat.params` | Before LLM call | `llm.ts:114` |
| `chat.headers` | Before LLM HTTP request | `llm.ts:133` |
| `experimental.chat.system.transform` | System prompt assembly | `llm.ts:83` |
| `experimental.chat.messages.transform` | Message array transform | `prompt.ts:650` |
| `tool.definition` | Tool schema resolution | `registry.ts:162` |
| `tool.execute.before` | Pre-tool execution | `prompt.ts:800` |
| `tool.execute.after` | Post-tool execution | `prompt.ts:821` |
| `experimental.text.complete` | Text part finished | `processor.ts:323` |

### State Management
- **Per-project state**: `Instance.state()` uses `AsyncLocalStorage`-style context propagation so each module (Agent, Bus, Config, Tools, MCP, etc.) has isolated state per project directory
- **Persistence**: Messages and parts stored in SQLite via Drizzle ORM (`SessionTable`, `MessageTable`, `PartTable`)
- **Context window management**: `SessionCompaction` detects overflow and summarizes conversation history using a structured template (Goal / Instructions / Discoveries / Accomplished / Relevant files)
