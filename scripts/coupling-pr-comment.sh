#!/usr/bin/env bash
# Measure module coupling on both sides of a PR and upsert one comment.
#
#   scripts/coupling-pr-comment.sh <pr-number> <base-sha> <head-sha>
#
# The comment is upserted by an HTML marker — one living comment per PR,
# renewed in place on every push, never a trail of stale copies. Same shape as
# scripts/dashboard-pr-comment.sh, minus the blob branch: this output is text,
# so nothing needs hosting.
#
# ONE BINARY, TWO TREES. fq-lint is built once from the head checkout and then
# run against each side. Building it separately per side would let a change to
# the measurement show up as a change in the measurement's subject — a PR that
# edits fq-lint would report coupling deltas across the whole tree that nobody
# introduced. The base tree is materialised as a throwaway git worktree, which
# leaves the PR checkout untouched.
#
# Needs: GH_TOKEN (or gh auth) and `pull-requests: write` for the comment.
# Run by the coupling-metrics CI job in PR context; runnable locally too.
# Advisory: this never fails a build, so any error here exits 0 after saying
# what went wrong.
set -uo pipefail

pr="${1:?usage: coupling-pr-comment.sh <pr-number> <base-sha> <head-sha>}"
base_sha="${2:?base sha}"
head_sha="${3:?head sha}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set (owner/repo)}"
marker="<!-- coupling-metrics -->"

root="$(git rev-parse --show-toplevel)"
work="$(mktemp -d)"
base_tree="${work}/base"
cleanup() {
    git -C "$root" worktree remove --force "$base_tree" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

give_up() {
    echo "coupling comment skipped: $*" >&2
    exit 0
}

# --- 1. one binary, from the head tree ------------------------------------
cargo build -q -p fq-lint || give_up "fq-lint did not build"
bin="${root}/target/debug/fq-lint"
[ -x "$bin" ] || give_up "no fq-lint binary at ${bin}"

# --- 2. measure both sides ------------------------------------------------
(cd "$root" && "$bin" --coupling --json) > "${work}/head.json" \
    || give_up "measuring the head tree failed"

# The merge base, not the base branch tip: a PR should be charged for what it
# changed, not for whatever landed on main while it was open.
merge_base="$(git -C "$root" merge-base "$base_sha" "$head_sha" 2>/dev/null)"
[ -n "$merge_base" ] || give_up "no merge base between ${base_sha} and ${head_sha}"

git -C "$root" worktree add -q --detach "$base_tree" "$merge_base" \
    || give_up "could not check out merge base ${merge_base}"
(cd "$base_tree" && "$bin" --coupling --json) > "${work}/base.json" \
    || give_up "measuring the base tree failed"

# --- 3. render ------------------------------------------------------------
body="$(python3 "${root}/scripts/coupling-comment.py" \
    "${work}/base.json" "${work}/head.json" "$head_sha" "$repo")" \
    || give_up "rendering the comment failed"
[ -n "$body" ] || give_up "renderer produced nothing"

# --- 4. upsert ------------------------------------------------------------
existing="$(gh api "repos/${repo}/issues/${pr}/comments" --paginate \
    --jq "[.[] | select(.body | startswith(\"${marker}\")) | .id][0] // empty")"
if [ -n "$existing" ]; then
    gh api -X PATCH "repos/${repo}/issues/comments/${existing}" \
        -f body="$body" > /dev/null || give_up "updating the comment failed"
    echo "updated coupling comment ${existing} on PR #${pr}"
else
    gh api -X POST "repos/${repo}/issues/${pr}/comments" \
        -f body="$body" > /dev/null || give_up "creating the comment failed"
    echo "created coupling comment on PR #${pr}"
fi
