#!/bin/sh
# Launch the github-watcher for the dogfood instance, from the active
# release. Ships inside the main-latest artifact bundle (#102); deploy.sh
# invokes it via the `current` symlink:
#
#   setsid "$FQ_DOGFOOD/current/watcher.sh" >> logs/watcher.log 2>&1 </dev/null &
#
# The process environment comes ONLY from $FQ_DOGFOOD/.secrets/env (which
# must provide GH_TOKEN — see env.example for the rotation trade-off).
# Lifecycle labels use the status: convention (status:ready ->
# status:in-progress -> status:in-review/status:failed -> status:done),
# the watcher default; override per-label via GHW_*_LABEL in .secrets/env.
#
# The broker URL reaches the binary through GHW_NATS_URL, never argv: the
# instance broker is token-authenticated (#542) and the token rides in the
# URL's userinfo, so a `--nats-url` flag would publish it to every `ps` on
# the host. The binary reads the variable itself; this only supplies the
# instance default. It is the watcher's own variable — FQ_NATS_URL is the
# daemon's, and the daemon refuses one that carries a credential (#540).
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

set -a
. ./.secrets/env
set +a

export GHW_NATS_URL="${GHW_NATS_URL:-nats://127.0.0.1:4223}"

exec ./current/github-watcher \
  --repo "${FQ_WATCH_REPO:-bricef/factor-q}" \
  --agent "${FQ_WATCH_AGENT:-m0-issue-fix}" \
  --poll "${FQ_WATCH_POLL:-60s}"
