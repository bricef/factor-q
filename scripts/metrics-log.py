#!/usr/bin/env python3
"""Append one row to the measurement instrument's human-entered logs.

The capability ladder (docs/reviews/2026-07-07-25-factor-q-capability-ladder.md
§2) derives every measure from five primitives; two of them, typed
interventions and touch minutes, must be entered by a human, and the M0 plan
adds one labelled judgement per accepted change, the baseline-equivalent
size. This script is the whole data-entry surface for those three: it
validates the vocabulary, stamps UTC time, appends a CSV row, and commits it.

    scripts/metrics-log.py touch <issue> <minutes> <type> [note...]
    scripts/metrics-log.py tag   <pr>    <size>    [--issue N] [note...]

The data lives on the orphan branch `metrics` (no shared history with
`main`, never merged), so a row never triggers `main`'s CI or the hourly
deploy. This script keeps a worktree of that branch at `.metrics-data/` in
the repository root (gitignored), fast-forwards it, appends the row, commits
with a fixed message form (`log: fix #838 15m`), and pushes. `--no-push`
commits locally only; `--no-commit` appends without committing (for dry
runs).

Types (docs/guide/measurement-logs.md; rubric to land under #840):

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
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DATA_BRANCH = "metrics"
DATA_DIR = ROOT / ".metrics-data"
INTERVENTIONS = pathlib.Path("metrics/interventions.csv")
ACCEPTED = pathlib.Path("metrics/accepted.csv")

INTERVENTION_TYPES = ("unblock", "clarify", "respec", "fix", "restart", "retune")
SIZES = ("S", "M", "L", "XL")
SOURCES = ("human", "agent")

INTERVENTIONS_HEADER = ["at", "issue", "type", "minutes", "note", "source"]
ACCEPTED_HEADER = ["at", "pr", "issue", "baseline_equivalent", "note", "source"]

TRAILER = (
    "\n\nCo-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
    "\nClaude-Session: https://claude.ai/code/session_01UDSPzDra9TpkKDuWf88uEo"
)


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


def git(*args: str, cwd: pathlib.Path = ROOT, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", "-C", str(cwd), *args], text=True, capture_output=True, check=check)


def ensure_data_worktree(data_dir: pathlib.Path) -> None:
    """Make `data_dir` a fast-forwarded worktree of the data branch.

    The branch is orphan and append-only, so `pull --ff-only` is the only
    integration ever needed; a non-fast-forward means someone else appended
    since we last fetched, and the caller's retry handles it.
    """
    if not (data_dir / ".git").exists():
        git("fetch", "--quiet", "origin", f"{DATA_BRANCH}:refs/remotes/origin/{DATA_BRANCH}")
        git("worktree", "add", "--quiet", "-B", DATA_BRANCH, str(data_dir), f"origin/{DATA_BRANCH}")
    else:
        git("fetch", "--quiet", "origin", DATA_BRANCH, cwd=data_dir)
        git("merge", "--ff-only", "--quiet", f"origin/{DATA_BRANCH}", cwd=data_dir)


def append_row(path: pathlib.Path, header: list[str], row: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fresh = not path.exists() or path.stat().st_size == 0
    with path.open("a", newline="") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        if fresh:
            writer.writerow(header)
        writer.writerow(row)


def commit_and_push(data_dir: pathlib.Path, rel: pathlib.Path, message: str, push: bool) -> str:
    git("add", str(rel), cwd=data_dir)
    git("commit", "--quiet", "-m", message + TRAILER, cwd=data_dir)
    sha = git("rev-parse", "--short", "HEAD", cwd=data_dir).stdout.strip()
    if not push:
        return f"committed {sha} on {DATA_BRANCH} (not pushed)"
    pushed = git("push", "--quiet", "origin", f"HEAD:{DATA_BRANCH}", cwd=data_dir, check=False)
    if pushed.returncode != 0:
        # Someone appended since our fetch: rebase our one commit onto theirs
        # and push again. The files are append-only, so the rebase is clean
        # unless two rows landed on the same header-only file, in which case
        # the second writer re-runs.
        git("fetch", "--quiet", "origin", DATA_BRANCH, cwd=data_dir)
        rebased = git("rebase", "--quiet", f"origin/{DATA_BRANCH}", cwd=data_dir, check=False)
        if rebased.returncode != 0:
            git("rebase", "--abort", cwd=data_dir, check=False)
            raise SystemExit(
                f"row committed as {sha} on {DATA_BRANCH} but the push conflicted with a concurrent "
                f"append; run again to retry (worktree: {data_dir})"
            )
        git("push", "--quiet", "origin", f"HEAD:{DATA_BRANCH}", cwd=data_dir)
        sha = git("rev-parse", "--short", "HEAD", cwd=data_dir).stdout.strip()
    return f"pushed {sha} to origin/{DATA_BRANCH}"


def cmd_touch(args: argparse.Namespace) -> str:
    issue = positive_int(args.issue, "issue")
    minutes = positive_int(args.minutes, "minutes")
    if args.type not in INTERVENTION_TYPES:
        raise SystemExit(
            f"unknown intervention type {args.type!r}; one of {', '.join(INTERVENTION_TYPES)}"
        )
    note = " ".join(args.note).strip()
    at = now_utc()
    row = [at, str(issue), args.type, str(minutes), note, args.source]
    message = f"log: {args.type} #{issue} {minutes}m" + (f" — {note}" if note else "")
    return record(args, INTERVENTIONS, INTERVENTIONS_HEADER, row, message,
                  f"logged {args.type} on #{issue}: {minutes} min ({args.source}) at {at}")


def cmd_tag(args: argparse.Namespace) -> str:
    pr = positive_int(args.pr, "pr")
    size = args.size.upper()
    if size not in SIZES:
        raise SystemExit(f"unknown size {args.size!r}; one of {', '.join(SIZES)}")
    issue = str(positive_int(args.issue, "issue")) if args.issue else ""
    note = " ".join(args.note).strip()
    at = now_utc()
    row = [at, str(pr), issue, size, note, args.source]
    target = f"PR #{pr}" + (f" (issue #{issue})" if issue else "")
    message = f"tag: {size} PR #{pr}" + (f" #{issue}" if issue else "") + (f" — {note}" if note else "")
    return record(args, ACCEPTED, ACCEPTED_HEADER, row, message,
                  f"tagged {target} as {size} ({args.source}) at {at}")


def record(args: argparse.Namespace, rel: pathlib.Path, header: list[str], row: list[str],
           message: str, summary: str) -> str:
    data_dir = pathlib.Path(args.data_dir)
    if args.no_commit:
        append_row(data_dir / rel, header, row)
        return f"{summary}; appended to {data_dir / rel} (not committed)"
    ensure_data_worktree(data_dir)
    append_row(data_dir / rel, header, row)
    outcome = commit_and_push(data_dir, rel, message, push=not args.no_push)
    return f"{summary}; {outcome}"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="metrics-log", description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--source", choices=SOURCES, default="human",
        help="who entered the row (default human; agent = a coordinating agent on the maintainer's behalf)",
    )
    parser.add_argument("--data-dir", default=str(DATA_DIR),
                        help=f"worktree of the `{DATA_BRANCH}` branch (default {DATA_DIR})")
    parser.add_argument("--no-push", action="store_true", help="commit locally, do not push")
    parser.add_argument("--no-commit", action="store_true",
                        help="append to the file only; no worktree, commit or push (dry run)")
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
    tag.add_argument("note", nargs="*")
    tag.set_defaults(run=cmd_tag)

    # The `just` recipes append every extra word after the positionals, so
    # `--source agent`, `--no-push`, `--data-dir X` and `--issue N` all arrive
    # *after* the subcommand and its trailing nargs="*" note. argparse cannot
    # intermix options there, so lift the known ones out by hand and hand the
    # global ones back to the parser in front of the subcommand.
    argv = list(argv)
    lifted_front: list[str] = []
    issue = None
    valued = {"--source", "--data-dir", "--issue"}
    flags = {"--no-push", "--no-commit"}
    i = 0
    while i < len(argv):
        tok = argv[i]
        if tok in valued:
            if i + 1 >= len(argv):
                raise SystemExit(f"{tok} needs a value")
            if tok == "--issue":
                issue = argv[i + 1]
            else:
                lifted_front += [tok, argv[i + 1]]
            del argv[i : i + 2]
            continue
        if tok in flags:
            lifted_front.append(tok)
            del argv[i]
            continue
        i += 1
    args = parser.parse_args(lifted_front + argv)
    args.issue = getattr(args, "issue", None) or issue
    print(args.run(args))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
