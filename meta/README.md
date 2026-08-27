# meta/

Repo-travelling, agent-agnostic working material *about* this repository — as
opposed to the product (which has its own `agents/` directory and skill format,
ADR-0019) and CI plumbing (`scripts/`).

## Layout

- `meta/skills/<name>/SKILL.md` — a repeatable procedure an agent (or a human)
  can follow: when to use it, the steps, the verification anchors, and any
  utility scripts colocated in the same directory.

## Discovery

`AGENTS.md` points agents here, and that is the discovery path every skill
in this directory has.

A skill that should *also* be invocable as a Claude Code slash command gets
a thin tracked shim at `.claude/skills/<name>/SKILL.md` deferring to the
`meta/skills/` copy — the substance stays agent-agnostic in this directory.
The shim is opt-in, not automatic: `architecture-diagram` and
`backlog-grooming` have one, `agent-prompt-engineering` does not. Adding a
skill here therefore does not give it a slash command; add the shim if you
want one.
