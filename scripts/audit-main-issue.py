#!/usr/bin/env python3
"""Keep one GitHub issue open while the dependency audit of `main` fails.

    scripts/audit-main-issue.py [--dry-run] [--open-issue BODY_FILE]

`.github/workflows/scheduled-audit.yml` runs this once a day on `main`. It
runs `just audit`, the gate the Dependency audit CI job runs, and keeps one
tracking issue in step with the result:

    audit fails,  no issue open  ->  open one
    audit fails,  issue open     ->  if what fails changed: comment what
                                     changed and rewrite the description;
                                     otherwise nothing
    audit passes, issue open     ->  comment the clean commit and close it
    audit passes, no issue open  ->  nothing

Advisories arrive without a commit, so before this a new RustSec id was
found by whichever unrelated pull request ran CI next (RUSTSEC-2026-0486,
2026-11-11). This makes it an item of its own, with its own fix.

Identity: the tracking issue is the open issue whose title starts with
TITLE_PREFIX or whose description starts with MARKER. It is opened with the
`dependencies` label so `label:dependencies` finds it too, but the lookup
does not require the label, so removing it cannot cause a duplicate. The
lookup reads the issues list, not search: search is eventually consistent
and can miss an issue opened minutes earlier. The workflow's concurrency
group stops two runs racing to open one each. The findings a description
was written for are listed inside MARKER, and "changed" is measured
against that list.

Findings are read from `just audit`'s own output: cargo audit's `Crate:`
blocks and cargo deny's `error[...]` diagnostics. The issue names exactly
what the gate reported, and there is no second copy of the gate's ignore
policy here. A failure with nothing recognisable in it (the advisory
database could not be fetched, the tools are the wrong version) still opens
the issue and says so. The issue is the point of this script, so it fails
closed.

--dry-run prints what would be done and makes no GitHub calls. It assumes
no issue is open unless --open-issue names a file holding the description
of one. Exit status: 0 the audit passes, 1 the audit fails (and the issue
is in step), 2 this script could not do its job.

Needs GITHUB_REPOSITORY (owner/repo) and, unless --dry-run, GH_TOKEN (or gh
auth) with `issues: write`. GITHUB_SERVER_URL and GITHUB_RUN_ID, default
Actions variables, give the run link. FQ_REPO_ROOT overrides the checkout
that is audited.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

TITLE_PREFIX = "Dependency audit failing on main"
LABEL = "dependencies"
MARKER = "<!-- dependency-audit-main"
UNRECOGNISED = "unrecognised-failure"
WORKFLOW = ".github/workflows/scheduled-audit.yml"
# GitHub refuses a description over 65536 characters; the table, the
# guidance and the fence need the rest.
EXCERPT_LIMIT = 40_000

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
AUDIT_FIELD = re.compile(r"^(Crate|Version|Warning|Title|ID|URL):\s+(.*?)\s*$")
DIAGNOSTIC = re.compile(r"^(error|warning|note|help)(?:\[([^\]]+)\])?:\s*(.*?)\s*$")
DENY_SUMMARY = re.compile(r"^(advisories|bans|licenses|sources) (ok|FAILED)\b")
DENY_ID = re.compile(r"^\s*├ ID: (\S+)\s*$")
DENY_URL = re.compile(r"^\s*├ Advisory: (\S+)\s*$")
DENY_CRATE = re.compile(r"^\s*├ (\S+) v(\d\S*)\s*$")


class Failure(Exception):
    """This script could not do its job (exit 2)."""


@dataclass
class Finding:
    kind: str
    crate: str = ""
    version: str = ""
    id: str = ""
    title: str = ""
    url: str = ""

    @property
    def key(self) -> str:
        subject = f" {self.crate}@{self.version}" if self.crate else ""
        return f"{self.id or self.kind}{subject}"


@dataclass
class Plan:
    action: str  # open | update | close | none
    reason: str = ""
    title: str = ""
    body: str = ""
    comment: str = ""


# --- reading the audit -------------------------------------------------------

def parse_findings(output: str) -> list[Finding]:
    """Every finding cargo audit or cargo deny reported, one per key."""
    found: dict[str, Finding] = {}

    def add(finding: Finding | None) -> None:
        if finding is None:
            return
        prior = found.setdefault(finding.key, finding)
        for name in ("title", "url"):
            if not getattr(prior, name):
                setattr(prior, name, getattr(finding, name))

    block: dict[str, str] | None = None  # a cargo audit `Crate:` block
    diagnostic: Finding | None = None    # a cargo deny `error[...]` diagnostic
    for line in ANSI.sub("", output).splitlines():
        field = AUDIT_FIELD.match(line)
        if field:
            name, value = field.groups()
            if name == "Crate":
                add(audit_finding(block))
                block = {}
            if block is not None:
                block.setdefault(name, value)
            continue
        header = DIAGNOSTIC.match(line)
        if header or DENY_SUMMARY.match(line):
            add(audit_finding(block))
            add(diagnostic)
            block, diagnostic = None, None
            if header and header.group(1) == "error" and header.group(2):
                diagnostic = Finding(kind=header.group(2), title=header.group(3))
            continue
        if diagnostic is None:
            continue
        if m := DENY_ID.match(line):
            diagnostic.id = m.group(1)
        elif m := DENY_URL.match(line):
            diagnostic.url = m.group(1)
        elif (m := DENY_CRATE.match(line)) and not diagnostic.crate:
            diagnostic.crate, diagnostic.version = m.groups()
    add(audit_finding(block))
    add(diagnostic)
    return sorted(found.values(), key=lambda f: f.key)


def audit_finding(block: dict[str, str] | None) -> Finding | None:
    if not block:
        return None
    return Finding(
        kind=block.get("Warning", "vulnerability"),
        crate=block.get("Crate", ""),
        version=block.get("Version", ""),
        id=block.get("ID", ""),
        title=block.get("Title", ""),
        url=block.get("URL", ""),
    )


def excerpt(output: str) -> str:
    """The output a reader needs: everything but cargo deny's warnings.

    `multiple-versions = "warn"` prints every duplicate crate's tree, which is
    thousands of lines on this workspace and never the reason the gate failed.
    """
    kept: list[str] = []
    skipping = False
    for line in ANSI.sub("", output).splitlines():
        header = DIAGNOSTIC.match(line)
        if header:
            skipping = header.group(1) != "error" and header.group(2) is not None
        elif DENY_SUMMARY.match(line):
            skipping = False
        if not skipping:
            kept.append(line.rstrip())
    text = re.sub(r"\n{3,}", "\n\n", "\n".join(kept)).strip("\n")
    if len(text) > EXCERPT_LIMIT:
        text = text[:EXCERPT_LIMIT].rstrip() + "\n[truncated: the workflow run's log has the whole output]"
    return text


def recorded_keys(body: str) -> list[str] | None:
    """The finding keys inside MARKER, or None when there is no marker."""
    lines = body.replace("\r\n", "\n").split("\n")
    for at, line in enumerate(lines):
        if line.strip() == MARKER:
            keys = []
            for entry in lines[at + 1:]:
                if entry.strip() == "-->":
                    return keys
                if entry.strip():
                    keys.append(entry.strip())
            return keys
    return None


# --- deciding ----------------------------------------------------------------

@dataclass
class Context:
    repo: str
    sha: str
    status: int
    run_url: str
    server: str

    def link(self, path: str) -> str:
        return f"{self.server}/{self.repo}/{path}"

    def at(self) -> str:
        commit = f"[`{self.sha[:12]}`]({self.link('commit/' + self.sha)})"
        run = f" ([workflow run]({self.run_url}))" if self.run_url else ""
        return commit + run


def keys_of(findings: list[Finding]) -> list[str]:
    return [f.key for f in findings] or [UNRECOGNISED]


def plan(passed: bool, findings: list[Finding], output: str,
         open_body: str | None, ctx: Context) -> Plan:
    if passed:
        if open_body is None:
            return Plan("none", reason="the audit passes and no tracking issue is open")
        return Plan("close", comment=clean_comment(ctx))
    keys = keys_of(findings)
    title, body = issue_title(findings), issue_body(findings, keys, output, ctx)
    if open_body is None:
        return Plan("open", title=title, body=body)
    before = recorded_keys(open_body)
    if before is not None and sorted(before) == sorted(keys):
        return Plan("none", reason="the audit still fails with the same findings; the issue already says so")
    return Plan("update", title=title, body=body,
                comment=changed_comment(before, keys, findings, ctx))


# --- rendering ---------------------------------------------------------------

def cell(text: str) -> str:
    return (text or "—").replace("|", "\\|").replace("\n", " ")


def describe(key: str) -> str:
    if key == UNRECOGNISED:
        return "a failure with no finding this script recognises (see the audit output)"
    return f"`{key}`"


def issue_title(findings: list[Finding]) -> str:
    named = []
    for f in findings:
        label = f"{f.id or f.kind} ({f.crate})" if f.crate else (f.id or f.kind)
        if label not in named:
            named.append(label)
    if not named:
        return TITLE_PREFIX
    shown = ", ".join(named[:3])
    if len(named) > 3:
        shown += f" and {len(named) - 3} more"
    return f"{TITLE_PREFIX}: {shown}"


def issue_body(findings: list[Finding], keys: list[str], output: str, ctx: Context) -> str:
    docs = ctx.link("blob/main/")
    lines = [MARKER, *keys, "-->",
             f"The scheduled dependency audit of `main` fails at {ctx.at()}: "
             f"`just audit` exited {ctx.status}.",
             ""]
    if findings:
        lines += ["| Finding | Crate | Version | Kind | Title |", "|---|---|---|---|---|"]
        for f in findings:
            name = f.id or f.kind
            name = f"[{name}]({f.url})" if f.url else cell(name)
            crate = f"`{f.crate}`" if f.crate else "—"
            lines.append(f"| {name} | {crate} | {cell(f.version)} | {cell(f.kind)} | {cell(f.title)} |")
    else:
        lines.append("No finding could be read from the output, so the audit may not "
                     "have run at all (the advisory database could not be fetched, or "
                     "the tools are not the pinned versions). The output below says why.")
    lines += [
        "",
        "Until this is resolved, the Dependency audit job, and with it the required "
        "**Rust CI** check, is red on every fresh CI run, including pull requests that "
        "touch no dependency. A red audit is never merged around. There are two ways "
        "out, each in a pull request of its own:",
        "",
        "1. **Fix it.** Usually a lockfile or dependency bump: `cargo update -p <crate>` "
        "when a patched release exists, then `just audit` to confirm.",
        "2. **Accept it formally**, when there is no fix yet: an explained `ignore` line "
        f"in [`deny.toml`]({docs}deny.toml), a dated entry in "
        f"[`docs/decision-log.md`]({docs}docs/decision-log.md), a row in the "
        f"[`SECURITY.md` risk register]({docs}SECURITY.md#risk-register) and a tracking "
        "issue, together in one pull request. The RUSTSEC-2026-0459 entry is a worked example.",
        "",
        "Once that has merged, a pull request that was red only because of this needs "
        "updating with `main` and a fresh green run before it merges.",
        "",
        "<details><summary>Audit output</summary>",
        "",
        "````text",
        excerpt(output) or "(no output)",
        "````",
        "",
        "</details>",
        "",
        f"_Kept by [`{WORKFLOW}`]({docs}{WORKFLOW}) (`scripts/audit-main-issue.py`). "
        "It comments only when what fails changes, and closes this issue on the first "
        f"green run. Keep the `{TITLE_PREFIX}` title prefix: it is how the next run "
        "finds this issue._",
    ]
    return "\n".join(lines)


def changed_comment(before: list[str] | None, keys: list[str],
                    findings: list[Finding], ctx: Context) -> str:
    titles = {f.key: f.title for f in findings}
    lines = [f"What fails on `main` changed at {ctx.at()}.", ""]
    if before is None:
        lines.append("The description did not list the findings it was written for, "
                     "so it has been rewritten from this run.")
    else:
        for key in sorted(set(keys) - set(before)):
            title = f": {titles[key]}" if titles.get(key) else ""
            lines.append(f"- **New:** {describe(key)}{title}")
        for key in sorted(set(before) - set(keys)):
            lines.append(f"- **No longer failing:** {describe(key)}")
    lines += ["", "The description now shows the current findings and audit output."]
    return "\n".join(lines)


def clean_comment(ctx: Context) -> str:
    return f"`main` is clean again: `just audit` passes at {ctx.at()}. Closing."


# --- GitHub ------------------------------------------------------------------

def gh(*args: str, payload: dict | None = None) -> str:
    command = ["gh", "api", *args]
    if payload is not None:
        command += ["--input", "-"]
    result = subprocess.run(
        command, capture_output=True, text=True,
        input=None if payload is None else json.dumps(payload),
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise Failure(f"`gh api {' '.join(args)}` failed: {detail}")
    return result.stdout


def find_open_issue(repo: str) -> dict | None:
    pages = json.loads(gh("--paginate", "--slurp", f"repos/{repo}/issues?state=open&per_page=100") or "[]")
    matches = sorted(
        (issue for page in pages for issue in page
         if "pull_request" not in issue
         and (issue.get("title", "").startswith(TITLE_PREFIX)
              or (issue.get("body") or "").lstrip().startswith(MARKER))),
        key=lambda issue: issue["number"],
    )
    if len(matches) > 1:
        others = ", ".join(f"#{issue['number']}" for issue in matches[1:])
        say(f"more than one tracking issue is open; keeping #{matches[0]['number']}, "
            f"leaving {others} for a human to close")
    return matches[0] if matches else None


def carry_out(step: Plan, repo: str, issue: dict | None) -> str:
    if step.action == "open":
        created = json.loads(gh("-X", "POST", f"repos/{repo}/issues", payload={
            "title": step.title, "body": step.body, "labels": [LABEL]}))
        return f"opened #{created['number']}"
    number = issue["number"]
    gh("-X", "POST", f"repos/{repo}/issues/{number}/comments", payload={"body": step.comment})
    if step.action == "update":
        gh("-X", "PATCH", f"repos/{repo}/issues/{number}",
           payload={"title": step.title, "body": step.body})
        return f"commented on #{number} and rewrote its description"
    gh("-X", "PATCH", f"repos/{repo}/issues/{number}",
       payload={"state": "closed", "state_reason": "completed"})
    return f"commented on #{number} and closed it"


# --- the run -----------------------------------------------------------------

def say(text: str) -> None:
    print(f"audit-main-issue: {text}", flush=True)


def audit(root: Path) -> tuple[str, int, str]:
    """The commit at `root`, and `just audit`'s exit status and output there."""
    sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, capture_output=True, text=True)
    if sha.returncode != 0:
        raise Failure(f"cannot read the commit being audited: {sha.stderr.strip()}")
    result = subprocess.run(["just", "audit"], cwd=root, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True)
    # The whole output goes to the run log; the issue gets the excerpt.
    sys.stderr.write(result.stdout)
    return sha.stdout.strip(), result.returncode, result.stdout


def show(step: Plan, number: object) -> None:
    say({
        "open": f"dry run: would open an issue labelled `{LABEL}`",
        "update": f"dry run: would comment on #{number} and rewrite its title and description",
        "close": f"dry run: would comment on #{number} and close it",
    }[step.action])
    if step.title:
        print(f"title: {step.title}")
    if step.comment:
        print("----- comment -----", step.comment, "----- end -----", sep="\n")
    if step.body:
        print("----- description -----", step.body, "----- end -----", sep="\n")


def run(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--dry-run", action="store_true",
                        help="print what would be done; make no GitHub calls")
    parser.add_argument("--open-issue", metavar="BODY_FILE", type=Path,
                        help="with --dry-run: act as if a tracking issue with this description is open")
    args = parser.parse_args(argv)
    if args.open_issue and not args.dry_run:
        raise Failure("--open-issue simulates GitHub, so it needs --dry-run")
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    if not repo:
        raise Failure("GITHUB_REPOSITORY must be set (owner/repo)")
    root = Path(os.environ.get("FQ_REPO_ROOT") or Path(__file__).resolve().parent.parent)
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_id = os.environ.get("GITHUB_RUN_ID", "")

    sha, status, output = audit(root)
    ctx = Context(repo=repo, sha=sha, status=status, server=server,
                  run_url=f"{server}/{repo}/actions/runs/{run_id}" if run_id else "")
    passed = status == 0
    findings = [] if passed else parse_findings(output)
    say(f"`just audit` exited {status} at {sha}")
    if not passed:
        say("failing: " + "; ".join(keys_of(findings)))

    if not args.dry_run:
        issue = find_open_issue(repo)
        say(f"tracking issue: #{issue['number']}" if issue else "tracking issue: none open")
    elif args.open_issue:
        issue = {"number": "N", "body": args.open_issue.read_text()}
        say(f"tracking issue: #N (simulated, description from {args.open_issue})")
    else:
        issue = None
        say("tracking issue: none open (dry run: assumed; --open-issue simulates one)")

    step = plan(passed, findings, output, None if issue is None else issue.get("body") or "", ctx)
    if step.action == "none":
        say(f"nothing to do: {step.reason}")
    elif args.dry_run:
        show(step, issue["number"] if issue else None)
    else:
        say(carry_out(step, repo, issue))

    if not passed and os.environ.get("GITHUB_ACTIONS") == "true":
        print(f"::error::the dependency audit fails on main: {'; '.join(keys_of(findings))}")
    return 0 if passed else 1


def main() -> int:
    try:
        return run(sys.argv[1:])
    except Failure as error:
        print(f"audit-main-issue: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
