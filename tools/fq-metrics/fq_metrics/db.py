"""SQLite schema and idempotent writes for the attempt ledger."""

from __future__ import annotations

import sqlite3
from pathlib import Path

SCHEMA = """
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS tasks(
  issue INTEGER PRIMARY KEY, title TEXT, type_label TEXT, created_at TEXT,
  admitted_at TEXT, admission_body_hash TEXT, closed_at TEXT, current_status TEXT
);
CREATE TABLE IF NOT EXISTS transitions(
  issue INTEGER NOT NULL, at TEXT NOT NULL, kind TEXT NOT NULL,
  from_state TEXT, to_state TEXT, actor TEXT, ref TEXT NOT NULL DEFAULT '',
  UNIQUE(issue, at, kind, ref)
);
CREATE TABLE IF NOT EXISTS attempts(
  invocation_id TEXT PRIMARY KEY, issue INTEGER, trigger_id TEXT, agent TEXT,
  model TEXT, dispatched_at TEXT, ended_at TEXT, outcome_event TEXT,
  task_status TEXT, error_kind TEXT, total_cost REAL, llm_calls INTEGER,
  tool_calls INTEGER, pr_number INTEGER
);
CREATE TABLE IF NOT EXISTS outcomes(
  issue INTEGER NOT NULL, pr_number INTEGER NOT NULL, merged_at TEXT,
  accepted_at TEXT, accepted INTEGER NOT NULL DEFAULT 0,
  correction_commit TEXT, rule TEXT, baseline_equivalent TEXT,
  PRIMARY KEY(issue, pr_number)
);
CREATE TABLE IF NOT EXISTS size_tags(
  pr_number INTEGER PRIMARY KEY, at TEXT NOT NULL, issue INTEGER,
  baseline_equivalent TEXT NOT NULL, note TEXT, source TEXT
);
CREATE TABLE IF NOT EXISTS interventions(
  at TEXT NOT NULL, issue INTEGER NOT NULL, type TEXT NOT NULL, minutes INTEGER,
  note TEXT, source TEXT, UNIQUE(at, issue, type, note)
);
CREATE TABLE IF NOT EXISTS pull_requests(
  number INTEGER PRIMARY KEY, issue INTEGER, created_at TEXT, merged_at TEXT,
  closed_at TEXT, head_branch TEXT, body TEXT, agent_authored INTEGER NOT NULL,
  provenance_invocation TEXT, additions INTEGER, deletions INTEGER
);
CREATE TABLE IF NOT EXISTS corrective_commits(
  sha TEXT NOT NULL, pr_number INTEGER NOT NULL, issue INTEGER NOT NULL,
  at TEXT NOT NULL, rule TEXT NOT NULL, additions INTEGER, deletions INTEGER,
  PRIMARY KEY(sha, pr_number)
);
"""


def connect(path: str | Path) -> sqlite3.Connection:
    db = sqlite3.connect(path)
    db.row_factory = sqlite3.Row
    db.executescript(SCHEMA)
    columns = {row[1] for row in db.execute("PRAGMA table_info(outcomes)")}
    if "baseline_equivalent" not in columns:
        db.execute("ALTER TABLE outcomes ADD COLUMN baseline_equivalent TEXT")
    return db


def upsert(db: sqlite3.Connection, table: str, values: dict, keys: tuple[str, ...]) -> None:
    columns = list(values)
    marks = ",".join("?" for _ in columns)
    updates = [c for c in columns if c not in keys]
    conflict = ",".join(keys)
    action = ("DO UPDATE SET " + ",".join(f"{c}=excluded.{c}" for c in updates)
              if updates else "DO NOTHING")
    db.execute(
        f"INSERT INTO {table} ({','.join(columns)}) VALUES ({marks}) "
        f"ON CONFLICT({conflict}) {action}",
        [values[c] for c in columns],
    )


def transition(db: sqlite3.Connection, **values: object) -> None:
    values.setdefault("ref", "")
    upsert(db, "transitions", values, ("issue", "at", "kind", "ref"))
