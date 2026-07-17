---
name: architecture-diagram
description: Regenerate docs/design/committed/architecture-diagram.{dot,svg,png} — a code-verified Graphviz map of factor-q's components, dataflow, and interfaces. Use when asked to refresh, recreate, or update the architecture diagram, or after architecture-changing work lands.
---

# Regenerate the architecture diagram

The diagram is a **cleanroom read of the code, not of the docs**: every box
and edge label must be verifiable against source. The checked-in
`docs/design/committed/architecture-diagram.dot` is the prior; the job is to find where
reality has drifted from it, update the dot, and re-render.

## Process

### 1. Establish drift

The dot's title line carries the date of the last regeneration. List what
landed since:

```sh
git log --oneline --since=<that date> -- services/ adapters/ justfile
```

Skim for architecture-relevant changes: new crates or binaries, new NATS
subjects/streams, new ports, components added/removed/split, store changes,
new external services, and — especially — previously-unwired parts becoming
wired in (or the reverse).

### 2. Verify every load-bearing label against source

Do not trust the existing dot or ARCHITECTURE.md. Anchors:

| Claim in the diagram | Source of truth |
|---|---|
| Stream names, subjects, retention, max_deliver | `services/fq-runtime/crates/fq-runtime/src/bus.rs` (`STREAM_NAME`, `TRIGGER_STREAM_NAME`, `ADVISORY_STREAM_NAME`, `CONTROL_*_SUBJECT`) |
| Event subject shapes (`fq.agent.<id>.*`, `fq.worker.*`) | `services/fq-runtime/crates/fq-runtime/src/events.rs` |
| Daemon composition (which tasks `fq run` hosts) | `run_daemon` in `services/fq-runtime/crates/fq-cli/src/main.rs` |
| CLI verbs; which read the DB directly vs. talk to NATS | `enum Commands` + handlers in `fq-cli/src/main.rs` |
| The stores and their source-of-truth status | `fq-runtime/src/control_plane/store.rs`, the worker store, `control_plane/projection/` |
| Read-service port, loopback guard, on/off default | `fq-runtime/src/read_service.rs`, `fq-runtime/src/config.rs` |
| Dashboard ports and read path | `services/fq-dashboard/src/main.rs` (arg defaults) |
| LLM / MCP / pricing integrations | `fq-runtime/src/llm/`, the worker's MCP modules, `fq-runtime/src/pricing.rs` |
| Trigger adapters | `adapters/` (one box per adapter binary) |
| fq-cas wiring status | Does any fq-runtime crate depend on or call `fq-store`? While the answer is no, fq-cas stays a **dashed "not yet wired" box**; the moment it is wired, redraw it as a live component. Ports/auth: `services/fq-store` |
| Binaries shipped | `[[bin]]` sections, `install.sh`, root `justfile` build targets |

Quote ports, subjects, and stream names exactly — a diagram that says
`fq.control.*` when the code says something else is worse than no diagram.

### 3. Update the dot

Edit `docs/design/committed/architecture-diagram.dot` in place and update the date in
its title line. Keep the established visual language:

- Solid arrows = dataflow/requests; dashed = control and secondary paths;
  circled digits ①–⑧ = the invocation lifecycle; grey dashed cluster = built
  but not yet wired in.
- Green fill = operator surfaces, lavender = NATS, blue = daemon,
  yellow = state, grey ellipses = external processes/services.

### 4. Render

```sh
node meta/skills/architecture-diagram/render.mjs
```

The script prefers a real `dot` on PATH and falls back to the WASM Graphviz
(`@viz-js/viz`, installed into this skill's directory on first use). It writes
the SVG next to the dot, and a PNG too when it can find a chromium
(`$CHROME_BIN`, PATH, or a Playwright browser cache).

### 5. Inspect before declaring done

Open and actually look at the rendered PNG. Check: nothing clipped at the
canvas edges, no label overlapping another node, no cluster title colliding
with its first row. Iterate on the dot until clean — never ship a render you
have not looked at.

### 6. Report

Summarise the architectural drift found (not the cosmetic churn), then offer
to commit `docs/design/committed/architecture-diagram.{dot,svg,png}`.

## Layout rules that keep the render clean

Hard-won against the WASM renderer's quirks; keep them unless re-verified:

- `rankdir=TB` with flow **operator surfaces → NATS → daemon → stores /
  external services**. LR produced a 3300px-wide canvas with arcing
  cross-edges.
- The WASM renderer **underestimates bold text width**: give bold names their
  own line (details on a smaller `<font>` line below), and put `&#160;` after
  any `</b>` that shares a line with regular text — otherwise labels overlap.
- Worker→bus publish edges: write them as `nats_events -> worker [dir=back]`
  so the arrow reads worker→bus while rank order stays sources-above-daemon.
- Consumers that write to stores land far from them by default; `minlen=3..4`
  on their inbound edges pulls them down next to the store cluster.
- `pad=0.5` on the graph absorbs text-metric overflow at canvas edges;
  without it edge and cluster labels get clipped.
- Keep cluster labels short — a long mixed bold/regular cluster label is the
  worst overflow case.
- PNG capture: SVG dimensions are in pt and chromium renders at 4/3 px per
  pt. The render script oversizes the window and trims with ImageMagick;
  don't fight this by hand.
