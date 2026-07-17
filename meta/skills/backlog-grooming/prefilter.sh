#!/usr/bin/env bash
# Backlog-grooming pre-filter: which open issues could recent landings have
# invalidated?
#
# Deterministic reduction for the weekly groom (see SKILL.md §0-1): instead
# of deep-verifying every open issue, intersect the paths each issue body
# cites with the files changed since the last groom. Only the intersection
# needs model-grade verification; the rest is reported as QUIET (their
# ground truth cannot have moved through code).
#
# Also surfaces, independently of the window:
#   - LABEL-CHECK: status:blocked / status:in-progress issues (labels lie;
#     verify against reality every groom)
#   - MISSING-PATH: issues citing paths that no longer exist at HEAD (a
#     hard staleness signal — the cited code moved or died)
#
# Usage (run from a checkout at the groom's pinned commit):
#   prefilter.sh                     # window: last 7 days (weekly default)
#   prefilter.sh --since "2026-07-10"
#   prefilter.sh --from  <sha>       # everything since that commit
#
# Needs: git, gh (authenticated), jq. Output: markdown on stdout.
set -euo pipefail

REPO="${GROOM_REPO:-bricef/factor-q}"
MODE="since"
WINDOW="7 days ago"
if [ "${1:-}" = "--since" ]; then
    MODE="since"; WINDOW="${2:?--since needs a date}"
elif [ "${1:-}" = "--from" ]; then
    MODE="from"; WINDOW="${2:?--from needs a sha}"
fi

pinned_sha="$(git rev-parse --short HEAD)"

# --- changed files in the window -----------------------------------------
changed="$(mktemp)"
trap 'rm -f "$changed" "$issues" "$tracked"' EXIT
if [ "$MODE" = "from" ]; then
    git diff --name-only "$WINDOW"..HEAD | sort -u > "$changed"
else
    git log --since="$WINDOW" --name-only --pretty=format: | sort -u | sed '/^$/d' > "$changed"
fi

# --- open issues ---------------------------------------------------------
issues="$(mktemp)"
gh issue list -R "$REPO" --state open --limit 200 \
    --json number,title,body,labels > "$issues"

tracked="$(mktemp)"
git ls-files > "$tracked"

# Extract path-like tokens from an issue body: backtick-free file mentions
# with a known extension, or cites rooted at a top-level source dir. Short
# tokens (<6 chars) are dropped as noise.
cited_paths() {
    grep -oE '([A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.(rs|go|md|sh|toml|py|tla|cfg|yml|dot)|(services|adapters|docs|ops|scripts|meta|agents)/[A-Za-z0-9_./-]+' \
        | tr -d '`' | sed 's/[).,;:]*$//' | awk 'length($0) >= 6' | sort -u
}

priority=""
missing=""
quiet=""

count="$(jq length "$issues")"
for i in $(seq 0 $((count - 1))); do
    num="$(jq -r ".[$i].number" "$issues")"
    title="$(jq -r ".[$i].title" "$issues")"
    body="$(jq -r ".[$i].body // \"\"" "$issues")"

    hits=""
    dead=""
    while IFS= read -r p; do
        [ -n "$p" ] || continue
        # A cited path matches a changed file exactly, as a suffix
        # (issues often cite `worker/store.rs` or a bare filename), or
        # as a directory prefix.
        hit="$(grep -F -e "$p" "$changed" | while IFS= read -r c; do
            case "$c" in
                "$p"|*/"$p"|"$p"/*) printf '%s\n' "$c" ;;
            esac
        done | head -3 || true)"
        [ -n "$hit" ] && hits="${hits}${hits:+, }${p}"
        # Path-shaped cites (contain a slash) that resolve to nothing
        # tracked at HEAD are hard-stale.
        case "$p" in
            */*)
                if ! grep -qF -e "$p" "$tracked" \
                    && ! grep -q "/$p\$" "$tracked"; then
                    dead="${dead}${dead:+, }${p}"
                fi
                ;;
        esac
    done <<EOF
$(printf '%s\n' "$body" | cited_paths)
EOF

    if [ -n "$hits" ]; then
        priority="${priority}- #${num} ${title} — touched: ${hits}
"
    else
        quiet="${quiet}#${num} "
    fi
    if [ -n "$dead" ]; then
        missing="${missing}- #${num} ${title} — cites missing: ${dead}
"
    fi
done

labelcheck="$(jq -r '.[] | select([.labels[].name] | any(. == "status:blocked" or . == "status:in-progress"))
    | "- #\(.number) \(.title) — [\([.labels[].name] | join(", "))]"' "$issues")"

cat <<REPORT
# Groom pre-filter — $REPO @ ${pinned_sha} (window: ${MODE} ${WINDOW})

## PRIORITY — cited code changed in the window; deep-verify these
${priority:-- (none)}

## LABEL-CHECK — verify these labels against reality
${labelcheck:-- (none)}

## MISSING-PATH — cite paths absent at HEAD (hard-stale, re-ground)
${missing:-- (none)}

## QUIET — no cited-path intersection; body claims cannot have moved via code this window
${quiet:-"(none)"}
REPORT
