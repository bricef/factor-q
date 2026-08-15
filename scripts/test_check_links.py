#!/usr/bin/env python3
"""Tests for check-links.py's skip logic — the half that can fail silently.

Black-box on purpose: each case builds a throwaway tree, runs the checker
as a subprocess exactly the way `just check-links` and CI do, and asserts
on its exit code and summary line. The regression these exist for is not
"the link parser is wrong" (a wrong parser shows up as a red gate); it is
"the checker skipped everything and exited 0", which no amount of running
the real gate can reveal — a vacuous pass looks identical to a real one.

Run via ``just check-links`` (which depends on ``just test-check-links``)
or directly: ``python3 -m unittest discover -s scripts``.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check-links.py"


def run(root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(CHECKER), str(root)],
        capture_output=True,
        text=True,
        check=False,
    )


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


class SkipLogic(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)

    def test_scans_a_checkout_that_lives_under_dot_claude(self) -> None:
        """ROOT's own address must not disable the check (the false green)."""
        root = self.dir / ".claude" / "worktrees" / "wt"
        write(root / "README.md", "see [design](docs/design.md)\n")
        write(root / "docs" / "design.md", "# Design\n")

        result = run(root)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 relative links checked in 2 files", result.stdout)

    def test_detects_a_broken_link_under_dot_claude(self) -> None:
        """The point of scanning it: breakage there is still caught."""
        root = self.dir / ".claude" / "worktrees" / "wt"
        write(root / "README.md", "see [gone](docs/gone.md)\n")

        result = run(root)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("BROKEN   README.md:1: docs/gone.md", result.stdout)

    def test_skips_skip_parts_nested_beneath_the_root(self) -> None:
        """Scratch, build output and nested worktrees stay out of scope."""
        write(self.dir / "README.md", "# Root\n")
        for nested in (".claude/worktrees/inner", "target/doc", "node_modules/p"):
            write(self.dir / nested / "broken.md", "[gone](nope.md)\n")

        result = run(self.dir)

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("0 relative links checked in 1 files", result.stdout)

    def test_scanning_nothing_is_a_failure_not_a_pass(self) -> None:
        """The guard against every future variant of "the filter ate it all"."""
        write(self.dir / "target" / "only.md", "# Skipped\n")

        result = run(self.dir)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no markdown scanned", result.stderr)


if __name__ == "__main__":
    unittest.main()
