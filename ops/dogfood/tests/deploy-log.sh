#!/usr/bin/env bash
# An unattended deploy keeps its own log on the bind mount. Every other
# scheduled job's output lives in the scheduler's container log, but the
# deploy is the job that recreates that container (its `up`) and runs in
# a one-off that `--rm` removes on exit — so, run from the schedule, a
# deploy that succeeded left no line anywhere but its notification. Since
# 2026-09-13 `deploy --auto` appends to logs/deploy.log instead, and a
# run by hand still talks to its terminal. Driven against an empty
# instance directory: the first thing the script says once inside it —
# that there is no compose.yml — is where each mode's output must land.
#
#   ops/dogfood/tests/deploy-log.sh        (just ops-ci)
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
deploy="$here/../deploy.sh"
failed=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# --auto: nothing on stdout or stderr, the line in logs/deploy.log.
out="$(FQ_DOGFOOD="$tmp" bash "$deploy" --auto 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && [ -z "$out" ] && grep -q 'no compose.yml' "$tmp/logs/deploy.log" 2>/dev/null; then
    printf '  ok   --auto writes logs/deploy.log and nothing to the scheduler'"'"'s stream\n'
else
    printf '  FAIL --auto: rc=%s, stream=%q, deploy.log=%q\n' "$rc" "$out" "$(cat "$tmp/logs/deploy.log" 2>/dev/null)"; failed=1
fi

# By hand: the terminal gets it, and no log file appears.
rm -rf "$tmp/logs"
out="$(FQ_DOGFOOD="$tmp" bash "$deploy" 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q 'no compose.yml' && [ ! -e "$tmp/logs/deploy.log" ]; then
    printf '  ok   by hand, the terminal is the log\n'
else
    printf '  FAIL by hand: rc=%s, stream=%q, deploy.log present=%s\n' "$rc" "$out" "$([ -e "$tmp/logs/deploy.log" ] && echo yes || echo no)"; failed=1
fi

[ "$failed" = 0 ] && echo "deploy-log: all cases pass" || { echo "deploy-log: FAILED" >&2; exit 1; }
