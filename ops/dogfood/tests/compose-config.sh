#!/usr/bin/env bash
# The stack definition, resolved by `docker compose config` over a stub
# instance directory, and the facts ADR-0036 fixes about it checked on
# the result: the `ops` service runs the fq-ops image at the stack's tag
# as the deploy user with the docker group, mounts exactly the runtime's
# socket and the instance directory at its own path, works in that
# directory, carries the host's name — and NO other service mounts the
# socket (clause 5: the daemon's container never does). Needs the
# compose plugin, no daemon.
#
#   ops/dogfood/tests/compose-config.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$here/../compose.yml" "$work/compose.yml"
mkdir -p "$work/.secrets" "$work/infra"
: > "$work/.secrets/env"; : > "$work/.secrets/dashboard.env"; : > "$work/.secrets/caddy.env"
: > "$work/infra/nats.conf"; : > "$work/infra/Caddyfile"; : > "$work/.secrets/nats-auth.conf"
printf 'FQ_TAG=abcdef123456\nFQ_DOGFOOD=%s\nFQ_UID=1234\nFQ_DOCKER_GID=987\nFQ_HOST=stub-host\n' "$work" > "$work/.env"

if ! json="$(cd "$work" && docker compose config --format json 2>&1)"; then
    echo "compose config failed:" >&2; printf '%s\n' "$json" >&2; exit 1
fi
failed=0
expect() {  # expect <name> <jq filter that must be true>
    if jq -e "$2" <<<"$json" >/dev/null 2>&1; then printf '  ok   %s\n' "$1"
    else printf '  FAIL %s\n       %s\n' "$1" "$2"; failed=1; fi
}
expect "the ops service runs the fq-ops image at the stack's tag" '.services.ops.image | endswith("/fq-ops:abcdef123456")'
expect "as the deploy user with the docker group" '.services.ops.user == "1234:987"'
expect "with the host's name" '.services.ops.hostname == "stub-host"'
expect "working in the instance directory" ".services.ops.working_dir == \"$work\""
expect "mounting the runtime's socket" '[.services.ops.volumes[] | select(.source == "/var/run/docker.sock" and .target == "/var/run/docker.sock")] | length == 1'
expect "mounting the instance directory at its own path" "[.services.ops.volumes[] | select(.source == \"$work\" and .target == \"$work\")] | length == 1"
expect "and nothing else" '.services.ops.volumes | length == 2'
expect "restarting unless stopped, under an init" '.services.ops.restart == "unless-stopped" and .services.ops.init == true'
expect "no other service mounts the socket (ADR-0036 clause 5)" '[.services | to_entries[] | select(.key != "ops") | .value.volumes // [] | .[] | select(.source == "/var/run/docker.sock")] | length == 0'
expect "the daemon's container mounts only its volume" '[.services.fqd.volumes[] | .source] == ["fq-data"]'

[ "$failed" = 0 ] && echo "compose-config: all cases pass" || { echo "compose-config: FAILED" >&2; exit 1; }
