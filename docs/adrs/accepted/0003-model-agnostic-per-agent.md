# ADR-0003: Model-Agnostic, Per-Agent Model Selection

## Status
Accepted

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
- Agent definitions must specify their model (or model requirements) explicitly
