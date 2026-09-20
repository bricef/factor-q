from __future__ import annotations

import json
import sqlite3
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

from fq_metrics import events, git_history, github
from fq_metrics.db import connect, upsert

FIXTURES = Path(__file__).parent / "fixtures"


def fixture(name):
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


class ExtractorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.db = connect(Path(self.temp.name) / "ledger.sqlite")

    def tearDown(self):
        self.db.close()
        self.temp.cleanup()

    def test_timeline_records_tracked_transitions_and_admission(self):
        data = fixture("timeline.json")
        mapping = github.record_issue(self.db, data["issue"], data["timeline"])
        # A rerun updates rather than duplicating.
        github.record_issue(self.db, data["issue"], data["timeline"])
        task = self.db.execute("SELECT * FROM tasks WHERE issue=811").fetchone()
        self.assertEqual(task["admitted_at"], "2026-09-15T09:28:00Z")
        self.assertIsNone(task["admission_body_hash"])
        self.assertEqual(task["current_status"], "done")
        self.assertEqual(mapping, {900: 811})
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM transitions").fetchone()[0], 5)

    def test_one_attempt_per_trigger_and_terminal_totals(self):
        rows = fixture("events.json")
        events.import_events(self.db, rows)
        events.import_events(self.db, rows)
        attempts = self.db.execute("SELECT * FROM attempts ORDER BY invocation_id").fetchall()
        self.assertEqual(len(attempts), 2)
        self.assertEqual(attempts[0]["task_status"], "success")
        self.assertEqual(attempts[0]["tool_calls"], 7)
        self.assertEqual(attempts[1]["outcome_event"], "failed")
        self.assertEqual(attempts[1]["error_kind"], "tool_error")

    def test_trigger_without_an_issue_is_skipped_not_fatal(self):
        probe = {
            "envelope": {"agent_id": "doc-drift", "invocation_id": "inv-probe",
                         "timestamp": "2026-09-15T11:00:00Z"},
            "payload": {"event_type": "triggered", "payload": {
                "trigger_id": "trigger-9",
                "trigger_payload": "Daily doc-drift check. Review the last 36 hours.",
                "config_snapshot": {"model": "test-model"}}},
        }
        events.import_events(self.db, fixture("events.json") + [probe])
        rows = self.db.execute("SELECT invocation_id FROM attempts ORDER BY 1").fetchall()
        self.assertEqual([row[0] for row in rows], ["inv-1", "inv-2"])

    def test_provenance_joins_attempt_and_human_pr_does_not(self):
        events.import_events(self.db, fixture("events.json"))
        agent_pr, human_pr = fixture("prs.json")
        reviews = [{"id": 1, "submitted_at": "2026-09-15T10:00:00Z",
                    "state": "CHANGES_REQUESTED", "user": {"login": "reviewer"}}]
        commits = [{"sha": "abc", "commit": {"committer": {"date": "2026-09-15T10:05:00Z"}},
                    "author": {"login": "agent"}}]
        github.record_pr(self.db, agent_pr, 811, reviews, [], commits)
        github.record_pr(self.db, human_pr, 812, [], [], [])
        attempt = self.db.execute("SELECT pr_number FROM attempts WHERE invocation_id='inv-1'").fetchone()
        self.assertEqual(attempt[0], 900)
        values = self.db.execute("SELECT number,agent_authored FROM pull_requests ORDER BY number").fetchall()
        self.assertEqual([tuple(row) for row in values], [(900, 1), (901, 0)])
        kinds = {row[0] for row in self.db.execute("SELECT kind FROM transitions WHERE issue=811")}
        self.assertTrue({"pr_opened", "review", "push_after_review", "merged"} <= kinds)

    def test_fourteen_day_acceptance_rule_both_directions(self):
        for number, issue in ((900, 811), (902, 812)):
            upsert(self.db, "pull_requests", {
                "number": number, "issue": issue, "created_at": "2026-09-15T09:00:00Z",
                "merged_at": "2026-09-15T11:05:00Z", "closed_at": "2026-09-15T11:05:00Z",
                "head_branch": "m0/test", "body": "", "agent_authored": 1,
                "provenance_invocation": None, "additions": 10, "deletions": 2,
            }, ("number",))
        raw = fixture("git_log.json")
        history = [git_history.Commit(row["sha"], row["at"], row["subject"],
                                      row["body"], set(row["files"])) for row in raw]
        history.append(git_history.Commit("merge902", "2026-09-15T11:05:00Z",
                                          "Other feature (#902)", "", {"src/z.rs"}))
        with patch.object(git_history, "commits", return_value=history), \
             patch.object(git_history, "numstat", return_value=(2, 1)):
            git_history.extract(self.db, ".", now=datetime(2026, 10, 1, tzinfo=timezone.utc))
        corrected = self.db.execute("SELECT * FROM outcomes WHERE pr_number=900").fetchone()
        survived = self.db.execute("SELECT * FROM outcomes WHERE pr_number=902").fetchone()
        self.assertEqual((corrected["accepted"], corrected["rule"]), (0, "references_issue"))
        self.assertEqual(survived["accepted"], 1)
        self.assertEqual(survived["accepted_at"], "2026-09-29T11:05:00Z")


if __name__ == "__main__":
    unittest.main()
