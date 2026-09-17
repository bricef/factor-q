#!/usr/bin/env python3
"""Append one row to the measurement instrument's human-entered logs.

The capability ladder (docs/reviews/2026-07-07-25-factor-q-capability-ladder.md
§2) derives every measure from five primitives; two of them, typed
interventions and touch minutes, must be entered by a human, and the M0 plan
adds one labelled judgement per accepted change, the baseline-equivalent
size. This script is the whole data-entry surface for those three: it
validates the vocabulary, stamps UTC time, and appends a CSV row. Nothing
else. The extractor in tools/fq-metrics (#838) reads the files back.

    scripts/metrics-log.py touch <issue> <minutes> <type> [note...]
    scripts/metrics-log.py tag   <pr>    <size>    [note...]

Types (docs/plans/active/2026-09-17-measurement-instrument.md §4, to land as
docs/guide/measurement-rubric.md under #840):

    unblock  the run could not proceed without a human act (a rebase, a
             re-run of a flaky job, a dependency the agent could not install)
    clarify  answering a question or adding missing context after dispatch
    respec   changing the acceptance criteria after dispatch (ends the attempt)
    fix      a human or a non-fleet agent changed the code the fleet produced
    restart  re-dispatch after a failed or abandoned run
    retune   changing the agent definition, prompt or config because of this
             attempt

Sizes: S (a skilled human driving a frontier model, under an hour), M (up to
half a day), L (up to a day), XL (more).

Minutes are recorded as given; the rubric asks for them rounded UP to the
next five, context switch included, so round before you type. `--source
agent` marks a row a coordinating agent entered on the maintainer's behalf,
so it can be excluded or audited; the default is `human`.

The files are append-only CSV under metrics/ with a header row; the script
creates them if absent and never rewrites existing rows.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
INTERVENTIONS = ROOT / "metrics" / "interventions.csv"
ACCEPTED = ROOT / "metrics" / "accepted.csv"

INTERVENTION_TYPES = ("unblock", "clarify", "respec", "fix", "restart", "retune")
SIZES = ("S", "M", "L", "XL")
SOURCES = ("human", "agent")

INTERVENTIONS_HEADER = ["at", "issue", "type", "minutes", "note", "source"]
ACCEPTED_HEADER = ["at", "pr", "issue", "baseline_equivalent", "note", "source"]


def now_utc() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def positive_int(text: str, what: str) -> int:
    try:
        value = int(text)
    except ValueError:
        raise SystemExit(f"{what} must be an integer, got {text!r}")
    if value <= 0:
        raise SystemExit(f"{what} must be positive, got {value}")
    return value


def append_row(path: pathlib.Path, header: list[str], row: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fresh = not path.exists() or path.stat().st_size == 0
    with path.open("a", newline="") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        if fresh:
            writer.writerow(header)
        writer.writerow(row)


def cmd_touch(args: argparse.Namespace) -> str:
    issue = positive_int(args.issue, "issue")
    minutes = positive_int(args.minutes, "minutes")
    if args.type not in INTERVENTION_TYPES:
        raise SystemExit(
            f"unknown intervention type {args.type!r}; one of {', '.join(INTERVENTION_TYPES)}"
        )
    note = " ".join(args.note).strip()
    at = now_utc()
    append_row(
        INTERVENTIONS,
        INTERVENTIONS_HEADER,
        [at, str(issue), args.type, str(minutes), note, args.source],
    )
    return f"logged {args.type} on #{issue}: {minutes} min ({args.source}) at {at}"


def cmd_tag(args: argparse.Namespace) -> str:
    pr = positive_int(args.pr, "pr")
    size = args.size.upper()
    if size not in SIZES:
        raise SystemExit(f"unknown size {args.size!r}; one of {', '.join(SIZES)}")
    issue = str(positive_int(args.issue, "issue")) if args.issue else ""
    note = " ".join(args.note).strip()
    at = now_utc()
    append_row(ACCEPTED, ACCEPTED_HEADER, [at, str(pr), issue, size, note, args.source])
    target = f"PR #{pr}" + (f" (issue #{issue})" if issue else "")
    return f"tagged {target} as {size} ({args.source}) at {at}"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="metrics-log", description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--source",
        choices=SOURCES,
        default="human",
        help="who entered the row (default human; agent = a coordinating agent on the maintainer's behalf)",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    touch = sub.add_parser("touch", help="log an intervention and its touch minutes against an issue")
    touch.add_argument("issue")
    touch.add_argument("minutes")
    touch.add_argument("type")
    touch.add_argument("note", nargs="*")
    touch.set_defaults(run=cmd_touch)

    tag = sub.add_parser("tag", help="tag an accepted change with its baseline-equivalent size")
    tag.add_argument("pr")
    tag.add_argument("size")
    tag.add_argument("--issue", default=None, help="the issue the PR closed, if known")
    tag.add_argument("note", nargs="*")
    tag.set_defaults(run=cmd_tag)

    # Intermixed: `tag 837 M --issue 798 a note` puts the option after the
    # positionals, which plain parse_args refuses once `note` (nargs="*") has
    # matched nothing.
    args = parser.parse_intermixed_args(argv)
    print(args.run(args))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
