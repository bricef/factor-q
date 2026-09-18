"""GitHub collection and conversion to ledger rows."""

from __future__ import annotations

import json
import re
import sqlite3
import subprocess
from datetime import datetime
from typing import Any

from .db import transition, upsert

PROVENANCE = re.compile(
    r"(?m)^provenance:\s*agent=(\S+)\s+invocation=(\S+)\s+model=(\S+)\s*$"
)
CLOSES = re.compile(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#(\d+)")
TRACKED = ("status:", "fleet:")


def gh_json(endpoint: str, *fields: str, paginate: bool = False) -> Any:
    command = ["gh", "api", "-X", "GET", endpoint]
    for field in fields:
        command.extend(["-f", field])
    if paginate:
        command.extend(["--paginate", "--slurp"])
    result = subprocess.run(command, check=True, text=True, capture_output=True)
    data = json.loads(result.stdout)
    if paginate:
        return [item for page in data for item in page]
    return data


def actor(event: dict) -> str | None:
    value = event.get("actor") or event.get("user")
    return value.get("login") if isinstance(value, dict) else value


def label_name(event: dict) -> str | None:
    value = event.get("label")
    return value.get("name") if isinstance(value, dict) else value


def pr_number_from_xref(event: dict) -> int | None:
    source = event.get("source") or {}
    issue = source.get("issue") or {}
    if issue.get("pull_request"):
        return issue.get("number")
    return None


def record_issue(db: sqlite3.Connection, issue: dict, timeline: list[dict]) -> dict[int, int]:
    number = int(issue["number"])
    status: str | None = None
    admitted: str | None = None
    closed_at = issue.get("closed_at")
    pr_issues: dict[int, int] = {}
    for event in sorted(timeline, key=lambda row: row.get("created_at") or ""):
        kind = event.get("event")
        at = event.get("created_at")
        if not at:
            continue
        if kind in ("labeled", "unlabeled"):
            label = label_name(event)
            if not label or not label.startswith(TRACKED):
                continue
            old = status
            if kind == "labeled" and label.startswith("status:"):
                status = label.removeprefix("status:")
                if label == "status:ready" and admitted is None:
                    admitted = at
            elif kind == "unlabeled" and label.startswith("status:") and status == label[7:]:
                status = None
            transition(db, issue=number, at=at, kind="label", from_state=old,
                       to_state=(label if kind == "labeled" else f"-{label}"),
                       actor=actor(event), ref=f"{kind}:{label}")
        elif kind in ("closed", "reopened"):
            transition(db, issue=number, at=at, kind="closed", from_state=status,
                       to_state=kind, actor=actor(event), ref=kind)
            if kind == "reopened":
                closed_at = None
        elif kind == "cross-referenced":
            pr = pr_number_from_xref(event)
            if pr:
                pr_issues[pr] = number
                transition(db, issue=number, at=at, kind="pr_opened",
                           from_state=status, to_state=status, actor=actor(event),
                           ref=f"pr:{pr}")
    labels = [row.get("name", "") for row in issue.get("labels", [])]
    type_label = next((label for label in labels if label.startswith("type:")), None)
    if type_label is None:
        type_label = next((label for label in labels
                           if label in {"bug", "enhancement", "documentation"}), None)
    current = next((label[7:] for label in labels if label.startswith("status:")), status)
    upsert(db, "tasks", {
        "issue": number, "title": issue.get("title"), "type_label": type_label,
        "created_at": issue.get("created_at"), "admitted_at": admitted,
        # Timelines do not expose historical body contents; do not substitute today's body.
        "admission_body_hash": None, "closed_at": closed_at, "current_status": current,
    }, ("issue",))
    return pr_issues


def issue_from_body(body: str | None) -> int | None:
    match = CLOSES.search(body or "")
    return int(match.group(1)) if match else None


def record_pr(db: sqlite3.Connection, pr: dict, issue: int | None,
              reviews: list[dict], comments: list[dict], commits: list[dict]) -> None:
    number = int(pr["number"])
    body = pr.get("body") or ""
    provenance = PROVENANCE.search(body)
    branch = (pr.get("head") or {}).get("ref") or pr.get("head_branch") or ""
    issue = issue or issue_from_body(body)
    upsert(db, "pull_requests", {
        "number": number, "issue": issue, "created_at": pr.get("created_at"),
        "merged_at": pr.get("merged_at"), "closed_at": pr.get("closed_at"),
        "head_branch": branch, "body": body,
        "agent_authored": int(bool(provenance or branch.startswith("m0/"))),
        "provenance_invocation": provenance.group(2) if provenance else None,
        "additions": pr.get("additions"), "deletions": pr.get("deletions"),
    }, ("number",))
    if provenance:
        db.execute("UPDATE attempts SET pr_number=?, agent=COALESCE(agent, ?), "
                   "model=COALESCE(model, ?) WHERE invocation_id=?",
                   (number, provenance.group(1), provenance.group(3), provenance.group(2)))
    if issue is None:
        return
    transition(db, issue=issue, at=pr["created_at"], kind="pr_opened",
               from_state=None, to_state=None, actor=actor(pr), ref=f"pr:{number}")
    review_rows = [(row.get("submitted_at") or row.get("created_at"), row, "review")
                   for row in reviews]
    review_rows += [(row.get("created_at"), row, "comment") for row in comments]
    review_rows = [row for row in review_rows if row[0]]
    for at, row, source in review_rows:
        transition(db, issue=issue, at=at, kind="review", from_state=None,
                   to_state=row.get("state") or "comment", actor=actor(row),
                   ref=f"pr:{number}:{source}:{row.get('id', '')}")
    first_review = min((row[0] for row in review_rows), default=None)
    if first_review:
        for commit in commits:
            commit_data = commit.get("commit") or {}
            at = ((commit_data.get("committer") or {}).get("date") or
                  (commit_data.get("author") or {}).get("date"))
            if at and at > first_review:
                transition(db, issue=issue, at=at, kind="push_after_review",
                           from_state=None, to_state=None,
                           actor=((commit.get("author") or {}).get("login")),
                           ref=f"pr:{number}:commit:{commit.get('sha', '')}")
    if pr.get("merged_at"):
        transition(db, issue=issue, at=pr["merged_at"], kind="merged",
                   from_state=None, to_state="merged", actor=actor(pr.get("merged_by") or {}),
                   ref=f"pr:{number}")
    elif pr.get("closed_at"):
        transition(db, issue=issue, at=pr["closed_at"], kind="closed",
                   from_state=None, to_state="closed", actor=None, ref=f"pr:{number}")


def collect(db: sqlite3.Connection, repo: str, since: str | None = None) -> None:
    fields = ["state=all", "per_page=100"]
    if since:
        fields.append(f"since={since}")
    items = gh_json(f"repos/{repo}/issues", *fields, paginate=True)
    issues = [item for item in items if "pull_request" not in item]
    pr_issue: dict[int, int] = {}
    for issue in issues:
        timeline = gh_json(f"repos/{repo}/issues/{issue['number']}/timeline",
                           "per_page=100", paginate=True)
        pr_issue.update(record_issue(db, issue, timeline))

    pulls = gh_json(f"repos/{repo}/pulls", "state=all", "per_page=100", paginate=True)
    if since:
        cutoff = since
        pulls = [pr for pr in pulls if (pr.get("updated_at") or "") >= cutoff]
    for summary in pulls:
        number = summary["number"]
        pr = gh_json(f"repos/{repo}/pulls/{number}")
        reviews = gh_json(f"repos/{repo}/pulls/{number}/reviews", "per_page=100", paginate=True)
        comments = gh_json(f"repos/{repo}/pulls/{number}/comments", "per_page=100", paginate=True)
        commits = gh_json(f"repos/{repo}/pulls/{number}/commits", "per_page=100", paginate=True)
        record_pr(db, pr, pr_issue.get(number), reviews, comments, commits)
