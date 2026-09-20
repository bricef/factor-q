"""Command-line entry point for fq-metrics."""

from __future__ import annotations

import argparse
import csv
import sqlite3
import subprocess
import sys
from pathlib import Path

from . import events, git_history, github, preflight, report as metrics_report
from .db import connect, upsert


def _log_text(repo_path: Path, name: str) -> str | None:
    path = repo_path / "metrics" / name
    if path.exists():
        return path.read_text(encoding="utf-8")
    result = subprocess.run(
        ["git", "-C", str(repo_path), "show", f"origin/metrics:metrics/{name}"],
        text=True, capture_output=True,
    )
    return result.stdout if result.returncode == 0 else None


def import_interventions(db: sqlite3.Connection, repo_path: Path) -> int:
    text = _log_text(repo_path, "interventions.csv")
    if not text:
        return 0
    rows = list(csv.DictReader(text.splitlines()))
    for row in rows:
        upsert(db, "interventions", {
            "at": row["at"], "issue": int(row["issue"]), "type": row["type"],
            "minutes": int(row["minutes"]) if row.get("minutes") else None,
            "note": row.get("note"), "source": row.get("source"),
        }, ("at", "issue", "type", "note"))
    return len(rows)


def import_accepted(db: sqlite3.Connection, repo_path: Path) -> tuple[int, int]:
    text = _log_text(repo_path, "accepted.csv")
    rows = list(csv.DictReader(text.splitlines())) if text else []
    for row in rows:
        size = row["baseline_equivalent"].upper()
        if size not in ("S", "M", "L", "XL"):
            raise ValueError(f"unknown baseline-equivalent size {size!r}")
        upsert(db, "size_tags", {
            "pr_number": int(row["pr"]), "at": row["at"],
            "issue": int(row["issue"]) if row.get("issue") else None,
            "baseline_equivalent": size, "note": row.get("note"),
            "source": row.get("source"),
        }, ("pr_number",))

    for tag in db.execute("SELECT * FROM size_tags").fetchall():
        if tag["issue"] is None:
            cursor = db.execute(
                "UPDATE outcomes SET baseline_equivalent=? WHERE pr_number=?",
                (tag["baseline_equivalent"], tag["pr_number"]),
            )
        else:
            cursor = db.execute(
                "UPDATE outcomes SET baseline_equivalent=? WHERE pr_number=? AND issue=?",
                (tag["baseline_equivalent"], tag["pr_number"], tag["issue"]),
            )
        if cursor.rowcount:
            db.execute("DELETE FROM size_tags WHERE pr_number=?", (tag["pr_number"],))
    pending = db.execute("SELECT COUNT(*) FROM size_tags").fetchone()[0]
    return len(rows), pending


def extract(args: argparse.Namespace) -> None:
    repo_path = Path(args.repo_path).resolve()
    if args.events:
        preflight.check_export(args.events)
    else:
        preflight.check_fq()
    if not args.no_github:
        preflight.check_gh()
    preflight.check_git(repo_path, args.git_ref)
    db = connect(args.output)
    interventions = tags = pending = 0
    try:
        exported = events.read_export(args.events) if args.events else events.edge_events(args.since)
        events.import_events(db, exported)
        if not args.no_github:
            github.collect(db, args.repo, args.since)
        interventions = import_interventions(db, repo_path)
        git_history.extract(db, repo_path, ref=args.git_ref)
        tags, pending = import_accepted(db, repo_path)
        views = Path(__file__).resolve().parent.parent / "views.sql"
        db.executescript(views.read_text(encoding="utf-8"))
        db.commit()
    finally:
        db.close()
    print(f"wrote {args.output}: {interventions} interventions, {tags} size tags ({pending} pending)")


def report(args: argparse.Namespace) -> None:
    preflight.check_ledger(args.ledger)
    metrics_report.write_report(args.ledger, args.output_dir, args.since, args.by)
    print(f"wrote {Path(args.output_dir) / 'report.md'}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(prog="fq-metrics")
    commands = result.add_subparsers(dest="command", required=True)
    command = commands.add_parser("extract", help="build or update the attempt ledger")
    command.add_argument("--repo", default="bricef/factor-q", help="GitHub owner/repository")
    command.add_argument("--repo-path", default=".", help="local git checkout")
    command.add_argument("--git-ref", default="main", help="history ref to inspect")
    command.add_argument("--output", default="attempt_ledger.sqlite")
    command.add_argument("--since", help="ISO-8601 lower bound for GitHub and edge calls")
    command.add_argument("--events", metavar="DIR", help="read an exported JSON tree instead of the edge")
    command.add_argument("--no-github", action="store_true", help=argparse.SUPPRESS)
    command.set_defaults(func=extract)
    command = commands.add_parser("report", help="render metrics from an attempt ledger")
    command.add_argument("--ledger", default="attempt_ledger.sqlite")
    command.add_argument("--since", type=int, default=90, metavar="DAYS")
    command.add_argument("--by", choices=("week", "day"), default="week")
    command.add_argument("--output-dir", default="target/metrics/")
    command.set_defaults(func=report)
    return result


def main() -> None:
    args = parser().parse_args()
    try:
        args.func(args)
    except preflight.ToolError as error:
        sys.exit(str(error))
    except subprocess.CalledProcessError as error:
        sys.exit(str(preflight.describe_failure(error)))
    except KeyboardInterrupt:
        sys.exit(130)
