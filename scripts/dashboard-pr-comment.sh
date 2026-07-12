#!/usr/bin/env bash
# Publish dashboard screenshots and upsert a gallery comment on a PR.
#
#   scripts/dashboard-pr-comment.sh <shots-dir> <pr-number> <head-sha>
#
# PR comments cannot attach files — images must be markdown pointing at
# a publicly fetchable URL — so the PNGs are committed to the dedicated
# `ci/dashboard-screenshots` branch (blob storage, never touches main)
# and referenced by **commit sha** on raw.githubusercontent.com: the
# branch keeps history, so every past comment's images stay live.
# The comment is upserted by an HTML marker — one living gallery per
# PR, updated in place on each push, never a trail of stale copies.
#
# Needs: GH_TOKEN (or gh auth), GITHUB_REPOSITORY, push rights to the
# ci branch, and `pull-requests: write` for the comment. Run by the
# dashboard-screenshots CI job in PR context; runnable locally too.
set -euo pipefail

shots_dir="${1:?usage: dashboard-pr-comment.sh <shots-dir> <pr-number> <head-sha>}"
pr="${2:?pr number}"
head_sha="${3:?head sha}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set (owner/repo)}"
branch="ci/dashboard-screenshots"
marker="<!-- dashboard-screenshots -->"

# Actions runners have no git identity; default one for the blob commit.
export GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-github-actions[bot]}"
export GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-41898282+github-actions[bot]@users.noreply.github.com}"
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"

shopt -s nullglob
pngs=("$shots_dir"/*.png)
[ "${#pngs[@]}" -gt 0 ] || { echo "no PNGs in $shots_dir" >&2; exit 1; }

# --- 1. commit the PNGs to the ci branch, via plumbing --------------------
# A temp index + commit-tree builds the commit without ever switching the
# working tree away from the PR checkout. Parenting on the remote tip
# keeps history append-only; one retry absorbs a race with another run.
publish() {
    git fetch -q origin "$branch" 2>/dev/null || true
    local parent
    parent="$(git rev-parse -q --verify "origin/$branch^{commit}" || true)"
    local idx tree commit
    idx="$(mktemp -u)"
    for f in "${pngs[@]}"; do
        local blob
        blob="$(git hash-object -w "$f")"
        GIT_INDEX_FILE="$idx" git update-index --add \
            --cacheinfo "100644,$blob,$(basename "$f")"
    done
    tree="$(GIT_INDEX_FILE="$idx" git write-tree)"
    rm -f "$idx"
    commit="$(git commit-tree "$tree" ${parent:+-p "$parent"} \
        -m "dashboard screenshots for ${head_sha} (PR #${pr})")"
    git push -q origin "$commit:refs/heads/$branch" && echo "$commit"
}
shots_commit="$(publish || publish)"
[ -n "$shots_commit" ] || { echo "failed to publish screenshots branch" >&2; exit 1; }

# --- 2. build the gallery body --------------------------------------------
raw="https://raw.githubusercontent.com/${repo}/${shots_commit}"
short="${head_sha:0:12}"
taken_at="$(date -u '+%Y-%m-%d %H:%M UTC')"
body="${marker}
### Dashboard screenshots

Rendered from deterministic fixtures at [\`${short}\`](https://github.com/${repo}/commit/${head_sha}) · ${taken_at}.
This comment renews in place on every dashboard-touching push — if the sha above is not the PR head, the gallery is stale.

| | |
|---|---|"
row=""
for f in "${pngs[@]}"; do
    name="$(basename "$f" .png)"
    cell="**${name}**<br>![${name}](${raw}/${name}.png)"
    if [ -z "$row" ]; then
        row="| ${cell} "
    else
        body="${body}
${row}| ${cell} |"
        row=""
    fi
done
[ -n "$row" ] && body="${body}
${row}| |"

# --- 3. upsert the comment --------------------------------------------------
existing="$(gh api "repos/${repo}/issues/${pr}/comments" --paginate \
    --jq "[.[] | select(.body | startswith(\"${marker}\")) | .id][0] // empty")"
if [ -n "$existing" ]; then
    gh api -X PATCH "repos/${repo}/issues/comments/${existing}" \
        -f body="$body" > /dev/null
    echo "updated gallery comment ${existing} on PR #${pr} (images @ ${shots_commit})"
else
    gh api -X POST "repos/${repo}/issues/${pr}/comments" \
        -f body="$body" > /dev/null
    echo "created gallery comment on PR #${pr} (images @ ${shots_commit})"
fi
