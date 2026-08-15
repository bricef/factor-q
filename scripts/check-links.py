#!/usr/bin/env python3
"""Check that relative links in the repo's markdown files resolve.

Covers inline links and images — ``[text](path)`` / ``![alt](path)`` —
including ``#fragment`` targets (the fragment itself is not validated).
External schemes (http/https/mailto) and pure-fragment links are ignored.
Links that resolve to a path *outside* the repository (e.g. a sibling-repo
checkout) can't be validated portably, so they are reported as skipped
rather than broken.

Usage: check-links.py [ROOT]   (default: the repository containing this
script). Exits non-zero if any link is broken — CI runs this via
``just check-links``.

SKIP_PARTS is matched against each file's path *relative to ROOT*, never
its absolute path, and that distinction is the whole gate. Matching the
absolute path made the checker silently self-disable inside the repo's
own standard layout: worktrees live at ``.claude/worktrees/<name>``, so
every file under one carried a ``.claude`` component, every markdown file
was skipped, and ``just check-links`` reported "0 relative links checked"
and exited 0 — a green gate that had checked nothing, in exactly the
place the repo tells people to work. Relative matching keeps the original
intent (don't scan agent scratch, build output, or vendored trees *nested
beneath* the tree being checked, including a worktree's own nested
``.claude``) without letting ROOT's own address turn the check off.

The empty-scan guard is the belt to that brace: scanning zero markdown
files is reported as a failure rather than a pass, so any future variant
of "the filter ate everything" comes back red instead of vacuously green.
"""

import re
import sys
from pathlib import Path

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
SKIP_PARTS = {".git", "target", "node_modules", "dist", ".claude"}
SKIP_SCHEMES = ("http://", "https://", "mailto:")


def main() -> int:
    root = (
        Path(sys.argv[1]).resolve()
        if len(sys.argv) > 1
        else Path(__file__).resolve().parent.parent
    )
    broken: list[str] = []
    external: list[str] = []
    checked = 0
    scanned = 0

    for md in sorted(root.rglob("*.md")):
        rel = md.relative_to(root)
        if any(part in SKIP_PARTS for part in rel.parts):
            continue
        scanned += 1
        text = md.read_text(encoding="utf-8", errors="replace")
        for m in LINK.finditer(text):
            target = m.group(1)
            if target.startswith(SKIP_SCHEMES) or target.startswith("#"):
                continue
            path = target.split("#", 1)[0]
            if not path:
                continue
            checked += 1
            resolved = (md.parent / path).resolve()
            where = f"{rel}:{text[: m.start()].count(chr(10)) + 1}"
            if not resolved.is_relative_to(root):
                external.append(f"{where}: {target}")
            elif not resolved.exists():
                broken.append(f"{where}: {target}")

    for b in broken:
        print(f"BROKEN   {b}")
    for e in external:
        print(f"SKIPPED  {e}  (outside the repo — not verifiable here)")
    print(
        f"{checked} relative links checked in {scanned} files: "
        f"{len(broken)} broken, {len(external)} outside the repo"
    )
    if not scanned:
        print(
            f"ERROR: no markdown scanned under {root} — the check did nothing, "
            "which is a failure, not a pass",
            file=sys.stderr,
        )
        return 1
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main())
