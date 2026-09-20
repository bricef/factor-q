from __future__ import annotations

import json
import tempfile
import unittest
import xml.etree.ElementTree as ET
from datetime import datetime, timezone
from pathlib import Path

from fq_metrics.db import connect, transition, upsert
from fq_metrics.report import cumulative_flow, markdown, report_data, write_report

UTC = timezone.utc


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.ledger = Path(self.temp.name) / "ledger.sqlite"
        self.db = connect(self.ledger)
        self._fixture()

    def tearDown(self):
        self.db.close()
        self.temp.cleanup()

    def _label(self, issue, at, label, applied=True):
        transition(self.db, issue=issue, at=at, kind="label", from_state=None,
                   to_state=label if applied else f"-{label}", actor="test",
                   ref=("add:" if applied else "remove:") + label)

    def _fixture(self):
        self._label(1, "2026-01-01T00:00:00Z", "fleet:candidate")
        self._label(1, "2026-01-08T00:00:00Z", "status:ready")
        self._label(1, "2026-01-15T00:00:00Z", "status:in-progress")
        self._label(1, "2026-01-17T00:00:00Z", "status:in-review")
        self._label(1, "2026-01-20T00:00:00Z", "status:done")
        self._label(2, "2026-01-01T00:00:00Z", "fleet:candidate")
        self._label(2, "2026-01-08T00:00:00Z", "fleet:refined")
        self._label(2, "2026-01-15T00:00:00Z", "status:blocked")
        self._label(2, "2026-01-16T00:00:00Z", "status:blocked", False)
        transition(self.db, issue=2, at="2026-01-18T00:00:00Z", kind="closed",
                   from_state=None, to_state="closed", actor="test", ref="closed")
        for invocation, issue, cost, pr in (("one", 1, 2.0, 10), ("two", 2, 4.0, 11)):
            upsert(self.db, "attempts", {
                "invocation_id": invocation, "issue": issue,
                "dispatched_at": "2026-01-10T00:00:00Z",
                "ended_at": "2026-01-11T00:00:00Z", "total_cost": cost,
                "pr_number": pr,
            }, ("invocation_id",))
        upsert(self.db, "outcomes", {"issue": 1, "pr_number": 10,
               "accepted_at": "2026-01-22T00:00:00Z", "accepted": 1}, ("issue", "pr_number"))
        upsert(self.db, "interventions", {"at": "2026-01-10T12:00:00Z", "issue": 1,
               "type": "fix", "minutes": 10, "note": "a", "source": "test"},
               ("at", "issue", "type", "note"))
        upsert(self.db, "interventions", {"at": "2026-01-11T12:00:00Z", "issue": 1,
               "type": "fix", "minutes": 20, "note": "b", "source": "test"},
               ("at", "issue", "type", "note"))
        upsert(self.db, "pull_requests", {"number": 10, "issue": 1,
               "merged_at": "2026-01-12T00:00:00Z", "agent_authored": 1,
               "additions": 80, "deletions": 20}, ("number",))
        upsert(self.db, "corrective_commits", {"sha": "abc", "pr_number": 10,
               "issue": 1, "at": "2026-01-13T00:00:00Z", "rule": "fix",
               "additions": 5, "deletions": 5}, ("sha", "pr_number"))
        self.db.commit()

    def test_cfd_band_counts_at_three_dates(self):
        points = [datetime(2026, 1, day, tzinfo=UTC) for day in (7, 10, 21)]
        flow = cumulative_flow(self.db, points)
        self.assertEqual(flow["fleet:candidate"], [2, 0, 0])
        self.assertEqual(flow["fleet:refined"], [0, 1, 0])
        self.assertEqual(flow["status:ready"], [0, 1, 0])
        self.assertEqual(flow["status:done"], [0, 0, 1])

    def test_series_cycle_time_and_every_measure(self):
        data = report_data(self.db, since=59, by="week",
                           now=datetime(2026, 1, 29, tzinfo=UTC))
        self.assertEqual(sum(row["accepted"] for row in data["throughput"]), 1)
        in_progress = next(row for row in data["cycle_times"]
                           if row["stage"] == "status:in-progress")
        self.assertEqual((in_progress["median_days"], in_progress["p90_days"]), (2.0, 2.0))
        self.assertEqual(data["measures"], {
            "first_pass_rate": 0.0,
            "attempts_per_accept": 2.0,
            "correction_ratio": 0.1,
            "touch_per_accept_minutes": 30.0,
            "mtbi_hours": 24.0,
            "cost_per_attempt": 3.0,
            "cost_per_accepted_change": 2.0,
        })

    def test_empty_intervention_measures_are_na(self):
        self.db.execute("DELETE FROM interventions")
        data = report_data(self.db, since=59, now=datetime(2026, 1, 29, tzinfo=UTC))
        self.assertIsNone(data["measures"]["touch_per_accept_minutes"])
        self.assertIsNone(data["measures"]["mtbi_hours"])
        self.assertIn("n/a (no interventions logged)", markdown(data))

    def test_renderer_writes_parseable_svgs_and_reports(self):
        self.db.close()
        output = Path(self.temp.name) / "out"
        write_report(self.ledger, output, since=59, now=datetime(2026, 1, 29, tzinfo=UTC))
        for name in ("cumulative-flow.svg", "throughput.svg", "cycle-times.svg"):
            path = output / name
            self.assertGreater(path.stat().st_size, 0)
            ET.parse(path)
        self.assertIn("cumulative_flow", json.loads((output / "report.json").read_text()))
        self.db = connect(self.ledger)


if __name__ == "__main__":
    unittest.main()
