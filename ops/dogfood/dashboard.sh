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
# The dashboard is its own crash domain: if the daemon is down it
# renders "runtime unreachable" rather than exiting, and killing it
# never affects the daemon. deploy.sh stops and relaunches it with the
# daemon — it must run the same build, because the two share the
# contract types they exchange (fq_runtime::surface, fq_runtime::views)
# and a field removed on one side is a decode failure on the other.
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

# Read the declared environment; do not become it. `.secrets/env` holds
# every secret the instance has — the provider keys, GH_TOKEN — and this
# is the one web-facing process. The other launchers `set -a` the whole
# file into their process; this one sources it only to read values and
# then starts the dashboard under `env -i`, carrying exactly the
# variables the binary reads (its three edge settings and its own
# FQ_DASHBOARD_* tuning) plus PATH, and nothing else
# (https://github.com/bricef/factor-q/issues/545). An attenuated token
# in a process that also holds the admin-grade secrets would make the
# attenuation decorative.
# shellcheck disable=SC1091
. ./.secrets/env

: "${FQ_EDGE:?set it in .secrets/env to the [edge] bind — see ops/dogfood/README.md}"
: "${FQ_EDGE_TOKEN:?set it in .secrets/env — an ATTENUATED token, see env.example}"
: "${FQ_EDGE_FINGERPRINT:?set it in .secrets/env — the certificate fingerprint the daemon printed}"

exec env -i \
  PATH="$PATH" \
  FQ_EDGE="$FQ_EDGE" \
  FQ_EDGE_TOKEN="$FQ_EDGE_TOKEN" \
  FQ_EDGE_FINGERPRINT="$FQ_EDGE_FINGERPRINT" \
  FQ_DASHBOARD_BIND="${FQ_DASHBOARD_BIND:-127.0.0.1:9472}" \
  FQ_DASHBOARD_REFRESH="${FQ_DASHBOARD_REFRESH:-5}" \
  ./current/fq-dashboard
