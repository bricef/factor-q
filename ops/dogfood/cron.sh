#!/bin/sh
# Launch fq-cron for the dogfood instance from the active release. This
# launcher ships inside the artifact bundle and writes to logs/cron.log.
# The process environment comes ONLY from $FQ_DOGFOOD/.secrets/env.
set -eu

FQ_DOGFOOD="${FQ_DOGFOOD:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$FQ_DOGFOOD"

set -a
. ./.secrets/env
set +a

exec ./current/fq-cron \
  --config ./fq-cron.toml \
  --nats-url "${FQCRON_NATS_URL:-nats://127.0.0.1:4223}" \
  >> logs/cron.log 2>&1
