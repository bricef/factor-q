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
"""

import re
import sys
from pathlib import Path

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
SKIP_PARTS = {".git", "target", "node_modules", "dist"}
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

    for md in sorted(root.rglob("*.md")):
        if any(part in SKIP_PARTS for part in md.parts):
            continue
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
            where = f"{md.relative_to(root)}:{text[: m.start()].count(chr(10)) + 1}"
            if not resolved.is_relative_to(root):
                external.append(f"{where}: {target}")
            elif not resolved.exists():
                broken.append(f"{where}: {target}")

    for b in broken:
        print(f"BROKEN   {b}")
    for e in external:
        print(f"SKIPPED  {e}  (outside the repo — not verifiable here)")
    print(
        f"{checked} relative links checked: "
        f"{len(broken)} broken, {len(external)} outside the repo"
    )
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main())
