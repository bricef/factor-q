#!/usr/bin/env bash
# ops/dogfood/restore.sh — put a backup set back: the instance volume and
# the broker's JetStream store, from the two tarballs backup.sh writes.
# The restore drill (README) is this script on a freshly bootstrapped host.
#
#   restore.sh <backup-dir>        refuse if either volume already has content
#   restore.sh <backup-dir> --yes  overwrite existing volume content
#
# The stack is taken down first (containers removed, volumes kept), the
# tarballs are checked against SHA256SUMS, the volumes are created if
# missing and filled by one-off containers on the fq-dogfood image running
# as root, ownership is set to the runtime user, and the stack comes up on
# the tag the backup was taken at (FQ_TAG in its MANIFEST) — unless .env
# already names a tag, which is kept.
set -euo pipefail

DOGFOOD="${FQ_DOGFOOD:-$HOME/fq-dogfood}"
SET="${1:-}"; YES=0; [ "${2:-}" = "--yes" ] && YES=1
[ -n "$SET" ] && [ -d "$SET" ] || { echo "usage: restore.sh <backup-dir> [--yes]" >&2; exit 2; }
SET="$(cd "$SET" && pwd)"
cd "$DOGFOOD" 2>/dev/null || { echo "restore: dogfood dir not found: $DOGFOOD" >&2; exit 2; }
[ -f compose.yml ] || { echo "restore: no compose.yml in $DOGFOOD — bootstrap first" >&2; exit 2; }
[ -f .env ] || { echo "restore: no .env in $DOGFOOD — bootstrap first" >&2; exit 2; }
for f in fq-data.tgz nats-data.tgz SHA256SUMS; do [ -f "$SET/$f" ] || { echo "restore: $SET has no $f" >&2; exit 2; }; done
PROJECT="$(docker compose config --format json 2>/dev/null | sed -n 's/.*"name": *"\([^"]*\)".*/\1/p' | head -1)"; PROJECT="${PROJECT:-fq-dogfood}"

now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }
say() { printf '%s %s\n' "$(now)" "$*"; }
die() { printf '%s ERROR: %s\n' "$(now)" "$*" >&2; exit 1; }

exec 9>"$DOGFOOD/.deploy.lock"
flock -n 9 || die "another deploy or backup holds $DOGFOOD/.deploy.lock"

( cd "$SET" && sha256sum -c --quiet SHA256SUMS ) || die "checksums do not match in $SET"
say "checksums verified"

# A tag to come up on: .env's if set, else the backup's.
if [ -z "$(sed -n 's/^FQ_TAG=\(.*\)$/\1/p' .env | tail -1)" ]; then
    tag="$(sed -n 's/^fq_tag=//p' "$SET/MANIFEST" 2>/dev/null || true)"
    [ -n "$tag" ] || die "no FQ_TAG in .env and none in the backup's MANIFEST — set one"
    if grep -q '^FQ_TAG=' .env; then sed -i "s/^FQ_TAG=.*/FQ_TAG=$tag/" .env; else printf 'FQ_TAG=%s\n' "$tag" >> .env; fi
    say ".env: FQ_TAG=$tag (from the backup's manifest)"
fi

say "taking the stack down (volumes kept)"
docker compose down >/dev/null 2>&1 || true
docker volume create "${PROJECT}_fq-data" >/dev/null
docker volume create "${PROJECT}_nats-data" >/dev/null

# Refuse to overwrite a live instance unless told to.
occupied() { docker compose run --rm --no-deps --user root -v "$1:/probe:ro" --entrypoint sh fqd -c 'ls -A /probe | grep -vxE "lost\+found|agents|state|cache|workspace|build|home" | head -1; for d in state cache; do [ -d /probe/$d ] && [ -n "$(ls -A /probe/$d)" ] && echo "$d"; done' 2>/dev/null | head -1; }
if [ "$YES" != 1 ]; then
    for v in "${PROJECT}_fq-data" "${PROJECT}_nats-data"; do
        [ -z "$(occupied "$v")" ] || die "$v already has content — restore.sh $SET --yes to overwrite it"
    done
fi

say "restoring the instance volume"
docker compose run --rm --no-deps --user root -v "$SET:/set:ro" --entrypoint sh fqd -c \
    'cd /var/lib/factor-q && find . -mindepth 1 -maxdepth 1 ! -name build ! -name workspace -exec rm -rf {} + && tar -xzf /set/fq-data.tgz -C /var/lib/factor-q && mkdir -p build workspace && chown -R 65532:65532 /var/lib/factor-q' \
    || die "instance volume restore failed"
say "restoring the event log"
docker compose run --rm --no-deps --user root -v "$SET:/set:ro" -v "${PROJECT}_nats-data:/nats" --entrypoint sh fqd -c \
    'rm -rf /nats/* && tar -xzf /set/nats-data.tgz -C /nats' \
    || die "event log restore failed"

say "bringing the stack up"
docker compose up -d >/dev/null 2>&1 || die "docker compose up failed"
say "restored $SET — now: docker compose ps; docker compose exec fqd fq status (the pairing came back with state/client/)"
