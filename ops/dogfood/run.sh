#!/bin/sh
# Launch the factor-q daemon for the dogfood instance, from the active
# release. Ships inside the main-latest artifact bundle (#102) so the
# launcher is versioned with the binary it launches; deploy.sh invokes it
# via the `current` symlink:
#
#   setsid "$FQ_DOGFOOD/current/run.sh" >> logs/fq-run.log 2>&1 </dev/null &
#
# Brings up the instance's own NATS (port 4223 — separate from the repo's
# dev/test broker on 4222) before starting the daemon. The process
# environment comes ONLY from $FQ_DOGFOOD/.secrets/env — one declared
# file, no ambient lookups here (#102).
set -eu

# Resolves to the dogfood root when invoked as <dogfood>/current/run.sh.
FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

docker compose -f infra/docker-compose.yml up -d
timeout 60 sh -c 'until curl -sf http://127.0.0.1:8223/healthz >/dev/null 2>&1; do sleep 1; done'

set -a
. ./.secrets/env
set +a

exec ./current/fq run
