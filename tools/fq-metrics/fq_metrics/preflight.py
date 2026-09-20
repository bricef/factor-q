"""Pre-flight checks and actionable errors for the extractor's external tools.

The extractor shells out to three programs — ``fq`` for the daemon's event
log, ``gh`` for GitHub, ``git`` for mainline history — and a missing or
unpaired one used to surface as a bare ``FileNotFoundError`` or a
``CalledProcessError`` from deep inside a loop. Every failure the operator
can act on is raised here as a :class:`ToolError`, which says what failed,
why the tool needs it, and what to do next.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

FQ_WHY = ("`fq` is the factor-q CLI. The extractor runs `fq events query` and "
          "`fq events get` to read the daemon's event log, which is the only "
          "source of attempts (dispatch time, model, cost, outcome).")
FQ_FIX = (
    "Either build and pair the CLI on this machine:\n"
    "    cargo build --release -p fq-cli        # -> target/release/fq, put it on PATH\n"
    "    fq connect <edge-addr> --token <token> --fingerprint <fingerprint>\n"
    "  Mint a read-only token from a client that is already paired (on the dogfood\n"
    "  guest that is the fqd container; attenuation only narrows, and read:event is\n"
    "  all the extractor needs):\n"
    "    docker compose exec fqd fq token attenuate --addr 127.0.0.1:9470 --grant read:event\n"
    "    docker compose exec fqd cat /var/lib/factor-q/state/edge/fingerprint\n"
    "  The edge listens on the guest's 127.0.0.1:9470, so tunnel it first:\n"
    "    ssh -L 9470:127.0.0.1:9470 fq@<guest>   then   fq connect 127.0.0.1:9470 …\n"
    "or skip the edge and read an export instead:\n"
    "    just metrics-extract -- --events <dir>\n"
    "  where <dir> holds the JSON written by `fq events query --json` and\n"
    "  `fq events get <id> --json` on a machine where fq is paired\n"
    "  (tools/fq-metrics/README.md, \"Event export\")."
)
GH_WHY = ("`gh` is the GitHub CLI. The extractor reads issue timelines, pull "
          "requests, reviews and comments through `gh api`; they supply task "
          "admission, transitions, provenance and outcomes.")
GH_FIX = ("Install it (https://cli.github.com/) and sign in:\n"
          "    gh auth login\n"
          "or pass --no-github to build a ledger from the event log and git only\n"
          "(no outcomes, no acceptance, no transitions).")


class ToolError(Exception):
    """A failure the operator can fix, rendered as what / why / fix."""

    def __init__(self, what: str, why: str, fix: str) -> None:
        super().__init__(what)
        self.what, self.why, self.fix = what, why, fix

    def __str__(self) -> str:
        def indent(text: str) -> str:
            return "\n".join("    " + line for line in text.splitlines())

        return f"fq-metrics: {self.what}\n  why:\n{indent(self.why)}\n  fix:\n{indent(self.fix)}"


def _run(command: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(command, text=True, capture_output=True)


def check_fq() -> None:
    if shutil.which("fq") is None:
        raise ToolError("`fq` is not on PATH", FQ_WHY, FQ_FIX)
    probe = _run(["fq", "events", "query", "--limit", "1", "--json"])
    if probe.returncode:
        raise ToolError(
            "`fq events query` failed, so the event log cannot be read",
            FQ_WHY + "\nThe CLI is installed but the call did not succeed. Its stderr:\n"
            + (probe.stderr.strip() or "(empty)"),
            "If it says no pairing or no daemon: `fq connect <edge-addr> --token … "
            "--fingerprint …`, or set FQ_ADDR / --addr when several daemons are paired.\n"
            "If the daemon is unreachable: check the tunnel or the host, or use --events.\n"
            + FQ_FIX,
        )


def check_gh() -> None:
    if shutil.which("gh") is None:
        raise ToolError("`gh` is not on PATH", GH_WHY, GH_FIX)
    probe = _run(["gh", "auth", "status"])
    if probe.returncode:
        raise ToolError(
            "`gh` is installed but not signed in",
            GH_WHY + "\n`gh auth status` said:\n" + (probe.stderr.strip() or probe.stdout.strip() or "(nothing)"),
            GH_FIX,
        )


def check_git(repo_path: Path, ref: str) -> None:
    why = ("The extractor reads `git log` on the mainline to find merge commits and "
           "the corrective commits that follow them within 14 days; that is how an "
           "outcome becomes accepted or corrected.")
    if shutil.which("git") is None:
        raise ToolError("`git` is not on PATH", why, "Install git.")
    inside = _run(["git", "-C", str(repo_path), "rev-parse", "--is-inside-work-tree"])
    if inside.returncode:
        raise ToolError(
            f"{repo_path} is not a git checkout", why,
            "Run from the factor-q repository root, or pass --repo-path <checkout>.",
        )
    if _run(["git", "-C", str(repo_path), "rev-parse", "--is-shallow-repository"]).stdout.strip() == "true":
        raise ToolError(
            "the checkout is shallow, so its history is incomplete", why,
            "    git -C {0} fetch --unshallow".format(repo_path),
        )
    if _run(["git", "-C", str(repo_path), "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"]).returncode:
        raise ToolError(
            f"git ref `{ref}` does not exist in {repo_path}", why,
            f"    git -C {repo_path} fetch origin\nor pass --git-ref <ref> (default: main).",
        )
    if _run(["git", "-C", str(repo_path), "rev-parse", "--verify", "--quiet", "origin/metrics"]).returncode:
        print("fq-metrics: note: no `origin/metrics` ref, so the human logs are empty this run;"
              " `git fetch origin metrics` to include them", file=sys.stderr)


def check_export(directory: str) -> None:
    path = Path(directory)
    why = "--events replaces the edge with a directory of JSON files written by `fq events query/get --json`."
    if not path.is_dir():
        raise ToolError(f"--events {directory} is not a directory", why,
                        "Point --events at the export directory (it is searched recursively).")
    if next(path.rglob("*.json"), None) is None:
        raise ToolError(f"--events {directory} holds no *.json files", why,
                        "Export the events first (tools/fq-metrics/README.md, \"Event export\").")


def check_ledger(path: str) -> None:
    if not Path(path).is_file():
        raise ToolError(
            f"no ledger at {path}",
            "`report` only renders; the numbers come from a ledger that `extract` built.",
            "    just metrics-extract\nfirst, or pass --ledger <path> to an existing attempt_ledger.sqlite.",
        )


def describe_failure(error: subprocess.CalledProcessError) -> ToolError:
    """Turn a failed external call, mid-run, into the same what/why/fix shape."""
    program = str(error.cmd[0]) if error.cmd else "a subprocess"
    stderr = (error.stderr or "").strip() if isinstance(error.stderr, str) else ""
    shown = " ".join(str(part) for part in error.cmd)[:200]
    fixes = {
        "fq": "The pairing or the daemon changed mid-run; re-run `fq events query --limit 1` by hand, then retry.",
        "gh": ("A GitHub call failed part-way. If stderr mentions rate limits, wait for the reset "
               "(`gh api rate_limit`) and re-run: extraction is incremental and resumes where it left off. "
               "If it mentions auth, `gh auth login`."),
        "git": "Check --repo-path and --git-ref; `git fetch origin` if the ref is stale.",
    }
    return ToolError(
        f"`{program}` exited {error.returncode}: {shown}",
        stderr or "(no stderr)",
        fixes.get(program, "Re-run with the same arguments; if it repeats, open an issue with this output."),
    )
