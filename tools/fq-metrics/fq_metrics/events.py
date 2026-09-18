"""Factor-q event-log import, from the edge or exported JSON files."""

from __future__ import annotations

import json
import sqlite3
import subprocess
from pathlib import Path
from typing import Iterable

from .db import upsert


def _documents(value: object) -> Iterable[dict]:
    if isinstance(value, dict):
        if "envelope" in value or "event_type" in value:
            yield value
        else:
            for key in ("events", "items", "data"):
                if key in value:
                    yield from _documents(value[key])
                    return
    elif isinstance(value, list):
        for item in value:
            yield from _documents(item)


def read_export(directory: str | Path) -> list[dict]:
    records: list[dict] = []
    for path in sorted(Path(directory).rglob("*.json")):
        with path.open(encoding="utf-8") as stream:
            records.extend(_documents(json.load(stream)))
    return records


def edge_events(since: str | None) -> list[dict]:
    """Fetch the index and full payload for starts and terminal events."""
    records: list[dict] = []
    for event_type in ("triggered", "completed", "failed"):
        command = ["fq", "events", "query", "--event-type", event_type, "--json"]
        if since:
            command.extend(["--since", since])
        index = json.loads(subprocess.run(command, check=True, text=True,
                                          capture_output=True).stdout)
        found_payload = False
        for row in _documents(index):
            event_id = row.get("event_id") or (row.get("envelope") or {}).get("event_id")
            if not event_id:
                continue
            output = subprocess.run(["fq", "events", "get", str(event_id), "--json"],
                                    check=True, text=True, capture_output=True).stdout
            records.extend(_documents(json.loads(output)))
            found_payload = True
        # Some edge versions return the complete records in the query response.
        if not found_payload:
            records.extend(_documents(index))
    return records


def _shape(event: dict) -> tuple[dict, str | None, dict]:
    envelope = event.get("envelope") or event
    outer = event.get("payload") or event
    event_type = outer.get("event_type") or event.get("event_type")
    payload = outer.get("payload") if isinstance(outer.get("payload"), dict) else outer
    return envelope, event_type, payload


def _issue(payload: dict) -> int | None:
    trigger = payload.get("trigger_payload") or {}
    github = trigger.get("github") or {}
    value = github.get("issue")
    try:
        return int(value) if value is not None else None
    except (TypeError, ValueError):
        return None


def import_events(db: sqlite3.Connection, source: Iterable[dict]) -> None:
    """Upsert one attempt for each issue-bearing triggered event and its terminal event."""
    records = list(source)
    for event in records:
        envelope, event_type, payload = _shape(event)
        invocation = envelope.get("invocation_id") or event.get("invocation_id")
        if not invocation or event_type != "triggered":
            continue
        issue = _issue(payload)
        if issue is None:
            continue
        config = payload.get("config_snapshot") or {}
        upsert(db, "attempts", {
            "invocation_id": invocation, "issue": issue,
            "trigger_id": payload.get("trigger_id"),
            "agent": envelope.get("agent_id") or config.get("name"),
            "model": config.get("model"),
            "dispatched_at": envelope.get("timestamp"), "ended_at": None,
            "outcome_event": None, "task_status": None, "error_kind": None,
            "total_cost": None, "llm_calls": None, "tool_calls": None,
            "pr_number": None,
        }, ("invocation_id",))
    for event in records:
        envelope, event_type, payload = _shape(event)
        if event_type not in ("completed", "failed"):
            continue
        invocation = envelope.get("invocation_id") or event.get("invocation_id")
        totals = payload.get("partial_totals") or payload
        db.execute(
            "UPDATE attempts SET ended_at=?, outcome_event=?, task_status=?, error_kind=?, "
            "total_cost=?, llm_calls=?, tool_calls=? WHERE invocation_id=?",
            (envelope.get("timestamp"), event_type, payload.get("task_status"),
             payload.get("error_kind"), totals.get("total_cost"),
             totals.get("total_llm_calls"), totals.get("total_tool_calls"), invocation),
        )
