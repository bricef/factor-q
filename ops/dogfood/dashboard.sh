#!/bin/sh
# Launch the operator dashboard for the dogfood instance, from the
# active release (#105 layer 3). Ships inside the artifact bundle like
# run.sh / watcher.sh; start it detached:
#
#   setsid "$FQ_DOGFOOD/current/dashboard.sh" >> logs/dashboard.log 2>&1 </dev/null &
#
# The dashboard reads over the daemon's authenticated edge and needs an
# identity of its own: FQ_EDGE, FQ_EDGE_FINGERPRINT and FQ_EDGE_TOKEN
# in .secrets/env (see env.example for how to mint the token — it is an
# ATTENUATED token, never the admin one). The binary refuses to start
# without them and prints the `fq token attenuate` line that fixes it.
#
# FQ_EDGE has no default here on purpose: `[edge] bind` defaults to
# 127.0.0.1:9472, which is the port this process serves on, so on a host
# running both the daemon's edge must be moved and named explicitly.
#
# Still requires `[read_service] enabled = true` in fq.toml for the
# "Active now" table alone — the one surface with no declared operation
# yet. Everything else on the dashboard rides the edge.
#
# The dashboard is its own crash domain: if the daemon is down it
# renders "runtime unreachable" rather than exiting, and killing it
# never affects the daemon. deploy.sh stops and relaunches it with the
# daemon — it must run the same build, because the two share the
# contract types they exchange (fq_runtime::surface, fq_runtime::views)
# and a field removed on one side is a decode failure on the other.
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

set -a
. ./.secrets/env
set +a

exec ./current/fq-dashboard \
  --bind "${FQ_DASHBOARD_BIND:-127.0.0.1:9472}" \
  --edge "${FQ_EDGE:?set it in .secrets/env to the [edge] bind — see ops/dogfood/README.md}" \
  --read-service "${FQ_READ_SERVICE:-127.0.0.1:9471}" \
  --refresh "${FQ_DASHBOARD_REFRESH:-5}"
