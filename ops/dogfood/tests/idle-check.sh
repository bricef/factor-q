#!/usr/bin/env bash
# The "is the daemon idle?" question the unattended scripts ask before
# they stop or prune anything must be put to `fq doctor --json`'s live
# execution count, never to `fq invocation list --status in_flight`: the
# ownership table that listing reads is not written on dispatch, so it
# answered "idle" while trigger-dispatched agents were mid-tool and the
# deploy's drain killed them (#721). This pins the surface each script
# asks, and the jq that reads the answer — with the daemon absent, an
# unparseable answer must read as "unknown", never as 0.
#
#   ops/dogfood/tests/idle-check.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
failed=0

jq_expr="jq -rn '(try input catch {}) | .executions.in_flight // \"unknown\"'"
code="$(mktemp)"
trap 'rm -f "$code"' EXIT
for s in deploy.sh backup.sh hygiene.sh; do
    f="$here/../$s"
    # The comments are allowed to name the old form; only code is judged.
    # Grepped from a file, not a pipe: `grep -q` may close a pipe before
    # the writer is done, which pipefail reports as a miss.
    grep -v '^[[:space:]]*#' "$f" > "$code"
    if grep -q 'invocation list --status in_flight' "$code"; then
        printf '  FAIL %s still asks `invocation list --status in_flight` (#721)\n' "$s"; failed=1
    elif grep -q 'fq doctor --json' "$code" && grep -qF "$jq_expr" "$code"; then
        printf '  ok   %s asks fq doctor and reads executions.in_flight\n' "$s"
    else
        printf '  FAIL %s has no idle check this test recognises\n' "$s"; failed=1
    fi
done

# The jq the scripts share, on the shapes it meets.
read_count() { printf '%s' "$1" | jq -rn '(try input catch {}) | .executions.in_flight // "unknown"' 2>/dev/null || echo unknown; }
check() {  # check <name> <want> <json>
    local got; got="$(read_count "$3")"
    if [ "$got" = "$2" ]; then printf '  ok   %s\n' "$1"
    else printf '  FAIL %s: want %s, got %s\n' "$1" "$2" "$got"; failed=1; fi
}
check "a busy daemon counts, stuck included"   3         '{"executions":{"in_flight":3,"working":1,"stuck":2}}'
check "an idle daemon is 0, not unknown"       0         '{"executions":{"in_flight":0,"working":0,"stuck":0}}'
check "a report with no executions is unknown" unknown   '{"reachable":true}'
check "garbage is unknown"                     unknown   'runtime unreachable'
check "empty is unknown"                       unknown   ''
check "a bare number is unknown"               unknown   '5'
check "an array is unknown"                    unknown   '[1]'

[ "$failed" = 0 ] && echo "idle-check: all cases pass" || { echo "idle-check: FAILED" >&2; exit 1; }
