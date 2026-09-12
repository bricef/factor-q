#!/usr/bin/env bash
# The deploy message's change list, rendered from canned commit subjects
# through `deploy.sh --render-changes` — the one seam in a script that
# otherwise needs a compose stack to run. Each case is a subjects list
# on stdin and the exact body expected; a difference prints as a diff.
#
#   ops/dogfood/tests/render-changes.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
deploy="$here/../deploy.sh"
failed=0

check() {  # check <name> <from> <to> <repo> <<< subjects; expected on fd 3
    local name="$1" from="$2" to="$3" repo="$4" got want
    got="$(bash "$deploy" --render-changes "$from" "$to" "$repo")"
    want="$(cat <&3)"
    if [ "$got" = "$want" ]; then
        printf '  ok   %s\n' "$name"
    else
        printf '  FAIL %s\n' "$name"
        diff <(printf '%s\n' "$want") <(printf '%s\n' "$got") | sed 's/^/       /'
        failed=1
    fi
}

check "user-facing listed, the rest counted by type" aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q 3<<'WANT' <<'GOT'
from aaaaaaaaaaaa · 6 commits
• feat(dashboard): icon set and manifest for the phone home screen
• feat(dashboard): phone layout under one media query
• fix: the watcher reads v3 completion events again
+3 other: docs 2, refactor 1
https://github.com/bricef/factor-q/compare/aaaaaaaaaaaa...bbbbbbbbbbbb
WANT
feat(dashboard): icon set and manifest for the phone home screen
feat(dashboard): phone layout under one media query
refactor(dashboard): move the stylesheet into render/style.rs
docs: STATUS.md says the watcher outcome observation is broken
fix: the watcher reads v3 completion events again
docs(ops): the dogfood runbook after the move
GOT

check "nothing user-facing says so" aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q 3<<'WANT' <<'GOT'
from aaaaaaaaaaaa · 4 commits
no user-facing changes; docs 2, chore 1, ops/dogfood 1
https://github.com/bricef/factor-q/compare/aaaaaaaaaaaa...bbbbbbbbbbbb
WANT
docs: one
chore: cargo fmt
ops/dogfood: bootstrap from a checkout lays out the checkout as it is
docs: two
GOT

check "a single commit, no type prefix, counts as other" aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q 3<<'WANT' <<'GOT'
from aaaaaaaaaaaa · 1 commit
no user-facing changes; other 1
https://github.com/bricef/factor-q/compare/aaaaaaaaaaaa...bbbbbbbbbbbb
WANT
Merge branch 'hotfix' into main
GOT

check "breaking and scoped forms are user-facing; more than six folds" aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q 3<<'WANT' <<'GOT'
from aaaaaaaaaaaa · 8 commits
• feat!: one
• fix(edge)!: two
• perf(store): three
• feat: four
• fix: five
• feat(x): six
+2 more user-facing
https://github.com/bricef/factor-q/compare/aaaaaaaaaaaa...bbbbbbbbbbbb
WANT
feat!: one
fix(edge)!: two
perf(store): three
feat: four
fix: five
feat(x): six
feat: seven
fix: eight
GOT

check "no commits between the two builds" aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q 3<<'WANT' <<'GOT'
no commits between aaaaaaaaaaaa and bbbbbbbbbbbb — the same build, or a step back
https://github.com/bricef/factor-q/compare/aaaaaaaaaaaa...bbbbbbbbbbbb
WANT
GOT

check "lookalikes are not user-facing: feature/, fixture, performance" aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q 3<<'WANT' <<'GOT'
from aaaaaaaaaaaa · 3 commits
no user-facing changes; other 3
https://github.com/bricef/factor-q/compare/aaaaaaaaaaaa...bbbbbbbbbbbb
WANT
feature/foo landed
fixture data for the smoke test
performance notes
GOT

# A subject is cut at a hundred characters, and the whole body at nine
# hundred, so a long list never trips Pushover's 1024 with the signature
# notify.sh adds.
long="$(printf 'feat: %0130d' 0 | tr 0 x)"
got="$(printf '%s\n' "$long" | bash "$deploy" --render-changes aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q)"
line="$(printf '%s\n' "$got" | sed -n 2p)"
if [ "${#line}" -eq 102 ]; then printf '  ok   a subject is cut at 100 characters\n'; else printf '  FAIL subject cut: %s chars\n' "${#line}"; failed=1; fi
got="$(for i in $(seq 1 40); do printf 'feat: %s %0090d\n' "$i" 0; done | bash "$deploy" --render-changes aaaaaaaaaaaa bbbbbbbbbbbb bricef/factor-q)"
if [ "${#got}" -le 900 ]; then printf '  ok   the body stops at 900 characters (%s)\n' "${#got}"; else printf '  FAIL body is %s chars\n' "${#got}"; failed=1; fi

if bash "$deploy" --render-changes a b >/dev/null 2>&1; then printf '  FAIL --render-changes with too few arguments exited 0\n'; failed=1; else printf '  ok   --render-changes wants exactly three arguments\n'; fi

[ "$failed" = 0 ] && echo "render-changes: all cases pass" || { echo "render-changes: FAILED" >&2; exit 1; }
