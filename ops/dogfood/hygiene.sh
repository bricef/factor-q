#!/usr/bin/env bash
# ops/dogfood/hygiene.sh — the host's periodic check (ops/dogfood/crontab,
# every 30 minutes): disk, the instance volume's subtrees, a bounded build
# cache, dangling images. Prints a short report; exits non-zero when a
# threshold is crossed, so cron mails the deploy user and the log shows a
# red line. Nothing here touches the stores, the identity or a workspace.
#
#   hygiene.sh            report, and prune what is over its bound
#   hygiene.sh --report   report only
#
# Knobs (in $FQ_DOGFOOD/.env): FQ_DISK_WARN_PCT (default 80) — warn when
# docker's data root is fuller than this; FQ_BUILD_CACHE_MAX_GB (default
# 60) — above this the daemon's build/ subtree (cargo target, sccache, go
# caches — all regenerable) is emptied, but only while no invocation is in
# flight, since a running build would lose its tree from under it.
#
# Workspaces are reported, never deleted: reclaiming a terminal
# invocation's workspace is the daemon's job (#367), and this script
# cannot tell a suspended invocation's directory from a dead one.
set -euo pipefail

DOGFOOD="${FQ_DOGFOOD:-$HOME/fq-dogfood}"
cd "$DOGFOOD" 2>/dev/null || { echo "hygiene: dogfood dir not found: $DOGFOOD" >&2; exit 2; }
[ -f compose.yml ] || { echo "hygiene: no compose.yml in $DOGFOOD" >&2; exit 2; }

REPORT_ONLY=0; [ "${1:-}" = "--report" ] && REPORT_ONLY=1
envval() { sed -n "s/^$1=\(.*\)$/\1/p" .env 2>/dev/null | tail -1; }
WARN_PCT="${FQ_DISK_WARN_PCT:-$(envval FQ_DISK_WARN_PCT)}"; WARN_PCT="${WARN_PCT:-80}"
CACHE_MAX_GB="${FQ_BUILD_CACHE_MAX_GB:-$(envval FQ_BUILD_CACHE_MAX_GB)}"; CACHE_MAX_GB="${CACHE_MAX_GB:-60}"

now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }
say()  { printf '%s %s\n' "$(now)" "$*"; }
warn() { printf '%s WARNING: %s\n' "$(now)" "$*"; RC=1; }
RC=0

# --- 1. the disk docker lives on -------------------------------------------------
root="$(docker info --format '{{.DockerRootDir}}' 2>/dev/null || echo /var/lib/docker)"
used="$(df --output=pcent "$root" 2>/dev/null | tail -1 | tr -dc '0-9')"
avail="$(df -h --output=avail "$root" 2>/dev/null | tail -1 | tr -d ' ')"
if [ -n "$used" ]; then
    if [ "$used" -ge "$WARN_PCT" ]; then
        warn "disk holding $root is ${used}% full (${avail} free; threshold ${WARN_PCT}%) — a full disk kills the daemon and the broker"
    else
        say "disk: ${used}% used, ${avail} free ($root)"
    fi
fi
say "docker: $(docker system df --format '{{.Type}} {{.Size}} (reclaimable {{.Reclaimable}})' 2>/dev/null | tr '\n' ';' | sed 's/;$//;s/;/; /g')"

# --- 2. the instance volume, by subtree (through the daemon's container) ----------
fqd="$(docker compose ps -q --status running fqd 2>/dev/null | head -1)"
if [ -z "$fqd" ]; then
    say "daemon not running — volume report and cache bound skipped"
    exit "$RC"
fi
sizes="$(docker compose exec -T fqd sh -c 'cd /var/lib/factor-q && du -sm build workspace cache state agents 2>/dev/null' || true)"
say "volume: $(printf '%s' "$sizes" | awk '{printf "%s=%dM ", $2, $1}')"
build_mb="$(printf '%s\n' "$sizes" | awk '$2=="build"{print $1}')"; build_mb="${build_mb:-0}"
ws_count="$(docker compose exec -T fqd sh -c 'ls -1 /var/lib/factor-q/workspace 2>/dev/null | wc -l' | tr -dc '0-9')"
ws_old="$(docker compose exec -T fqd sh -c 'find /var/lib/factor-q/workspace -mindepth 1 -maxdepth 1 -mtime +7 2>/dev/null | wc -l' | tr -dc '0-9')"
say "workspaces: ${ws_count:-0} (${ws_old:-0} untouched for over 7 days — the daemon reclaims terminal ones; #367)"

# --- 3. the build cache, bounded ------------------------------------------------------
max_mb=$(( CACHE_MAX_GB * 1024 ))
if [ "$build_mb" -gt "$max_mb" ]; then
    if [ "$REPORT_ONLY" = 1 ]; then
        warn "build cache is ${build_mb}M, over ${CACHE_MAX_GB}G (report only — not pruned)"
    else
        inflight="$(if out="$(docker compose exec -T fqd fq invocation list --status in_flight --json 2>/dev/null)"; then printf '%s' "$out" | { grep -o '"invocation_id"' || true; } | wc -l | tr -dc '0-9'; else echo unknown; fi)"
        if [ "$inflight" = "0" ]; then
            say "build cache is ${build_mb}M, over ${CACHE_MAX_GB}G and the daemon is idle — emptying build/ (the next build is cold)"
            docker compose exec -T fqd sh -c 'cd /var/lib/factor-q/build && rm -rf target sccache cargo/registry go-cache go-mod' \
                && say "build/ emptied" || warn "could not empty build/"
        else
            warn "build cache is ${build_mb}M, over ${CACHE_MAX_GB}G, but ${inflight} invocation(s) in flight (or the daemon cannot be asked) — not pruning now"
        fi
    fi
fi

# --- 4. dangling images ------------------------------------------------------------------
if [ "$REPORT_ONLY" != 1 ]; then
    pruned="$(docker image prune -f 2>/dev/null | tail -1 || true)"
    [ -n "$pruned" ] && say "images: $pruned"
fi

exit "$RC"
