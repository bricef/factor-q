#!/bin/sh
# Launch the github-watcher for the dogfood instance, from the active
# release. Ships inside the main-latest artifact bundle (#102); deploy.sh
# invokes it via the `current` symlink:
#
#   setsid "$FQ_DOGFOOD/current/watcher.sh" >> logs/watcher.log 2>&1 </dev/null &
#
# The process environment comes ONLY from $FQ_DOGFOOD/.secrets/env (which
# must provide GH_TOKEN — see env.example for the rotation trade-off).
# Labels / retries use the #15 defaults (ready -> in-progress ->
# in-review/failed -> done).
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

set -a
. ./.secrets/env
set +a

exec ./current/github-watcher \
  --repo "${FQ_WATCH_REPO:-bricef/factor-q}" \
  --agent "${FQ_WATCH_AGENT:-m0-issue-fix}" \
  --nats-url "${FQ_NATS_URL:-nats://127.0.0.1:4223}" \
  --poll "${FQ_WATCH_POLL:-60s}"
