#!/bin/sh
# Launch the operator dashboard for the dogfood instance, from the
# active release (#105 layer 3). Ships inside the artifact bundle like
# run.sh / watcher.sh; start it detached:
#
#   setsid "$FQ_DOGFOOD/current/dashboard.sh" >> logs/dashboard.log 2>&1 </dev/null &
#
# Requires `[read_service] enabled = true` in fq.toml (and a daemon
# restart to pick it up). The dashboard is its own crash domain: if the
# daemon is down it renders "runtime unreachable" rather than exiting,
# and killing it never affects the daemon. deploy.sh stops and
# relaunches it with the daemon — it must run the same build, because
# the read-service RPC is a length-framed binary codec and a
# cross-build dashboard fails to decode responses (the #154-skew
# incident).
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

set -a
. ./.secrets/env
set +a

exec ./current/fq-dashboard \
  --bind "${FQ_DASHBOARD_BIND:-127.0.0.1:9472}" \
  --read-service "${FQ_READ_SERVICE:-127.0.0.1:9471}" \
  --refresh "${FQ_DASHBOARD_REFRESH:-5}"
