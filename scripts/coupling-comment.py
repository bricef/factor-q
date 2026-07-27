#!/usr/bin/env python3
"""Render a PR comment body from two `fq-lint --coupling --json` runs.

    coupling-comment.py <base.json> <head.json> <head-sha> <repo>

Writes markdown to stdout. Both inputs must come from the SAME fq-lint
binary — the caller builds it once from the head tree and runs it against
each checkout, so a change to the measurement itself can never show up as a
change in the measurement's subject.

Stdlib only, like check-links.py: the runner has python3 and no wheel budget.
"""

import json
import sys

MARKER = "<!-- coupling-metrics -->"

# Sorting modules by production lines matches the review table and puts the
# modules whose coupling matters most at the top.
def load(path):
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    crates = {}
    for c in doc.get("crates", []):
        crates[c["name"]] = {
            "modules": {m["name"]: m for m in c.get("modules", [])},
            # Frozen so cycle groups can go in a set: a group is identified by
            # its membership, not by the order Tarjan happened to pop it.
            "cycles": {frozenset(g) for g in c.get("cycles", [])},
        }
    return crates


def num(n):
    return f"{n:,}"


def delta(before, after, fmt=num):
    """`after` alone when unchanged, `before → after (±n)` when not."""
    if before is None:
        return f"{fmt(after)} (new)"
    if before == after:
        return fmt(after)
    sign = "+" if after > before else "−"
    return f"{fmt(before)} → {fmt(after)} ({sign}{fmt(abs(after - before))})"


def changed(before, after):
    if before is None:
        return True
    return any(
        before.get(k) != after.get(k)
        for k in ("prod_lines", "fan_out", "fan_in", "depends_on")
    )


def edge_changes(before, after):
    """New and dropped dependencies for one module."""
    old = set() if before is None else set(before.get("depends_on", []))
    new = set(after.get("depends_on", []))
    return sorted(new - old), sorted(old - new)


def module_table(crate, base_modules, head_modules):
    rows = []
    for name, mod in sorted(
        head_modules.items(), key=lambda kv: -kv[1]["prod_lines"]
    ):
        before = base_modules.get(name)
        if not changed(before, mod):
            continue
        added, dropped = edge_changes(before, mod)
        notes = []
        if added:
            notes.append("+" + ", ".join(f"`{d}`" for d in added))
        if dropped:
            notes.append("−" + ", ".join(f"`{d}`" for d in dropped))
        rows.append(
            "| `{name}` | {lines} | {out} | {inn} | {notes} |".format(
                name=name,
                lines=delta(
                    None if before is None else before["prod_lines"],
                    mod["prod_lines"],
                ),
                out=delta(
                    None if before is None else before["fan_out"], mod["fan_out"]
                ),
                inn=delta(
                    None if before is None else before["fan_in"], mod["fan_in"]
                ),
                notes=" ".join(notes) or "—",
            )
        )

    gone = sorted(set(base_modules) - set(head_modules))
    for name in gone:
        rows.append(f"| `{name}` | removed | — | — | — |")

    if not rows:
        return []
    return [
        f"**{crate}**",
        "",
        "| Module | Production lines | Fan-out | Fan-in | Dependencies |",
        "|---|---:|---:|---:|---|",
        *rows,
        "",
    ]


def full_table(crate, modules, cycles):
    lines = [
        f"**{crate}** — {len(modules)} modules"
        + (f", {len(cycles)} cycle group(s)" if cycles else ""),
        "",
        "| Module | Production lines | Fan-out | Fan-in | Depends on |",
        "|---|---:|---:|---:|---|",
    ]
    for name, mod in sorted(modules.items(), key=lambda kv: -kv[1]["prod_lines"]):
        deps = ", ".join(f"`{d}`" for d in mod["depends_on"]) or "—"
        # The painful quadrant: costly to change AND changed often.
        hub = " ⚑" if mod["fan_in"] >= 4 and mod["fan_out"] >= 4 else ""
        lines.append(
            f"| `{name}`{hub} | {num(mod['prod_lines'])} | {mod['fan_out']} "
            f"| {mod['fan_in']} | {deps} |"
        )
    lines.append("")
    return lines


def main():
    if len(sys.argv) != 5:
        sys.exit(__doc__)
    base_path, head_path, head_sha, repo = sys.argv[1:5]
    base, head = load(base_path), load(head_path)

    new_cycles, fixed_cycles = [], []
    for crate, data in head.items():
        prior = base.get(crate, {}).get("cycles", set())
        for group in sorted(data["cycles"], key=lambda g: sorted(g)):
            if group not in prior:
                new_cycles.append((crate, sorted(group)))
        for group in sorted(prior, key=lambda g: sorted(g)):
            if group not in data["cycles"]:
                fixed_cycles.append((crate, sorted(group)))

    body = [MARKER, "### Module coupling", ""]

    detail = []
    for crate, data in sorted(head.items()):
        detail += module_table(crate, base.get(crate, {}).get("modules", {}), data["modules"])

    # Headline first: most PRs move nothing here, and that answer should be
    # readable without opening anything.
    if new_cycles:
        body.append(
            f"⚠️ **{len(new_cycles)} new import cycle"
            f"{'' if len(new_cycles) == 1 else 's'}.**"
        )
    elif detail:
        body.append("Module coupling changed in this PR — see below.")
    else:
        body.append("**No change to module coupling.**")
    body.append("")

    for crate, group in new_cycles:
        body.append(
            f"- ⚠️ new cycle in **{crate}**: {', '.join(f'`{m}`' for m in group)} "
            f"— each module in the group can reach every other."
        )
    for crate, group in fixed_cycles:
        body.append(
            f"- ✅ cycle resolved in **{crate}**: {', '.join(f'`{m}`' for m in group)}"
        )
    if new_cycles or fixed_cycles:
        body.append("")

    if detail:
        body += ["#### What changed", ""] + detail

    body += ["<details>", "<summary>Full coupling table</summary>", ""]
    for crate, data in sorted(head.items()):
        body += full_table(crate, data["modules"], data["cycles"])
    body += ["</details>", ""]

    short = head_sha[:12]
    body += [
        "---",
        "",
        f"Advisory — this gates nothing. Measured at "
        f"[`{short}`](https://github.com/{repo}/commit/{head_sha}) against the "
        "merge base, by the same `fq-lint` build on both sides.",
        "",
        "Edges are `crate::`/`super::` paths in production code, between a "
        "crate's top-level modules. Re-exports and trait-method calls resolve "
        "without naming a path, so every count is a floor. ⚑ marks fan-in and "
        "fan-out both ≥ 4. Rationale: "
        "[`docs/reviews/2026-07-27-code-quality-metrics.md`]"
        f"(https://github.com/{repo}/blob/main/docs/reviews/2026-07-27-code-quality-metrics.md).",
    ]
    print("\n".join(body))


if __name__ == "__main__":
    main()
