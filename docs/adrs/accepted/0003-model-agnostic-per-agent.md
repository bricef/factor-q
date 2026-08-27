# ADR-0003: Model-Agnostic, Per-Agent Model Selection

## Status

Accepted

Implementation: complete — model choice is a per-agent frontmatter field
resolved through `genai`, with per-model pricing in cost tracking and a
per-agent reasoning effort (#144). Note the runtime settled on
explicit-or-inherited rather than explicit-only; see the last Consequence.

## Context

Agent systems require LLM calls, and different agents have different cost/capability requirements. A supervisory planning agent needs frontier-level reasoning. A document summariser needs speed and low cost. A classification agent may use a fine-tuned specialist model.

## Decision

Model choice is a per-agent configuration concern. A single agent graph can mix models from different providers.

## Rationale

Different tasks have fundamentally different cost/capability profiles. Using a frontier model for document summarisation is wasteful; using a cheap model for supervisory planning is ineffective. The system must make it natural to assign the right model to each agent rather than forcing a global model choice.

## Consequences

- The agent executor must abstract over multiple model providers
- Provider-specific quirks (message formats, tool calling conventions, streaming behaviour) must be normalised
- Cost tracking must handle different pricing across models and providers
- Agent definitions specify their model, or inherit the worker default. The
  runtime resolves this in `parse_agent_with_default`: frontmatter without
  `model:` loads against the worker's `agents.default_model`, and only
  *neither* fails. (The original consequence read "must specify their model
  … explicitly"; the daemon's explicit-or-inherited semantics are the
  authoritative shape — #508 tracks the client validator still rejecting it.)
