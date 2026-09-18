"""Merge and corrective-commit extraction from git history."""

from __future__ import annotations

import re
import sqlite3
import subprocess
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

from .db import transition, upsert

FIX = re.compile(r"(?i)\b(?:fix(?:e[sd])?|revert(?:e[sd])?)\b")
REFERENCE = re.compile(r"(?i)(?:#|review\s+#?)(\d+)")
REVERT_SHA = re.compile(r"(?i)this reverts commit ([0-9a-f]{7,40})")
PR_SUBJECT = re.compile(r"\(#(\d+)\)|Merge pull request #(\d+)")


@dataclass
class Commit:
    sha: str
    at: str
    subject: str
    body: str
    files: set[str]


def parse_time(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def git(repo_path: str | Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(repo_path), *args], check=True,
                          text=True, capture_output=True).stdout


def commits(repo_path: str | Path, ref: str = "main") -> list[Commit]:
    raw = git(repo_path, "log", ref, "--format=%x1e%H%x1f%cI%x1f%s%x1f%b", "--name-only")
    result: list[Commit] = []
    for record in raw.split("\x1e"):
        record = record.strip()
        if not record:
            continue
        fields = record.split("\x1f", 3)
        if len(fields) != 4:
            continue
        sha, at, subject, tail = fields
        lines = tail.splitlines()
        body_lines: list[str] = []
        files: set[str] = set()
        for line in lines:
            if line and "\t" not in line and ("/" in line or "." in line) and " " not in line:
                files.add(line)
            else:
                body_lines.append(line)
        result.append(Commit(sha, at, subject, "\n".join(body_lines), files))
    return result


def numstat(repo_path: str | Path, sha: str) -> tuple[int, int]:
    added = deleted = 0
    for line in git(repo_path, "show", "--format=", "--numstat", sha).splitlines():
        parts = line.split("\t")
        if len(parts) >= 2 and parts[0].isdigit() and parts[1].isdigit():
            added += int(parts[0])
            deleted += int(parts[1])
    return added, deleted


def correction_rule(candidate: Commit, merge: Commit | None, pr: int,
                    issue: int, merge_files: set[str]) -> str | None:
    message = f"{candidate.subject}\n{candidate.body}"
    refs = {int(value) for value in REFERENCE.findall(message)}
    if pr in refs:
        return "references_pr"
    if issue in refs:
        return "references_issue"
    reverted = REVERT_SHA.search(message)
    if reverted and merge and merge.sha.startswith(reverted.group(1)):
        return "reverts_merge"
    if FIX.search(candidate.subject) and candidate.files & merge_files:
        return "fix_or_revert_same_files"
    return None


def extract(db: sqlite3.Connection, repo_path: str | Path,
            now: datetime | None = None, ref: str = "main") -> None:
    history = commits(repo_path, ref)
    by_pr: dict[int, Commit] = {}
    for commit in history:
        match = PR_SUBJECT.search(commit.subject)
        if match:
            by_pr[int(match.group(1) or match.group(2))] = commit
    now = now or datetime.now(timezone.utc)
    rows = db.execute(
        "SELECT number, issue, merged_at FROM pull_requests "
        "WHERE merged_at IS NOT NULL AND issue IS NOT NULL"
    ).fetchall()
    for row in rows:
        pr, issue, merged_at = int(row["number"]), int(row["issue"]), row["merged_at"]
        merged = parse_time(merged_at)
        deadline = merged + timedelta(days=14)
        merge_commit = by_pr.get(pr)
        merge_files = merge_commit.files if merge_commit else set()
        found: list[tuple[Commit, str]] = []
        for candidate in history:
            at = parse_time(candidate.at)
            if at <= merged or at > deadline:
                continue
            rule = correction_rule(candidate, merge_commit, pr, issue, merge_files)
            if rule:
                found.append((candidate, rule))
        found.sort(key=lambda item: item[0].at)
        for candidate, rule in found:
            additions, deletions = numstat(repo_path, candidate.sha)
            upsert(db, "corrective_commits", {
                "sha": candidate.sha, "pr_number": pr, "issue": issue,
                "at": candidate.at, "rule": rule, "additions": additions,
                "deletions": deletions,
            }, ("sha", "pr_number"))
            transition(db, issue=issue, at=candidate.at, kind="corrective_commit",
                       from_state="merged", to_state="corrected", actor=None,
                       ref=f"commit:{candidate.sha}:{rule}")
        survived = not found and now >= deadline
        first = found[0] if found else (None, None)
        upsert(db, "outcomes", {
            "issue": issue, "pr_number": pr, "merged_at": merged_at,
            "accepted_at": deadline.isoformat().replace("+00:00", "Z") if survived else None,
            "accepted": int(survived),
            "correction_commit": first[0].sha if found else None,
            "rule": first[1] if found else ("survived_14_days" if survived else "pending_14_days"),
        }, ("issue", "pr_number"))
