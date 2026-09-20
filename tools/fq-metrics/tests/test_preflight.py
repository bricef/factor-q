from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from fq_metrics import preflight


def completed(returncode: int = 0, stdout: str = "", stderr: str = "") -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr=stderr)


class PreflightTests(unittest.TestCase):
    def assert_actionable(self, error: preflight.ToolError, *needles: str) -> None:
        text = str(error)
        self.assertTrue(text.startswith("fq-metrics: "), text)
        self.assertIn("  why:", text)
        self.assertIn("  fix:", text)
        for needle in needles:
            self.assertIn(needle, text)

    def test_missing_fq_names_the_binary_its_purpose_and_both_remedies(self):
        with patch("fq_metrics.preflight.shutil.which", return_value=None):
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_fq()
        self.assert_actionable(caught.exception, "`fq` is not on PATH", "factor-q CLI",
                               "fq events query", "cargo build", "fq connect", "--events")

    def test_unpaired_fq_shows_its_stderr_and_the_pairing_remedy(self):
        with patch("fq_metrics.preflight.shutil.which", return_value="/usr/bin/fq"), \
             patch("fq_metrics.preflight._run", return_value=completed(1, stderr="no pairing for 127.0.0.1:9470")):
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_fq()
        self.assert_actionable(caught.exception, "`fq events query` failed", "no pairing for 127.0.0.1:9470",
                               "fq connect")

    def test_missing_gh_and_signed_out_gh_each_say_how_to_fix(self):
        with patch("fq_metrics.preflight.shutil.which", return_value=None):
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_gh()
        self.assert_actionable(caught.exception, "`gh` is not on PATH", "gh auth login", "--no-github")
        with patch("fq_metrics.preflight.shutil.which", return_value="/usr/bin/gh"), \
             patch("fq_metrics.preflight._run", return_value=completed(1, stderr="You are not logged into any GitHub hosts")):
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_gh()
        self.assert_actionable(caught.exception, "not signed in", "not logged into", "gh auth login")

    def test_git_checks_report_shallow_clones_and_missing_refs(self):
        answers = iter([completed(0, "true\n"), completed(0, "true\n")])
        with patch("fq_metrics.preflight.shutil.which", return_value="/usr/bin/git"), \
             patch("fq_metrics.preflight._run", side_effect=lambda cmd: next(answers)):
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_git(Path("/repo"), "main")
        self.assert_actionable(caught.exception, "shallow", "fetch --unshallow")
        answers = iter([completed(0, "true\n"), completed(0, "false\n"), completed(1)])
        with patch("fq_metrics.preflight.shutil.which", return_value="/usr/bin/git"), \
             patch("fq_metrics.preflight._run", side_effect=lambda cmd: next(answers)):
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_git(Path("/repo"), "release")
        self.assert_actionable(caught.exception, "`release` does not exist", "--git-ref")

    def test_export_directory_must_exist_and_hold_json(self):
        with tempfile.TemporaryDirectory() as empty:
            with self.assertRaises(preflight.ToolError) as caught:
                preflight.check_export(empty)
            self.assert_actionable(caught.exception, "no *.json files", "Event export")
            (Path(empty) / "one.json").write_text("[]", encoding="utf-8")
            preflight.check_export(empty)  # no error once a file exists
        with self.assertRaises(preflight.ToolError) as caught:
            preflight.check_export(str(Path(empty) / "gone"))
        self.assert_actionable(caught.exception, "is not a directory")

    def test_report_without_a_ledger_points_at_extract(self):
        with self.assertRaises(preflight.ToolError) as caught:
            preflight.check_ledger("/nonexistent/attempt_ledger.sqlite")
        self.assert_actionable(caught.exception, "no ledger at", "just metrics-extract", "--ledger")

    def test_mid_run_failures_keep_the_shape_and_pick_a_remedy_by_program(self):
        error = subprocess.CalledProcessError(1, ["gh", "api", "repos/x/y/issues"], stderr="API rate limit exceeded")
        described = preflight.describe_failure(error)
        self.assert_actionable(described, "`gh` exited 1", "rate limit exceeded", "gh api rate_limit", "resumes")
        error = subprocess.CalledProcessError(2, ["fq", "events", "get", "abc"], stderr="gone: the log holds position")
        self.assert_actionable(preflight.describe_failure(error), "`fq` exited 2", "gone: the log", "fq events query --limit 1")


if __name__ == "__main__":
    unittest.main()
