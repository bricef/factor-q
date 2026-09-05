#!/usr/bin/env bash
# ops/dogfood/backup.sh — a consistent copy of the instance: the daemon's
# volume (minus the regenerable build/ and the per-invocation workspace/)
# and the broker's JetStream store, as two tarballs under
# $FQ_DOGFOOD/backups/<utc-stamp>/. Nightly from ops/dogfood/crontab.
#
#   backup.sh             stop, copy, start — a minute or two of downtime,
#                         the daemon drained on the way down (ADR-0027)
#   backup.sh --auto      unattended: defer when an invocation is in flight
#
# Both stores are copied with their writers stopped: SQLite and JetStream
# files copied under a live writer are not guaranteed consistent, and a
# backup that might not restore is not one. The proxy and the dashboard
# stay up ("runtime unreachable" for the interval). Restores are
# restore.sh, and the drill is its own section in the README.
#
# Knobs (in .env): FQ_BACKUP_DIR (default $FQ_DOGFOOD/backups),
# FQ_BACKUP_KEEP (default 7 sets), FQ_BACKUP_HOOK — a command run with the
# finished set's directory as $1, for the off-host copy (rclone, scp, an
# object-store CLI); the on-host copy alone does not survive the host.
# hygiene.sh warns when the newest set is older than FQ_BACKUP_STALE_HOURS.
set -euo pipefail

DOGFOOD="${FQ_DOGFOOD:-$HOME/fq-dogfood}"
cd "$DOGFOOD" 2>/dev/null || { echo "backup: dogfood dir not found: $DOGFOOD" >&2; exit 2; }
[ -f compose.yml ] || { echo "backup: no compose.yml in $DOGFOOD" >&2; exit 2; }
AUTO=0; [ "${1:-}" = "--auto" ] && AUTO=1
envval() { sed -n "s/^$1=\(.*\)$/\1/p" .env 2>/dev/null | tail -1; }
OUT_ROOT="${FQ_BACKUP_DIR:-$(envval FQ_BACKUP_DIR)}"; OUT_ROOT="${OUT_ROOT:-$DOGFOOD/backups}"
KEEP="${FQ_BACKUP_KEEP:-$(envval FQ_BACKUP_KEEP)}"; KEEP="${KEEP:-7}"
HOOK="${FQ_BACKUP_HOOK:-$(envval FQ_BACKUP_HOOK)}"
PROJECT="$(docker compose config --format json 2>/dev/null | sed -n 's/.*"name": *"\([^"]*\)".*/\1/p' | head -1)"; PROJECT="${PROJECT:-fq-dogfood}"

now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }
say() { printf '%s %s\n' "$(now)" "$*"; }
# Unattended, a failed backup is told to a human through notify.sh
# (FQ_NOTIFY_HOOK): a nightly that fails quietly is the same as no backup.
die() {
    printf '%s ERROR: %s\n' "$(now)" "$*" >&2
    [ "$AUTO" = 1 ] && [ -x "$DOGFOOD/notify.sh" ] && { printf '%s\n' "$*" | "$DOGFOOD/notify.sh" "backup FAILED" || true; }
    exit 1
}

# Share deploy.sh's lock: a backup and a deploy must not interleave.
exec 9>"$DOGFOOD/.deploy.lock"
flock -n 9 || { [ "$AUTO" = 1 ] && { say "a deploy or backup is running — skipping"; exit 0; }; die "another deploy or backup holds $DOGFOOD/.deploy.lock"; }

if [ "$AUTO" = 1 ] && [ -n "$(docker compose ps -q --status running fqd 2>/dev/null)" ]; then
    inflight="$(if out="$(docker compose exec -T fqd fq invocation list --status in_flight --json 2>/dev/null)"; then printf '%s' "$out" | { grep -o '"invocation_id"' || true; } | wc -l | tr -dc '0-9'; else echo unknown; fi)"
    [ "$inflight" = "0" ] || { say "${inflight} invocation(s) in flight (or the daemon cannot be asked) — deferring the backup"; exit 0; }
fi

STAMP="$(date -u '+%Y%m%dT%H%M%SZ')"
OUT="$OUT_ROOT/$STAMP"
mkdir -p "$OUT"

# Which services were up, so exactly those come back.
was_up="$(docker compose ps --status running --format '{{.Service}}' 2>/dev/null | tr '\n' ' ')"
say "stopping the writers (drain) — was up: ${was_up:-nothing}"
docker compose stop fq-cron fqd github-watcher nats >/dev/null 2>&1 || die "docker compose stop failed"

# The copies run in one-off containers on the fq-dogfood image (it has tar),
# as root so the output directory needs no special ownership. The daemon's
# volume is where the fqd service mounts it; the broker's is attached
# explicitly, read-only.
say "copying the instance volume (without build/ and workspace/)"
docker compose run --rm --no-deps --user root -v "$OUT:/out" --entrypoint tar fqd \
    -czf /out/fq-data.tgz -C /var/lib/factor-q --exclude=./build --exclude=./workspace . \
    || die "instance volume copy failed"
say "copying the event log"
docker compose run --rm --no-deps --user root -v "$OUT:/out" -v "${PROJECT}_nats-data:/nats:ro" --entrypoint tar fqd \
    -czf /out/nats-data.tgz -C /nats . \
    || die "event log copy failed"
( cd "$OUT" && sha256sum fq-data.tgz nats-data.tgz > SHA256SUMS ) || die "checksum failed"
{ echo "taken=$STAMP"; echo "fq_tag=$(envval FQ_TAG)"; echo "project=$PROJECT"; echo "was_up=$was_up"; } > "$OUT/MANIFEST"

say "starting the stack"
docker compose up -d >/dev/null 2>&1 || die "docker compose up failed after the backup — the stack needs a human"

say "backup set: $OUT ($(du -sh "$OUT" | cut -f1))"

# Retention on the host copy; the hook is the off-host copy.
i=0
for d in $(ls -1d "$OUT_ROOT"/*/ 2>/dev/null | sort -r); do
    i=$((i + 1)); [ "$i" -le "$KEEP" ] && continue
    rm -rf "$d" && say "pruned old set $(basename "$d")"
done
if [ -n "$HOOK" ]; then
    say "running FQ_BACKUP_HOOK"
    $HOOK "$OUT" || die "FQ_BACKUP_HOOK failed for $OUT (the on-host set is intact)"
fi
