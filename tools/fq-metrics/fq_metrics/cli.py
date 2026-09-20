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


def import_interventions(db: sqlite3.Connection, repo_path: Path) -> int:
    path = repo_path / "metrics" / "interventions.csv"
    text: str | None = None
    if path.exists():
        text = path.read_text(encoding="utf-8")
    else:
        result = subprocess.run(
            ["git", "-C", str(repo_path), "show", "origin/metrics:metrics/interventions.csv"],
            text=True, capture_output=True,
        )
        if result.returncode == 0:
            text = result.stdout
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
    try:
        exported = events.read_export(args.events) if args.events else events.edge_events(args.since)
        events.import_events(db, exported)
        if not args.no_github:
            github.collect(db, args.repo, args.since)
        import_interventions(db, repo_path)
        git_history.extract(db, repo_path, ref=args.git_ref)
        views = Path(__file__).resolve().parent.parent / "views.sql"
        db.executescript(views.read_text(encoding="utf-8"))
        db.commit()
    finally:
        db.close()
    print(f"wrote {args.output}")


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
