# meta/

Repo-travelling, agent-agnostic working material *about* this repository — as
opposed to the product (which has its own `agents/` directory and skill format,
ADR-0019) and CI plumbing (`scripts/`).

## Layout

- `meta/skills/<name>/SKILL.md` — a repeatable procedure an agent (or a human)
  can follow: when to use it, the steps, the verification anchors, and any
  utility scripts colocated in the same directory.

## Discovery

`AGENTS.md` points agents here. For Claude Code specifically,
`.claude/skills/<name>/SKILL.md` holds a thin tracked shim per skill that
defers to the `meta/skills/` copy, so each is invocable as a slash command —
the substance stays agent-agnostic in this directory.
