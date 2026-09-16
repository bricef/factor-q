#!/usr/bin/env bash
# Run the dependency audit tools and upsert one advisory PR comment.
#
#   scripts/audit-pr-comment.sh [--dry-run] <pr-number>
#
# The HTML marker identifies one living comment per PR. --dry-run renders it
# to stdout without using gh. Needs GITHUB_REPOSITORY and, when not dry-run,
# GH_TOKEN (or gh auth) plus pull-requests: write.
#
# This reporter does not replace or alter `just audit`: that command remains
# the required gate. Reporting is advisory, so every error is explained and
# exits 0 rather than making a second audit gate.
set -uo pipefail

give_up() {
    echo "dependency audit comment skipped: $*" >&2
    exit 0
}

dry_run=false
if [ "${1:-}" = "--dry-run" ]; then
    dry_run=true
    shift
fi
pr="${1:-}"
if ! $dry_run && [ -z "$pr" ]; then
    give_up "usage: audit-pr-comment.sh [--dry-run] <pr-number>"
fi
repo="${GITHUB_REPOSITORY:-}"
[ -n "$repo" ] || give_up "GITHUB_REPOSITORY must be set (owner/repo)"
marker="<!-- dependency-audit -->"
root="$(git rev-parse --show-toplevel 2>/dev/null)" || give_up "not in a git checkout"
work="$(mktemp -d)" || give_up "could not create temporary directory"
trap 'rm -rf "$work"' EXIT

# Findings make these tools non-zero. Preserve their reports and statuses;
# neither status controls this advisory script's exit code.
cargo audit --json >"${work}/audit.json" 2>"${work}/audit.stderr"
audit_status=$?
cargo deny --format json check >"${work}/deny.json" 2>"${work}/deny.stderr"
deny_status=$?
[ -s "${work}/audit.json" ] || {
    detail="$(head -n 1 "${work}/audit.stderr")"
    give_up "cargo audit produced no JSON${detail:+: ${detail}}"
}

body="$(python3 "${root}/scripts/audit-comment.py" \
    "${work}/audit.json" "${root}/deny.toml" "$deny_status")" \
    || give_up "rendering the comment failed"
[ -n "$body" ] || give_up "renderer produced nothing"
if [ "$audit_status" -ne 0 ]; then
    echo "cargo audit reported findings (status ${audit_status}); rendering them" >&2
fi

if $dry_run; then
    printf '%s\n' "$body"
    exit 0
fi

existing="$(gh api "repos/${repo}/issues/${pr}/comments" --paginate \
    --jq "[.[] | select(.body | startswith(\"${marker}\")) | .id][0] // empty" 2>/dev/null)" \
    || give_up "looking up the existing comment failed"
if [ -n "$existing" ]; then
    gh api -X PATCH "repos/${repo}/issues/comments/${existing}" \
        -f body="$body" >/dev/null || give_up "updating the comment failed"
    echo "updated dependency audit comment ${existing} on PR #${pr}"
else
    gh api -X POST "repos/${repo}/issues/${pr}/comments" \
        -f body="$body" >/dev/null || give_up "creating the comment failed"
    echo "created dependency audit comment on PR #${pr}"
fi
