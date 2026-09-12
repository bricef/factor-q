#!/usr/bin/env bash
# The image's schedule holds the invariant ADR-0036 clause 3 rests on:
# every job is a one-shot sibling container — `docker compose run --rm
# -T --no-deps ops <verb>` — never a command in the scheduler's own
# process, so the deploy's `up` can recreate the scheduler mid-job. And
# the three jobs the ADR names are all there. Whether the file parses is
# supercronic's own `-test`, run by `fq-ops check` under `just
# docker-check`.
#
#   ops/dogfood/tests/ops-crontab.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crontab="$here/../ops.crontab"
failed=0
jobs="$(grep -vE '^\s*(#|$)' "$crontab")"

n="$(printf '%s\n' "$jobs" | wc -l | tr -dc '0-9')"
[ "$n" = 3 ] && printf '  ok   three jobs\n' || { printf '  FAIL %s jobs, not 3\n' "$n"; failed=1; }

bad="$(printf '%s\n' "$jobs" | grep -vE '^([^ ]+ +){5}docker compose run --rm -T --no-deps ops (deploy|hygiene|backup|restore|notify)( |$)' || true)"
[ -z "$bad" ] && printf '  ok   every job is a one-shot sibling of the ops service, running a known verb\n' \
    || { printf '  FAIL a job is not a sibling run of a known verb:\n%s\n' "$bad" | sed 's/^/       /'; failed=1; }

for want in 'ops deploy --auto' 'ops hygiene' 'ops backup --auto'; do
    if printf '%s\n' "$jobs" | grep -qF -- "$want"; then printf '  ok   schedules %s\n' "$want"
    else printf '  FAIL does not schedule %s\n' "$want"; failed=1; fi
done

[ "$failed" = 0 ] && echo "ops-crontab: all cases pass" || { echo "ops-crontab: FAILED" >&2; exit 1; }
