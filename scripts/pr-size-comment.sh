#!/usr/bin/env bash
# Count the non-test files a PR changes and, above a threshold, label it and
# ask for a scope note — one living comment per PR, renewed on every push.
#
#   scripts/pr-size-comment.sh <pr-number> <base-sha> <head-sha>
#
# The rule it serves (AGENTS.md): a pull request is one semantic change. A
# large diff is either one change with a wide fan-out — a runtime fact that
# reaches the operator surfaces, snapshots and goldens — or several changes
# rolled into one request, and from the outside the two look identical. So
# the job does not decide; it asks. Over the threshold the PR gets the
# `size:large` label and a comment pointing at the description's
# "Consequential files" section, where a genuine fan-out says so. Under it,
# any earlier label and comment are withdrawn, so a PR that was split reads
# clean. Advisory: never fails a build; any error here exits 0 after saying
# what went wrong, the same shape as scripts/coupling-pr-comment.sh.
#
# "Non-test" excludes tests, test support, fixtures, snapshots, goldens and
# the event corpus: adding tests is always free (the size ratchets say the
# same), and a regenerated snapshot is a consequence, not a change.
#
# The merge base, not the base branch tip: a PR is charged for what it
# changed, not for whatever landed on main while it was open.
#
# Needs: GH_TOKEN (or gh auth) and `pull-requests: write` for the label and
# the comment. PR_SIZE_THRESHOLD overrides the threshold (default 20).
# PR_SIZE_DRY_RUN=1 prints the verdict and the comment instead of posting.
set -euo pipefail

pr="${1:?usage: pr-size-comment.sh <pr-number> <base-sha> <head-sha>}"
base_sha="${2:?base sha}"
head_sha="${3:?head sha}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set (owner/repo)}"
threshold="${PR_SIZE_THRESHOLD:-20}"
label="size:large"
marker="<!-- pr-size -->"
dry="${PR_SIZE_DRY_RUN:-}"

give_up() {
    echo "pr-size comment skipped: $*" >&2
    exit 0
}

# --- 1. what changed ---------------------------------------------------------
merge_base="$(git merge-base "$base_sha" "$head_sha" 2>/dev/null)" \
    || give_up "no merge base between ${base_sha} and ${head_sha}"
mapfile -t changed < <(git diff --name-only "$merge_base" "$head_sha")

is_test() {
    case "$1" in
        */tests/*|*/tests.rs|*_tests.rs|*/test_support/*|*/fixtures/*|*/fixtures.rs| \
        */snapshots/*|*.golden|*/golden/*|*/corpus/*) return 0 ;;
        *) return 1 ;;
    esac
}

non_test=()
test_count=0
for f in "${changed[@]}"; do
    if is_test "$f"; then test_count=$((test_count + 1)); else non_test+=("$f"); fi
done
count=${#non_test[@]}
over=0
[ "$count" -gt "$threshold" ] && over=1

# --- 2. render ---------------------------------------------------------------
if [ "$over" = 1 ]; then
    listing="$(printf -- '- `%s`\n' "${non_test[@]}")"
    body="$(cat <<MSG
${marker}
**PR size:** ${count} non-test files changed (threshold ${threshold}), plus ${test_count} test, fixture or snapshot files. Labelled \`size:large\`.

A pull request is one semantic change ([AGENTS.md](https://github.com/${repo}/blob/main/AGENTS.md)). From the outside, one change with a wide fan-out and several changes in one request look the same, so this is a question, not a verdict: if this is one change reaching the operator surfaces, snapshots or goldens, say so under **Consequential files** in the description; if it is more than one change, split it.

<details><summary>Non-test files (${count})</summary>

${listing}

</details>
MSG
)"
else
    body="$(cat <<MSG
${marker}
**PR size:** ${count} non-test files changed (threshold ${threshold}). Within the guide; the \`size:large\` label, if it was set on an earlier push, has been removed.
MSG
)"
fi

if [ -n "$dry" ]; then
    echo "pr-size: pr=#${pr} non_test=${count} test=${test_count} threshold=${threshold} over=${over} label=$([ "$over" = 1 ] && echo add || echo remove)"
    printf '%s\n' "$body"
    exit 0
fi

# --- 3. label ----------------------------------------------------------------
if [ "$over" = 1 ]; then
    gh pr edit "$pr" --repo "$repo" --add-label "$label" > /dev/null \
        || give_up "adding the ${label} label failed (does the label exist?)"
else
    gh pr edit "$pr" --repo "$repo" --remove-label "$label" > /dev/null 2>&1 || true
fi

# --- 4. upsert the comment: always when over; when under, only renew one
#        that an earlier push left, so a split PR reads clean ----------------
existing="$(gh api "repos/${repo}/issues/${pr}/comments" --paginate \
    --jq "[.[] | select(.body | startswith(\"${marker}\")) | .id][0] // empty")"
if [ -n "$existing" ]; then
    gh api -X PATCH "repos/${repo}/issues/comments/${existing}" \
        -f body="$body" > /dev/null || give_up "updating the comment failed"
    echo "updated pr-size comment ${existing} on PR #${pr} (non-test files: ${count})"
elif [ "$over" = 1 ]; then
    gh api -X POST "repos/${repo}/issues/${pr}/comments" \
        -f body="$body" > /dev/null || give_up "creating the comment failed"
    echo "created pr-size comment on PR #${pr} (non-test files: ${count})"
else
    echo "PR #${pr} within the guide (non-test files: ${count}); nothing posted"
fi
