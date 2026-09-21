#!/usr/bin/env python3
"""Replay the github-watcher's merge verdict over already-merged PRs.

The merge verdict (https://github.com/bricef/factor-q/issues/879) is
advisory and off by default. Before it is trusted with anything, it has to
be calibrated: what would it have said about the PRs that a human actually
merged, and does the tier it assigns line up with the ground-truth signal
available today — whether the PR needed rework (more than one commit)?

    scripts/merge-verdicts-replay.py --since 2026-09-12 [--limit N] [-o out.csv]

One CSV row per merged PR goes to stdout; a summary goes to stderr.

**The verdict is not reimplemented here.** This script gathers facts and
formats a table; the verdict itself comes from `github-watcher verdict`,
the JSON-Lines seam over the same `Verdict` function the live sweep runs
(adapters/github-watcher/verdictcmd.go). A replay that measured a Python
copy of the rules would measure something that merely agrees today. The
script builds the binary with `go build` unless `--binary` names one.

Nothing is written to GitHub: the only calls are reads through `gh api
graphql`.

Two checks read differently in replay than they do live, and the CSV
keeps both rather than hiding either:

  mergeable       GitHub reports `mergeable: UNKNOWN` for a merged PR, so
                  this check fails for essentially every row. It measures
                  nothing here; it is kept so the column set matches the
                  live comment.
  closing-issue   A merged PR's issue carries `status:done` today, not
                  `status:in-review`. The watcher's state machine only
                  reaches `done` *through* `in-review` (watcher.go's
                  review sweep is the only writer of `done`), so by
                  default a closing issue labelled `status:done` is
                  replayed as having been `status:in-review`. Pass
                  `--raw-labels` to switch that off, and read
                  `closing_issue_labels` for what the issue carries now.

`--rubric` adds one column per question of the Jev rubric
(`.github/merge-rubric.yml`) plus `rubric_flagged`, scored through the same
`verdict` subcommand. It needs `TYPESAFE_API_KEY` and makes one paid API
call per PR, so it is off unless asked for.

Stdlib only, like the rest of scripts/: the fleet image has python3 and no
pip.
"""

from __future__ import annotations

import argparse
import collections
import csv
import json
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
WATCHER_DIR = ROOT / "adapters" / "github-watcher"
AREAS = ROOT / ".github" / "areas.yml"
POLICY = ROOT / ".github" / "merge-policy.yml"
RUBRIC = ROOT / ".github" / "merge-rubric.yml"
IN_REVIEW = "status:in-review"
DONE = "status:done"
# The checks, in the order adapters/github-watcher/verdict.go reports
# them. They become one CSV column each, so this list is the column
# contract: renaming one invalidates comparisons with earlier runs.
CHECKS = [
    "provenance",
    "closing-issue",
    "mergeable",
    "single-commit",
    "no-changes-requested",
    "no-hold-label",
    "min-age",
]

COLUMNS = (
    ["number", "merged_at", "head_branch", "fleet", "provenance_form", "tier", "rule", "areas", "file_count", "files_truncated"]
    + [f"check_{name.replace('-', '_')}" for name in CHECKS]
    + ["commit_count", "reworked", "closing_issues", "closing_issue_labels", "additions", "deletions", "files"]
)


def rubric_columns(rubric: pathlib.Path) -> list[str]:
    """One column per rubric question, named from the file, not from code."""
    ids, in_questions = [], False
    for line in rubric.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("questions:"):
            in_questions = True
        elif in_questions and stripped.startswith("- id:"):
            ids.append(stripped.split(":", 1)[1].strip())
    return [f"rubric_{i}" for i in ids] + ["rubric_flagged"]

SEARCH_QUERY = """query($q:String!,$cursor:String){
  search(query:$q, type:ISSUE, first:50, after:$cursor){
    pageInfo{ hasNextPage endCursor }
    nodes{ ... on PullRequest {
      number body mergedAt createdAt headRefName baseRefName headRefOid
      additions deletions mergeable reviewDecision
      commits(first:1){ totalCount }
      files(first:100){ totalCount nodes{ path } }
      labels(first:50){ nodes{ name } }
      closingIssuesReferences(first:10){ nodes{ number labels(first:50){ nodes{ name } } } }
    } }
  }
}"""

# GitHub's `mergeable` enum against the REST `mergeable_state` vocabulary
# the verdict's check speaks. UNKNOWN stays unknown rather than being
# guessed either way.
MERGEABLE_STATE = {"MERGEABLE": "clean", "CONFLICTING": "dirty", "UNKNOWN": "unknown"}


def gh_graphql(query: str, variables: dict) -> dict:
    """POST one GraphQL query through `gh api` and return `data`."""
    body = json.dumps({"query": query, "variables": variables})
    proc = subprocess.run(
        ["gh", "api", "graphql", "--input", "-"],
        input=body, capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"gh api graphql failed: {proc.stderr.strip()}")
    payload = json.loads(proc.stdout)
    if payload.get("errors"):
        messages = "; ".join(e.get("message", "?") for e in payload["errors"])
        sys.exit(f"GitHub GraphQL errors: {messages}")
    return payload["data"]


def merged_prs(repo: str, since: str, limit: int | None) -> list[dict]:
    """Every PR merged into `repo` on or after `since`, oldest first."""
    search = f"repo:{repo} is:pr is:merged merged:>={since} sort:created-asc"
    cursor, out = None, []
    while True:
        page = gh_graphql(SEARCH_QUERY, {"q": search, "cursor": cursor})["search"]
        out.extend(node for node in page["nodes"] if node)
        if limit is not None and len(out) >= limit:
            return out[:limit]
        if not page["pageInfo"]["hasNextPage"]:
            return out
        cursor = page["pageInfo"]["endCursor"]


def closing_issues(pr: dict, raw_labels: bool) -> list[dict]:
    """The issues this PR closes, with the labels to judge them by.

    See the module docstring: `status:done` on a merged PR's issue is
    evidence the issue passed through `status:in-review`, because the
    watcher's review sweep is the only thing that writes `done`.
    """
    issues = []
    for node in pr["closingIssuesReferences"]["nodes"]:
        labels = [label["name"] for label in node["labels"]["nodes"]]
        if not raw_labels and DONE in labels and IN_REVIEW not in labels:
            labels = labels + [IN_REVIEW]
        issues.append({"number": node["number"], "labels": labels})
    return issues


def facts_for(pr: dict, raw_labels: bool) -> dict:
    """The PRFacts the `verdict` seam reads, for one merged PR.

    `observed_at` is the merge time, so the `min-age` check answers "had a
    human had time to veto before this merged?" rather than "is it old
    now?", which every merged PR would pass.
    """
    return {
        "number": pr["number"],
        "head_sha": pr["headRefOid"],
        "base_ref": pr["baseRefName"],
        "body": pr["body"] or "",
        "files": [f["path"] for f in pr["files"]["nodes"]],
        "commit_count": pr["commits"]["totalCount"],
        "labels": [label["name"] for label in pr["labels"]["nodes"]],
        "mergeable_state": MERGEABLE_STATE.get(pr["mergeable"], "unknown"),
        "changes_requested": pr["reviewDecision"] == "CHANGES_REQUESTED",
        "created_at": pr["createdAt"],
        "observed_at": pr["mergedAt"],
        "closing_issues": closing_issues(pr, raw_labels),
    }


def build_watcher(into: pathlib.Path) -> pathlib.Path:
    """Build the watcher so the replay runs the code that is checked out."""
    binary = into / "github-watcher"
    proc = subprocess.run(
        ["go", "build", "-o", str(binary), "."],
        cwd=WATCHER_DIR, capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"go build failed: {proc.stderr.strip()}")
    return binary


def verdicts(binary: pathlib.Path, facts: list[dict], areas: pathlib.Path, policy: pathlib.Path,
             rubric: pathlib.Path | None) -> dict[int, dict]:
    """Run every PR's facts through the seam in one batch, keyed by number."""
    stdin = "".join(json.dumps(f) + "\n" for f in facts)
    command = [str(binary), "verdict", "--areas", str(areas), "--policy", str(policy)]
    if rubric is not None:
        command += ["--rubric", str(rubric)]
    proc = subprocess.run(
        command,
        input=stdin, capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"github-watcher verdict failed: {proc.stderr.strip()}")
    out = {}
    for line in proc.stdout.splitlines():
        if not line.strip():
            continue
        result = json.loads(line)
        if result.get("error"):
            sys.exit(f"github-watcher verdict rejected a row: {result['error']}")
        out[result["number"]] = result
    return out


def row_for(pr: dict, facts: dict, verdict: dict) -> dict:
    """One CSV row: what was merged, what the verdict says, and the rework."""
    checks = {c["name"]: c["pass"] for c in verdict["checks"]}
    areas = sorted({area for f in verdict["files"] for area in f.get("areas", [])})
    issues = facts["closing_issues"]
    row = {
        "number": pr["number"],
        "merged_at": pr["mergedAt"],
        "head_branch": pr["headRefName"],
        "fleet": verdict["provenance_form"] != "none",
        "provenance_form": verdict["provenance_form"],
        "tier": verdict["tier"],
        "rule": verdict["rule"],
        "areas": " ".join(areas),
        "file_count": pr["files"]["totalCount"],
        "files_truncated": pr["files"]["totalCount"] > len(facts["files"]),
        "commit_count": facts["commit_count"],
        "reworked": facts["commit_count"] > 1,
        "closing_issues": " ".join(f"#{i['number']}" for i in issues),
        "closing_issue_labels": " ".join(
            label for node in pr["closingIssuesReferences"]["nodes"] for label in
            (l["name"] for l in node["labels"]["nodes"])
        ),
        "additions": pr["additions"],
        "deletions": pr["deletions"],
        "files": " ".join(facts["files"]),
    }
    for name in CHECKS:
        row[f"check_{name.replace('-', '_')}"] = checks.get(name, "")
    scored = verdict.get("rubric")
    if scored:
        for answer in scored.get("answers") or []:
            row[f"rubric_{answer['id']}"] = round(answer["probability"], 4)
        row["rubric_flagged"] = scored.get("flagged", "")
        if not scored.get("answers"):
            row["rubric_flagged"] = scored.get("reason", "")
    return row


def summarise(rows: list[dict], out) -> None:
    """PRs per tier and reworked per tier — the calibration question."""
    per_tier = collections.Counter(r["tier"] for r in rows)
    reworked = collections.Counter(r["tier"] for r in rows if r["reworked"])
    fleet = collections.Counter(r["tier"] for r in rows if r["fleet"])
    print(f"\n{len(rows)} merged PRs ({sum(fleet.values())} fleet)", file=out)
    print("\n| tier | PRs | fleet PRs | reworked | reworked % |", file=out)
    print("|---|---|---|---|---|", file=out)
    for tier in ("unsupervised", "supervised", "never"):
        total = per_tier[tier]
        pct = f"{100 * reworked[tier] / total:.0f}%" if total else "—"
        print(f"| `{tier}` | {total} | {fleet[tier]} | {reworked[tier]} | {pct} |", file=out)
    print("\n| structural check | passed | of | pass % |", file=out)
    print("|---|---|---|---|", file=out)
    for name in CHECKS:
        column = f"check_{name.replace('-', '_')}"
        passed = sum(1 for r in rows if r[column] is True)
        pct = f"{100 * passed / len(rows):.0f}%" if rows else "—"
        print(f"| `{name}` | {passed} | {len(rows)} | {pct} |", file=out)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", default="bricef/factor-q", help="owner/name (default: %(default)s)")
    parser.add_argument("--since", required=True, metavar="YYYY-MM-DD", help="replay PRs merged on or after this date")
    parser.add_argument("--limit", type=int, help="stop after N PRs")
    parser.add_argument("--binary", type=pathlib.Path, help="an existing github-watcher binary (default: build one)")
    parser.add_argument("--areas", type=pathlib.Path, default=AREAS, help=argparse.SUPPRESS)
    parser.add_argument("--policy", type=pathlib.Path, default=POLICY, help=argparse.SUPPRESS)
    parser.add_argument("--raw-labels", action="store_true", help="do not replay status:done as status:in-review")
    parser.add_argument(
        "--rubric", nargs="?", const=RUBRIC, type=pathlib.Path, default=None,
        help=f"also score the Jev rubric, adding one column per question (needs TYPESAFE_API_KEY; default file: {RUBRIC.name})",
    )
    parser.add_argument("-o", "--out", type=pathlib.Path, help="write the CSV here instead of stdout")
    args = parser.parse_args(argv)

    prs = merged_prs(args.repo, args.since, args.limit)
    if not prs:
        sys.exit(f"no PRs merged in {args.repo} since {args.since}")
    facts = [facts_for(pr, args.raw_labels) for pr in prs]

    with tempfile.TemporaryDirectory() as tmp:
        binary = args.binary or build_watcher(pathlib.Path(tmp))
        scored = verdicts(binary, facts, args.areas, args.policy, args.rubric)

    rows = [row_for(pr, f, scored[pr["number"]]) for pr, f in zip(prs, facts) if pr["number"] in scored]
    columns = COLUMNS + (rubric_columns(args.rubric) if args.rubric else [])
    handle = open(args.out, "w", newline="", encoding="utf-8") if args.out else sys.stdout
    try:
        writer = csv.DictWriter(handle, fieldnames=columns, restval="")
        writer.writeheader()
        writer.writerows(rows)
    finally:
        if args.out:
            handle.close()
    summarise(rows, sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
