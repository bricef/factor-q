"""scripts/audit-main-issue.py, driven end to end with a fake `just` and `gh`.

The script only runs for real on `main`, on a schedule, so this is where its
decisions are pinned: which findings it reads out of `just audit`, when it
opens, comments, rewrites or closes the tracking issue, and that it stays
silent when nothing changed. `just` is replaced by recorded output and `gh`
by a recorder that answers the issue list from a fixture, so no test touches
the network or GitHub.

The outputs below are recorded from the pinned tools (cargo-audit 0.22.2,
cargo-deny 0.20.2), with local paths replaced by runner paths. RUSTIX is
`just audit` over commit 1ce79ab5's Cargo.lock (rustix 1.1.4); the deny
diagnostics come from a scratch crate depending on smallvec 1.6.0.

    just test-audit-main-issue        (a phase of `just quality`)
"""

import json
import os
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "audit-main-issue.py"
PREFIX = "Dependency audit failing on main"

RUSTIX = """\
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
      Loaded 1458 security advisories (from /home/runner/.cargo/advisory-db)
    Updating crates.io index
    Scanning Cargo.lock for vulnerabilities (511 crate dependencies)
Crate:     rustix
Version:   1.1.4
Warning:   unsound
Title:     xattr functions mark unwritten bytes as initialized when given a zero-length buffer
Date:      2026-10-27
ID:        RUSTSEC-2026-0486
URL:       https://rustsec.org/advisories/RUSTSEC-2026-0486

error: 1 denied warning found!
error: recipe `audit` failed with exit code 1
"""

# cargo audit passes, so the recipe reaches cargo deny, which fails twice and
# warns once; the warning is the kind that runs to thousands of lines.
DENY = """\
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
      Loaded 1458 security advisories (from /home/runner/.cargo/advisory-db)
    Updating crates.io index
    Scanning Cargo.lock for vulnerabilities (511 crate dependencies)
warning[duplicate]: found 3 duplicate entries for crate 'base64'
   ┌─ /home/runner/work/factor-q/factor-q/Cargo.lock:25:1
   │
25 │ ╭ base64 0.13.1 registry+https://github.com/rust-lang/crates.io-index
   │ ╰──────────────────────────────────────────────────────────────────┘ lock entries
   │
   ├ base64 v0.13.1
     └── biscuit-auth v6.0.0

error[rejected]: failed to satisfy license requirements
   ┌─ /home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/smallvec-1.6.0/Cargo.toml:23:12
   │
23 │ license = "MIT/Apache-2.0"
   │            ━━━─━━━━━━━━━━
   │            │   │
   │            │   rejected: license is not explicitly allowed
   │            rejected: license is not explicitly allowed
   │
   ├ MIT - MIT License:
   ├   - OSI approved
   ├ smallvec v1.6.0
     └── fq-runtime v0.1.0

error[vulnerability]: Buffer overflow in SmallVec::insert_many
  ┌─ /home/runner/work/factor-q/factor-q/Cargo.lock:2:1
  │
2 │ smallvec 1.6.0 registry+https://github.com/rust-lang/crates.io-index
  │ ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ security vulnerability detected
  │
  ├ ID: RUSTSEC-2021-0003
  ├ Advisory: https://rustsec.org/advisories/RUSTSEC-2021-0003
  ├ A bug in the `SmallVec::insert_many` method caused it to allocate a buffer that was smaller than needed.
  ├ Solution: Upgrade to >=0.6.14, <1.0.0 OR >=1.6.1 (try `cargo update -p smallvec`)
  ├ smallvec v1.6.0
    └── fq-runtime v0.1.0

advisories FAILED, bans ok, licenses FAILED, sources ok
error: recipe `audit` failed with exit code 1
"""

CLEAN = """\
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
      Loaded 1458 security advisories (from /home/runner/.cargo/advisory-db)
    Updating crates.io index
    Scanning Cargo.lock for vulnerabilities (511 crate dependencies)
warning[duplicate]: found 3 duplicate entries for crate 'base64'
   ├ base64 v0.13.1
     └── biscuit-auth v6.0.0

advisories ok, bans ok, licenses ok, sources ok
"""

# The audit failed, but not on a finding: nothing in this says which crate.
BROKEN = """\
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
error: couldn't fetch advisory database: git operation failed: failed to connect to github.com
error: recipe `audit` failed with exit code 1
"""

FAKE_JUST = """\
#!/usr/bin/env python3
import os, sys
assert sys.argv[1:] == ["audit"], sys.argv
sys.stdout.write(open(os.environ["FAKE_AUDIT_OUTPUT"]).read())
sys.exit(int(os.environ["FAKE_AUDIT_STATUS"]))
"""

# Records every call. Answers the issue list from issues.json (one page, as
# `--slurp` returns it) and an issue create with #4242.
FAKE_GH = """\
#!/usr/bin/env python3
import json, os, sys
state = os.environ["FAKE_GH_DIR"]
args = sys.argv[1:]
payload = json.loads(sys.stdin.read()) if "--input" in args else None
with open(os.path.join(state, "calls.jsonl"), "a") as log:
    log.write(json.dumps({"args": args, "payload": payload}) + "\\n")
if os.environ.get("FAKE_GH_FAIL"):
    sys.stderr.write("HTTP 403: Resource not accessible by integration\\n")
    sys.exit(1)
method = args[args.index("-X") + 1] if "-X" in args else "GET"
path = next(a for a in args if a.startswith("repos/"))
if method == "GET":
    print(json.dumps([json.load(open(os.path.join(state, "issues.json")))]))
elif method == "POST" and path.endswith("/issues"):
    print(json.dumps({"number": 4242}))
else:
    print("{}")
"""


def description(stdout: str) -> str:
    """The description a dry run would give the issue."""
    return stdout.split("----- description -----\n", 1)[1].split("\n----- end -----", 1)[0]


def calls_made(calls: list[dict]) -> list[tuple[str, str]]:
    """(method, path) per gh call."""
    made = []
    for call in calls:
        args = call["args"]
        method = args[args.index("-X") + 1] if "-X" in args else "GET"
        made.append((method, next(a for a in args if a.startswith("repos/"))))
    return made


class AuditMainIssueTest(unittest.TestCase):
    def setUp(self):
        self.sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, check=True,
                                  capture_output=True, text=True).stdout.strip()

    def run_script(self, output, status, *args, issues=(), open_issue=None, fail_gh=False):
        """Run the script; return (exit status, stdout, stderr, gh calls)."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            (tmp / "bin").mkdir()
            for name, source in (("just", FAKE_JUST), ("gh", FAKE_GH)):
                (tmp / "bin" / name).write_text(source)
                (tmp / "bin" / name).chmod(0o755)
            (tmp / "audit.txt").write_text(output)
            (tmp / "issues.json").write_text(json.dumps(list(issues)))
            extra = list(args)
            if open_issue is not None:
                (tmp / "open-issue.md").write_text(open_issue)
                extra += ["--open-issue", str(tmp / "open-issue.md")]
            env = {
                **os.environ,
                "PATH": f"{tmp / 'bin'}{os.pathsep}{os.environ['PATH']}",
                "FAKE_AUDIT_OUTPUT": str(tmp / "audit.txt"),
                "FAKE_AUDIT_STATUS": str(status),
                "FAKE_GH_DIR": str(tmp),
                "FAKE_GH_FAIL": "1" if fail_gh else "",
                "GITHUB_REPOSITORY": "example/repo",
                "GITHUB_SERVER_URL": "https://github.com",
                "GITHUB_RUN_ID": "777",
                "GITHUB_ACTIONS": "",
            }
            env.pop("FQ_REPO_ROOT", None)
            result = subprocess.run(["python3", str(SCRIPT), *extra], env=env,
                                    capture_output=True, text=True)
            log = tmp / "calls.jsonl"
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result.returncode, result.stdout, result.stderr, calls

    def red_description(self, output=RUSTIX):
        code, stdout, _, _ = self.run_script(output, 1, "--dry-run")
        self.assertEqual(code, 1, stdout)
        return description(stdout)

    # --- dry run: what it would do, and no GitHub call --------------------

    def test_red_with_no_issue_open_would_open_one_naming_the_advisory(self):
        code, stdout, stderr, calls = self.run_script(RUSTIX, 1, "--dry-run")
        self.assertEqual(code, 1, stdout + stderr)
        self.assertEqual(calls, [])
        self.assertIn("failing: RUSTSEC-2026-0486 rustix@1.1.4", stdout)
        self.assertIn("would open an issue labelled `dependencies`", stdout)
        self.assertIn(f"title: {PREFIX}: RUSTSEC-2026-0486 (rustix)", stdout)
        body = description(stdout)
        self.assertTrue(body.startswith("<!-- dependency-audit-main\nRUSTSEC-2026-0486 rustix@1.1.4\n-->\n"))
        self.assertIn("| [RUSTSEC-2026-0486](https://rustsec.org/advisories/RUSTSEC-2026-0486) "
                      "| `rustix` | 1.1.4 | unsound | xattr functions mark", body)
        self.assertIn(f"https://github.com/example/repo/commit/{self.sha}", body)
        self.assertIn("https://github.com/example/repo/actions/runs/777", body)
        self.assertIn("ID:        RUSTSEC-2026-0486", body)  # the audit's own output
        self.assertIn("**Fix it.**", body)
        self.assertIn("**Accept it formally**", body)
        self.assertIn("docs/decision-log.md", body)
        self.assertIn("SECURITY.md#risk-register", body)
        # The whole output still reaches the run log.
        self.assertIn("error: 1 denied warning found!", stderr)

    def test_the_same_findings_again_add_nothing(self):
        code, stdout, _, calls = self.run_script(RUSTIX, 1, "--dry-run",
                                                 open_issue=self.red_description())
        self.assertEqual(code, 1, stdout)
        self.assertEqual(calls, [])
        self.assertIn("nothing to do: the audit still fails with the same findings", stdout)
        self.assertNotIn("----- comment -----", stdout)

    def test_changed_findings_are_commented_and_the_description_rewritten(self):
        code, stdout, _, _ = self.run_script(RUSTIX, 1, "--dry-run",
                                             open_issue=self.red_description(DENY))
        self.assertEqual(code, 1, stdout)
        self.assertIn("would comment on #N and rewrite its title and description", stdout)
        comment = stdout.split("----- comment -----\n", 1)[1].split("\n----- end -----", 1)[0]
        self.assertIn("- **New:** `RUSTSEC-2026-0486 rustix@1.1.4`: xattr functions", comment)
        self.assertIn("- **No longer failing:** `RUSTSEC-2021-0003 smallvec@1.6.0`", comment)
        self.assertIn("- **No longer failing:** `rejected smallvec@1.6.0`", comment)
        self.assertTrue(description(stdout).startswith(
            "<!-- dependency-audit-main\nRUSTSEC-2026-0486 rustix@1.1.4\n-->"))

    def test_green_closes_an_open_issue_at_the_clean_commit(self):
        code, stdout, _, calls = self.run_script(CLEAN, 0, "--dry-run",
                                                 open_issue=self.red_description())
        self.assertEqual(code, 0, stdout)
        self.assertEqual(calls, [])
        self.assertIn("would comment on #N and close it", stdout)
        self.assertIn("`main` is clean again: `just audit` passes at "
                      f"[`{self.sha[:12]}`](https://github.com/example/repo/commit/{self.sha})", stdout)

    def test_green_with_nothing_open_does_nothing(self):
        code, stdout, _, calls = self.run_script(CLEAN, 0, "--dry-run")
        self.assertEqual(code, 0, stdout)
        self.assertEqual(calls, [])
        self.assertIn("nothing to do: the audit passes and no tracking issue is open", stdout)

    def test_deny_errors_are_read_and_its_warnings_left_out(self):
        code, stdout, _, _ = self.run_script(DENY, 1, "--dry-run")
        self.assertEqual(code, 1, stdout)
        self.assertIn("failing: RUSTSEC-2021-0003 smallvec@1.6.0; rejected smallvec@1.6.0", stdout)
        self.assertIn(f"title: {PREFIX}: RUSTSEC-2021-0003 (smallvec), rejected (smallvec)", stdout)
        body = description(stdout)
        self.assertIn("error[rejected]: failed to satisfy license requirements", body)
        self.assertIn("├ ID: RUSTSEC-2021-0003", body)
        self.assertIn("advisories FAILED, bans ok, licenses FAILED, sources ok", body)
        self.assertNotIn("warning[duplicate]", body)
        self.assertNotIn("biscuit-auth", body)

    def test_a_failure_with_no_finding_still_opens_the_issue(self):
        code, stdout, _, _ = self.run_script(BROKEN, 1, "--dry-run")
        self.assertEqual(code, 1, stdout)
        self.assertIn("failing: unrecognised-failure", stdout)
        self.assertIn(f"title: {PREFIX}\n", stdout)
        body = description(stdout)
        self.assertIn("No finding could be read from the output", body)
        self.assertIn("couldn't fetch advisory database", body)

    def test_open_issue_simulation_needs_dry_run(self):
        code, _, stderr, calls = self.run_script(RUSTIX, 1, open_issue="anything")
        self.assertEqual(code, 2)
        self.assertIn("needs --dry-run", stderr)
        self.assertEqual(calls, [])

    # --- for real, against the recording gh -------------------------------

    def test_red_opens_one_labelled_issue_and_ignores_pull_requests(self):
        dependabot = {"number": 5, "title": f"{PREFIX}: not an issue", "body": "", "pull_request": {}}
        code, stdout, _, calls = self.run_script(RUSTIX, 1, issues=[dependabot])
        self.assertEqual(code, 1, stdout)
        self.assertEqual(calls_made(calls), [("GET", "repos/example/repo/issues?state=open&per_page=100"),
                                             ("POST", "repos/example/repo/issues")])
        self.assertIn("--paginate", calls[0]["args"])
        created = calls[1]["payload"]
        self.assertEqual(created["labels"], ["dependencies"])
        self.assertEqual(created["title"], f"{PREFIX}: RUSTSEC-2026-0486 (rustix)")
        self.assertTrue(created["body"].startswith("<!-- dependency-audit-main\n"))
        self.assertIn("opened #4242", stdout)

    def test_red_again_with_the_same_findings_writes_nothing(self):
        issue = {"number": 7, "title": f"{PREFIX}: RUSTSEC-2026-0486 (rustix)",
                 "body": self.red_description()}
        code, stdout, _, calls = self.run_script(RUSTIX, 1, issues=[issue])
        self.assertEqual(code, 1, stdout)
        self.assertEqual(calls_made(calls), [("GET", "repos/example/repo/issues?state=open&per_page=100")])
        self.assertIn("tracking issue: #7", stdout)

    def test_a_change_is_commented_before_the_description_is_rewritten(self):
        issue = {"number": 7, "title": f"{PREFIX}: old", "body": self.red_description(DENY)}
        code, stdout, _, calls = self.run_script(RUSTIX, 1, issues=[issue])
        self.assertEqual(code, 1, stdout)
        self.assertEqual(calls_made(calls)[1:], [("POST", "repos/example/repo/issues/7/comments"),
                                                 ("PATCH", "repos/example/repo/issues/7")])
        self.assertIn("**New:** `RUSTSEC-2026-0486 rustix@1.1.4`", calls[1]["payload"]["body"])
        self.assertEqual(calls[2]["payload"]["title"], f"{PREFIX}: RUSTSEC-2026-0486 (rustix)")
        self.assertIn("RUSTSEC-2026-0486 rustix@1.1.4\n-->", calls[2]["payload"]["body"])

    def test_a_description_without_the_marker_counts_as_changed(self):
        issue = {"number": 7, "title": f"{PREFIX}: opened by hand", "body": "rustix is unsound"}
        code, _, _, calls = self.run_script(RUSTIX, 1, issues=[issue])
        self.assertEqual(code, 1)
        self.assertEqual(calls_made(calls)[1:], [("POST", "repos/example/repo/issues/7/comments"),
                                                 ("PATCH", "repos/example/repo/issues/7")])
        self.assertIn("did not list the findings", calls[1]["payload"]["body"])

    def test_green_comments_then_closes(self):
        issue = {"number": 7, "title": f"{PREFIX}: x", "body": self.red_description()}
        code, stdout, _, calls = self.run_script(CLEAN, 0, issues=[issue])
        self.assertEqual(code, 0, stdout)
        self.assertEqual(calls_made(calls)[1:], [("POST", "repos/example/repo/issues/7/comments"),
                                                 ("PATCH", "repos/example/repo/issues/7")])
        self.assertIn("`main` is clean again", calls[1]["payload"]["body"])
        self.assertEqual(calls[2]["payload"], {"state": "closed", "state_reason": "completed"})

    def test_found_by_its_marker_after_a_title_edit(self):
        issue = {"number": 7, "title": "rustix unsound (was: the audit issue)",
                 "body": self.red_description()}
        code, stdout, _, calls = self.run_script(RUSTIX, 1, issues=[issue])
        self.assertEqual(code, 1, stdout)
        self.assertEqual(len(calls), 1, "found and unchanged: no write")

    def test_the_oldest_of_two_open_issues_is_the_one_kept(self):
        body = self.red_description()
        issues = [{"number": 9, "title": f"{PREFIX}: b", "body": body},
                  {"number": 7, "title": f"{PREFIX}: a", "body": body}]
        code, stdout, _, _ = self.run_script(CLEAN, 0, issues=issues)
        self.assertEqual(code, 0, stdout)
        self.assertIn("keeping #7, leaving #9", stdout)
        self.assertIn("commented on #7 and closed it", stdout)

    def test_a_failed_github_call_fails_the_run(self):
        code, _, stderr, _ = self.run_script(RUSTIX, 1, fail_gh=True)
        self.assertEqual(code, 2)
        self.assertIn("HTTP 403", stderr)


if __name__ == "__main__":
    unittest.main()
