"""Turn Claude Code touch-hook events into reviewable touch-minute drafts."""

from __future__ import annotations

import datetime as dt
import json
import math
import pathlib
from dataclasses import dataclass


@dataclass
class Span:
    issue: str
    minutes: int
    first: str
    last: str
    session: str
    first_at: dt.datetime


def parse_timestamp(value: str) -> dt.datetime:
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    parsed = dt.datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed


def read_spans(path: pathlib.Path, since: str | None = None) -> list[Span]:
    threshold = parse_timestamp(since) if since else None
    active: dict[tuple[str, str], dict[str, object]] = {}
    spans: list[Span] = []

    def finish(key: tuple[str, str]) -> None:
        burst = active.pop(key, None)
        if not burst or burst["stop_at"] is None:
            return
        first_at = burst["first_at"]
        stop_at = burst["stop_at"]
        assert isinstance(first_at, dt.datetime)
        assert isinstance(stop_at, dt.datetime)
        elapsed = max(0.0, (stop_at - first_at).total_seconds() / 60)
        minutes = int(math.ceil(elapsed / 5) * 5)
        spans.append(Span(
            issue=key[1], minutes=minutes,
            first=str(burst["first_text"]), last=str(burst["stop_text"]),
            session=key[0], first_at=first_at,
        ))

    if not path.exists():
        raise SystemExit(f"touch log not found: {path}")
    for raw in path.read_text().splitlines():
        fields = raw.split("\t", 4)
        if len(fields) != 5:
            continue
        timestamp, event, session, _cwd, issue = fields
        try:
            at = parse_timestamp(timestamp)
        except ValueError:
            continue
        if threshold is not None and at < threshold:
            continue
        key = (session, issue)
        burst = active.get(key)
        if burst is not None:
            previous = burst["previous"]
            assert isinstance(previous, dt.datetime)
            if at - previous > dt.timedelta(minutes=10):
                # A delayed Stop still closes the prompt that preceded it;
                # other events after the gap begin (or await) a new burst.
                if event == "Stop":
                    burst["stop_at"] = at
                    burst["stop_text"] = timestamp
                    finish(key)
                    continue
                finish(key)
                burst = None
        if event == "UserPromptSubmit":
            if burst is None:
                burst = {
                    "first_at": at, "first_text": timestamp,
                    "stop_at": None, "stop_text": "", "previous": at,
                }
                active[key] = burst
            else:
                burst["previous"] = at
        elif burst is not None:
            burst["previous"] = at
            if event == "Stop":
                burst["stop_at"] = at
                burst["stop_text"] = timestamp
            elif event == "SessionEnd":
                finish(key)
    for key in list(active):
        finish(key)
    return sorted(spans, key=lambda span: span.first_at)


def run_spans(log: str, since: str | None, emit: bool) -> str:
    spans = read_spans(pathlib.Path(log).expanduser(), since)
    attributed = [span for span in spans if span.issue]
    unattributed = [span for span in spans if not span.issue]
    lines: list[str] = []
    if emit:
        for span in attributed:
            note = json.dumps(f"session {span.session}")
            lines.append(f"just touch {span.issue} {span.minutes} fix {note}")
    else:
        lines.append("issue, minutes, first, last, session")
        lines.extend(
            f"{span.issue}, {span.minutes}, {span.first}, {span.last}, {span.session}"
            for span in attributed
        )
    if unattributed:
        if lines:
            lines.append("")
        lines.append("unattributed minutes")
        lines.append("minutes, first, last, session")
        lines.extend(
            f"{span.minutes}, {span.first}, {span.last}, {span.session}"
            for span in unattributed
        )
    return "\n".join(lines)
