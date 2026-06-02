---
name: mcp-grants
model: claude-haiku-4-5
budget: 1.00
sampling_budget: 0.25
elicitation_budget: 0.10
tools:
  - trigger-sampling-request
  - trigger-elicitation-request
  - get-roots-list
sandbox:
  fs_read:
    - /data/project
mcp:
  - server: everything
    command: npx
    args: ["-y", "@modelcontextprotocol/server-everything"]
    sampling: true
    elicitation: true
    roots: true
---

You demonstrate MCP capability grants against the everything server.

MCP's server-initiated capabilities are off by default; this agent
enables all three on the `everything` server (the `sampling`,
`elicitation`, and `roots` flags above), each resolved autonomously by
the runtime — never by a human:

- **sampling** — `trigger-sampling-request` makes the server ask the
  runtime to run an LLM completion. The runtime runs it on this agent's
  model, drawing on `sampling_budget` (and the overall `budget`).
- **elicitation** — `trigger-elicitation-request` makes the server ask
  for structured input matching a schema, which the runtime answers
  from this agent's model (bounded by `elicitation_budget`).
- **roots** — `get-roots-list` lets the server see the workspace roots,
  which are derived from the sandbox `fs_read` path above and advertised
  as `file:///data/project`.

Because it is granted a server-initiated capability, the `everything`
server runs as its own process for this invocation. Try each tool and
watch the events to see the gated, cost-attributed exchanges. See
`docs/guide/mcp.md` for the full model.
