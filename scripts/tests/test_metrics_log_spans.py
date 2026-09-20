import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "metrics-log.py"


FIXTURE = """\
2026-09-20T09:00:00Z\tUserPromptSubmit\ts1\t/repo\t101
2026-09-20T09:07:00Z\tStop\ts1\t/repo\t101
2026-09-20T09:20:00Z\tUserPromptSubmit\ts1\t/repo\t101
2026-09-20T09:30:00Z\tStop\ts1\t/repo\t101
2026-09-20T10:00:00Z\tUserPromptSubmit\ts2\t/repo\t202
2026-09-20T10:11:00Z\tStop\ts2\t/repo\t202
2026-09-20T10:20:00Z\tUserPromptSubmit\ts2\t/repo\t
2026-09-20T10:27:00Z\tStop\ts2\t/repo\t
"""


class SpansTest(unittest.TestCase):
    def run_spans(self, *extra):
        with tempfile.TemporaryDirectory() as tmp:
            log = pathlib.Path(tmp) / "touch.log"
            log.write_text(FIXTURE)
            return subprocess.run(
                ["python3", str(SCRIPT), "spans", "--log", str(log), *extra],
                check=True, text=True, capture_output=True,
            ).stdout.strip()

    def test_bursts_gap_rounding_and_unattributed_output(self):
        output = self.run_spans()
        self.assertIn("101, 10, 2026-09-20T09:00:00Z, 2026-09-20T09:07:00Z, s1", output)
        self.assertIn("101, 10, 2026-09-20T09:20:00Z, 2026-09-20T09:30:00Z, s1", output)
        self.assertIn("202, 15, 2026-09-20T10:00:00Z, 2026-09-20T10:11:00Z, s2", output)
        self.assertIn("unattributed minutes", output)
        self.assertIn("10, 2026-09-20T10:20:00Z, 2026-09-20T10:27:00Z, s2", output)

    def test_emit_prints_draft_commands_without_appending(self):
        output = self.run_spans("--emit")
        self.assertIn('just touch 101 10 fix "session s1"', output)
        self.assertIn('just touch 202 15 fix "session s2"', output)
        self.assertIn("unattributed minutes", output)

    def test_since_filters_earlier_events(self):
        output = self.run_spans("--since", "2026-09-20T10:00:00Z")
        self.assertNotIn("101,", output)
        self.assertIn("202, 15", output)


if __name__ == "__main__":
    unittest.main()
