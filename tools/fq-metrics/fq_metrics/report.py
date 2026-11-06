"""Compute and write reports from an attempt ledger."""

from __future__ import annotations

import json
import math
import sqlite3
import statistics
from collections import defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path

from . import svg
from .measures import measures

BANDS = (
    "fleet:candidate", "fleet:refined", "status:ready", "status:in-progress",
    "status:in-review", "status:done", "status:failed", "status:blocked",
    "fleet:needs-decision",
)
STAGES = BANDS
SIZE_HOURS = {"S": 1, "M": 4, "L": 8, "XL": 16}


def timestamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)


def bucket_start(value: datetime, by: str) -> datetime:
    value = value.astimezone(timezone.utc).replace(hour=0, minute=0, second=0, microsecond=0)
    return value - timedelta(days=value.weekday()) if by == "week" else value


def bucket_points(start: datetime, end: datetime, by: str) -> list[datetime]:
    step = timedelta(days=7 if by == "week" else 1)
    point = bucket_start(start, by)
    result = []
    while point <= end:
        result.append(point)
        point += step
    return result


def _band_at(rows: list[sqlite3.Row], point: datetime) -> str | None:
    applied: dict[str, datetime] = {}
    closed = False
    for row in rows:
        at = timestamp(row["at"])
        if at > point:
            break
        if row["kind"] == "label" and row["to_state"]:
            label = row["to_state"]
            if label.startswith("-"):
                applied.pop(label[1:], None)
            else:
                applied[label] = at
        elif row["kind"] == "closed":
            closed = row["to_state"] == "closed"
    statuses = [(at, label) for label, at in applied.items() if label.startswith("status:")]
    fleets = [(at, label) for label, at in applied.items() if label.startswith("fleet:")]
    band = max(statuses or fleets, default=(None, None))[1]
    if closed and band != "status:done":
        return None
    return band if band in BANDS else None


def transition_rows(db: sqlite3.Connection) -> dict[int, list[sqlite3.Row]]:
    rows = db.execute(
        "SELECT issue,at,kind,to_state FROM transitions "
        "WHERE kind IN ('label','closed') ORDER BY issue,at,rowid"
    )
    result: dict[int, list[sqlite3.Row]] = defaultdict(list)
    for row in rows:
        result[row["issue"]].append(row)
    return result


def cumulative_flow(db: sqlite3.Connection, points: list[datetime]) -> dict[str, list[int]]:
    issues = transition_rows(db)
    return {
        band: [sum(_band_at(rows, point) == band for rows in issues.values()) for point in points]
        for band in BANDS
    }


def throughput(db: sqlite3.Connection, start: datetime, end: datetime, by: str) -> list[dict]:
    counts: dict[datetime, int] = defaultdict(int)
    sizes: dict[datetime, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    for row in db.execute(
            "SELECT accepted_at,baseline_equivalent FROM outcomes "
            "WHERE accepted=1 AND accepted_at IS NOT NULL"):
        at = timestamp(row["accepted_at"])
        if start <= at <= end:
            bucket = bucket_start(at, by)
            counts[bucket] += 1
            if row["baseline_equivalent"] in SIZE_HOURS:
                sizes[bucket][row["baseline_equivalent"]] += 1
    tagged = any(sizes.values())
    return [{
        "at": point.isoformat(),
        "accepted": counts[point],
        "baseline_equivalent_hours": (
            sum(sizes[point][size] * hours for size, hours in SIZE_HOURS.items())
            if tagged else None
        ),
        "size_counts": {size: sizes[point][size] for size in SIZE_HOURS},
    } for point in bucket_points(start, end, by)]


def _percentile(values: list[float], percentile: float) -> float | None:
    if not values:
        return None
    values = sorted(values)
    return values[math.ceil(percentile * len(values)) - 1]


def cycle_times(db: sqlite3.Connection, start: datetime, end: datetime, by: str) -> list[dict]:
    durations: dict[tuple[datetime, str], list[float]] = defaultdict(list)
    for rows in transition_rows(db).values():
        boundaries = sorted({timestamp(row["at"]) for row in rows})
        for entered, left in zip(boundaries, boundaries[1:]):
            band = _band_at(rows, entered)
            if band in STAGES and start <= left <= end:
                durations[(bucket_start(left, by), band)].append((left - entered).total_seconds() / 86400)
    result = []
    for (bucket, stage), values in sorted(durations.items()):
        result.append({"at": bucket.isoformat(), "stage": stage,
                       "median_days": statistics.median(values),
                       "p90_days": _percentile(values, .9), "samples": len(values)})
    return result


def report_data(db: sqlite3.Connection, since: int = 90, by: str = "week",
                now: datetime | None = None) -> dict:
    end = (now or datetime.now(timezone.utc)).astimezone(timezone.utc)
    start = end - timedelta(days=since)
    points = bucket_points(start, end, by)
    return {
        "window": {"start": start.isoformat(), "end": end.isoformat(), "bucket": by},
        "cumulative_flow": {"dates": [p.isoformat() for p in points],
                            "bands": cumulative_flow(db, points)},
        "throughput": throughput(db, start, end, by),
        "cycle_times": cycle_times(db, start, end, by),
        "measures": measures(db, start, end),
    }


def _display(value: float | None, interventions: bool = False) -> str:
    if value is None:
        return "n/a (no interventions logged)" if interventions else "n/a"
    return f"{value:.2f}"


def markdown(data: dict) -> str:
    values = data["measures"]
    rows = [
        ("First-pass rate", _display(values["first_pass_rate"])),
        ("Attempts per accepted change", _display(values["attempts_per_accept"])),
        ("Correction ratio", _display(values["correction_ratio"])),
        ("Touch per accepted change (minutes)", _display(values["touch_per_accept_minutes"], True)),
        ("MTBI (hours)", _display(values["mtbi_hours"], True)),
        ("Cost per attempt", _display(values["cost_per_attempt"])),
        ("Cost per accepted change", _display(values["cost_per_accepted_change"])),
    ]
    measure_table = "\n".join(f"| {name} | {value} |" for name, value in rows)
    def work(row: dict) -> str:
        hours = row["baseline_equivalent_hours"]
        return f"{hours} h" if hours is not None else "n/a (no size tags)"

    throughput_table = "\n".join(
        f"| {row['at'][:10]} | {row['accepted']} | {work(row)} |"
        for row in data["throughput"])
    cycle_table = "\n".join(
        f"| {row['at'][:10]} | {row['stage']} | {row['median_days']:.2f} | "
        f"{row['p90_days']:.2f} | {row['samples']} |" for row in data["cycle_times"])
    window = data["window"]
    return (f"# Factor-q metrics report\n\nWindow: `{window['start']}` through `{window['end']}` "
            f"({window['bucket']} buckets).\n\n## Derived measures and cost\n\n"
            f"| Measure | Value |\n|---|---:|\n{measure_table}\n\n"
            "## Cumulative flow\n\n![Cumulative flow](cumulative-flow.svg)\n\n"
            "## Throughput\n\n| Bucket | Accepted | Baseline-equivalent work |\n"
            "|---|---:|---:|\n"
            f"{throughput_table}\n\n![Accepted changes](throughput.svg)\n\n"
            "## Stage cycle time\n\n| Bucket | Stage | Median days | P90 days | N |\n"
            f"|---|---|---:|---:|---:|\n{cycle_table}\n\n"
            "![Stage cycle time](cycle-times.svg)\n")


def write_report(db_path: str | Path, output_dir: str | Path, since: int = 90,
                 by: str = "week", now: datetime | None = None) -> dict:
    output = Path(output_dir)
    output.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(db_path)
    db.row_factory = sqlite3.Row
    try:
        data = report_data(db, since, by, now)
    finally:
        db.close()
    (output / "report.json").write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    flow = data["cumulative_flow"]
    svg.stacked_area(output / "cumulative-flow.svg", flow["dates"], flow["bands"], "Issues by state")
    svg.line_chart(output / "throughput.svg", [r["at"] for r in data["throughput"]],
                   {"accepted": [r["accepted"] for r in data["throughput"]]}, "Accepted changes")
    cycle: dict[str, list[float | None]] = {stage: [] for stage in STAGES}
    dates = sorted({row["at"] for row in data["cycle_times"]})
    lookup = {(row["at"], row["stage"]): row["median_days"] for row in data["cycle_times"]}
    for stage in STAGES:
        cycle[stage] = [lookup.get((date, stage)) for date in dates]
    svg.line_chart(output / "cycle-times.svg", dates, cycle, "Median stage cycle time (days)")
    (output / "report.md").write_text(markdown(data), encoding="utf-8")
    return data
