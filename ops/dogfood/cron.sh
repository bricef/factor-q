#!/bin/sh
# Launch fq-cron for the dogfood instance from the active release. This
# launcher ships inside the artifact bundle and writes to logs/cron.log.
# The process environment comes ONLY from $FQ_DOGFOOD/.secrets/env.
#
# The broker URL reaches the binary through FQCRON_NATS_URL, never argv:
# the instance broker is token-authenticated (#542) and the token rides
# in the URL's userinfo, so a `--nats-url` flag would publish it to every
# `ps` on the host. The binary reads the variable itself; this only
# supplies the instance default.
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

set -a
. ./.secrets/env
set +a

export FQCRON_NATS_URL="${FQCRON_NATS_URL:-nats://127.0.0.1:4223}"

exec ./current/fq-cron \
  --config ./fq-cron.toml \
  >> logs/cron.log 2>&1
