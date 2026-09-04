#!/usr/bin/env bash
# ops/dogfood/deploy.sh — tag-bump deploy for the compose stack (ADR-0035,
# #587 slice 4).
#
#   deploy.sh                 deploy the newest main build (what the images'
#                             `main-latest` tag names right now)
#   deploy.sh <sha>           deploy that build — a rollback is this command
#                             with an older sha (a unique prefix is fine when
#                             the images are already on this host)
#   deploy.sh --force [...]   proceed even if already running the target
#                             (a .env, fqd.toml or secrets change)
#
# The host never compiles and never fetches a tarball. Every merge to main
# publishes one image per binary to ghcr.io/bricef (`FQ_IMAGE_REPO` in .env),
# tagged with the twelve-hex commit the binary inside reports; `main-latest`
# is a moving alias for the newest. A deploy is: pull the target tag, prove
# every image's binary reports it, write FQ_TAG in .env, `docker compose up
# -d`. The registry keeps every tag, so the deploy history is the registry
# and the rollback mechanism is the same command with an older sha.
#
# Contract: exits 0 ONLY when the daemon, watcher, dashboard and scheduler
# containers run the target tag — checked on the running containers'
# images, not on what .env says — and the daemon has logged "Runtime ready"
# since it was started.
#
# Bring-down is graceful (ADR-0027) and needs no ladder: `docker compose
# stop` sends SIGTERM, which the daemon treats as a drain — in-flight
# invocations suspend at their next step boundary, bounded by the drain
# deadline — and compose's stop_grace_period (FQ_STOP_GRACE, longer than
# that deadline) is the fallback kill. The scheduler stops first so no
# fire lands mid-drain; anything already published rides out the restart
# broker-side. The broker and the proxy are not touched by a deploy.
set -euo pipefail

DOGFOOD="${FQ_DOGFOOD:-$HOME/fq-dogfood}"
READY_WAIT="${READY_WAIT:-180}"      # seconds to wait for the daemon's "Runtime ready"
KEEP_IMAGES="${KEEP_IMAGES:-5}"      # local image tags kept per name, newest first

# The services a deploy restarts, with the image each runs, in the order
# they are brought down (the reverse is compose's business on the way up).
STACK_SERVICES=(fq-cron fqd github-watcher fq-dashboard)
declare -A IMAGE_OF=([fqd]=fq-dogfood [github-watcher]=github-watcher [fq-cron]=fq-cron [fq-dashboard]=fq-dashboard)

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
ok()  { printf '\033[1;32m    ✓ %s\033[0m\n' "$*"; }
die() { printf '\n\033[1;31m✗ ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

FORCE=0
WANT="latest"
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help) sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) die "unknown flag: $arg" ;;
        *) WANT="$arg" ;;
    esac
done

cd "$DOGFOOD" 2>/dev/null || die "dogfood dir not found: $DOGFOOD (set FQ_DOGFOOD)"
[ -f compose.yml ] || die "no compose.yml in $DOGFOOD — copy ops/dogfood/compose.yml here"
[ -f .env ] || die "no .env in $DOGFOOD — start from ops/dogfood/.env.example"
for f in env dashboard.env nats-auth.conf caddy.env; do
    [ -f ".secrets/$f" ] || die "no .secrets/$f in $DOGFOOD — see ops/dogfood/README.md, Bootstrap"
done
command -v docker >/dev/null || die "docker is required"
docker compose version >/dev/null 2>&1 || die "docker compose (v2) is required"

# One deploy at a time. Two concurrent runs would each stop and start the
# stack; the lock is on the instance directory, so it also covers a deploy
# started from another checkout.
exec 9>"$DOGFOOD/.deploy.lock"
flock -n 9 || die "another deploy holds $DOGFOOD/.deploy.lock"

REPO="$(sed -n 's/^FQ_IMAGE_REPO=\(.*\)$/\1/p' .env | tail -1)"
REPO="${REPO:-ghcr.io/bricef}"
CURRENT="$(sed -n 's/^FQ_TAG=\(.*\)$/\1/p' .env | tail -1)"
# What a failed deploy rolls back to: the tag that was live before, if it
# differs from the target (a forced redeploy of the live tag has none).
rollback_hint() { if [ -n "$CURRENT" ] && [ "$CURRENT" != "$SHA" ]; then echo "Roll back: $0 $CURRENT"; else echo "Roll back: $0 <an earlier sha — docker images $REPO/fq-dogfood>"; fi; }

# The embedded-SHA readers, on an image rather than a file: the binary is
# run through its entrypoint with --version. fqd prints
# "fqd <semver> (<sha> <target>)"; the Go adapters and the dashboard print
# "<name> <sha>". A "-dirty" suffix names an unclean build tree.
version_sha() {  # $1 = image ref
    local out
    out="$(docker run --rm "$1" --version 2>/dev/null)" || return 1
    case "$out" in
        *"("*) printf '%s' "$out" | sed -nE 's/.*\(([0-9a-f]+(-dirty)?) .*/\1/p' ;;
        *)     printf '%s' "$out" | awk '{print $2}' ;;
    esac
}

# --- 1. resolve the build to deploy → $SHA -------------------------------
if [ "$WANT" = "latest" ]; then
    log "Resolving $REPO main-latest"
    docker pull -q "$REPO/fq-dogfood:main-latest" >/dev/null \
        || die "cannot pull $REPO/fq-dogfood:main-latest — logged in to the registry? (docker login ghcr.io)"
    SHA="$(version_sha "$REPO/fq-dogfood:main-latest")"
    [[ "$SHA" =~ ^[0-9a-f]{12}$ ]] || die "main-latest's fqd reports '$SHA', not a twelve-hex commit — refusing"
    ok "main-latest is $SHA"
else
    if [[ "$WANT" =~ ^[0-9a-f]{12}$ ]]; then
        SHA="$WANT"
    else
        # A prefix resolves against the images already on this host —
        # the local deploy history — never against the registry.
        [[ "$WANT" =~ ^[0-9a-f]{4,11}$ ]] || die "'$WANT' is neither a twelve-hex commit nor a hex prefix of one"
        mapfile -t matches < <(docker images --format '{{.Tag}}' "$REPO/fq-dogfood" | grep -E "^${WANT}[0-9a-f]*$" | sort -u)
        [ "${#matches[@]}" -ge 1 ] || die "no local $REPO/fq-dogfood tag starts with '$WANT' — give the full twelve-hex commit to pull it"
        [ "${#matches[@]}" -eq 1 ] || die "ambiguous prefix '$WANT': ${matches[*]}"
        SHA="${matches[0]}"
    fi
    ok "target is $SHA"
fi

# --- 2. pull the target and prove it is what it says -----------------------
# Every image, at the commit tag. A pull that fails is fine when the tag is
# already here (a rollback while the registry is unreachable); a tag that is
# neither pullable nor local is not.
log "Pulling $REPO/*:$SHA"
for svc in "${STACK_SERVICES[@]}"; do
    ref="$REPO/${IMAGE_OF[$svc]}:$SHA"
    if ! docker pull -q "$ref" >/dev/null 2>&1; then
        docker image inspect "$ref" >/dev/null 2>&1 || die "cannot pull $ref and it is not on this host"
        printf '    (using local %s — pull failed)\n' "$ref"
    fi
done
ok "all four images present"

# The coherence check deploy.sh always made on a bundle, on the images: the
# binary inside each must report the commit its tag claims, with no -dirty.
log "Verifying every image's binary reports $SHA"
for svc in "${STACK_SERVICES[@]}"; do
    ref="$REPO/${IMAGE_OF[$svc]}:$SHA"
    got="$(version_sha "$ref")" || die "$ref: --version failed"
    [ "$got" = "$SHA" ] || die "$ref reports '$got', not $SHA — tag and content disagree; refusing"
done
ok "fqd, github-watcher, fq-cron and fq-dashboard all report $SHA"

# --- 3. early exit when the target is already live -------------------------
running_image() {  # $1 = service → the image its running container was created from, or ""
    local cid
    cid="$(docker compose ps -q --status running "$1" 2>/dev/null | head -1)"
    [ -n "$cid" ] || return 0
    docker inspect --format '{{.Config.Image}}' "$cid" 2>/dev/null || true
}
if [ "$FORCE" != 1 ] && [ "$CURRENT" = "$SHA" ]; then
    live=1
    for svc in "${STACK_SERVICES[@]}"; do
        [ "$(running_image "$svc")" = "$REPO/${IMAGE_OF[$svc]}:$SHA" ] || { live=0; break; }
    done
    if [ "$live" = 1 ]; then
        ok "already running $SHA — nothing to do (--force to restart anyway)"
        exit 0
    fi
fi

# --- 4. graceful bring-down --------------------------------------------------
# Scheduler first (no new fires mid-drain), then the daemon — SIGTERM is the
# drain, compose's grace period the deadline — then the stateless two.
for svc in "${STACK_SERVICES[@]}"; do
    if [ -n "$(docker compose ps -q --status running "$svc" 2>/dev/null)" ]; then
        log "Stopping $svc"
        t0=$(date +%s)
        docker compose stop "$svc" >/dev/null 2>&1 || die "docker compose stop $svc failed"
        ok "$svc stopped in $(( $(date +%s) - t0 ))s"
    fi
done

# --- 5. point the stack at the target and bring it up -------------------------
if grep -q '^FQ_TAG=' .env; then
    sed -i "s/^FQ_TAG=.*/FQ_TAG=$SHA/" .env
else
    printf 'FQ_TAG=%s\n' "$SHA" >> .env
fi
ok ".env: FQ_TAG=$SHA${CURRENT:+ (was ${CURRENT})}"

STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
log "Bringing the stack up at $SHA"
docker compose up -d --remove-orphans >/dev/null 2>&1 || die "docker compose up failed (docker compose logs fqd)"

# --- 6. verify: the daemon says ready, and every container runs the target --
log "Waiting for the daemon's 'Runtime ready' (up to ${READY_WAIT}s)"
FQD_CID="$(docker compose ps -q fqd | head -1)"
[ -n "$FQD_CID" ] || die "no fqd container after up (docker compose ps)"
ready=0
for _ in $(seq 1 "$READY_WAIT"); do
    fresh="$(docker logs --since "$STARTED_AT" "$FQD_CID" 2>&1 || true)"
    if printf '%s' "$fresh" | grep -qiE 'registry validation failed|refus(e|ing)|panicked'; then
        die "the daemon failed to start on $SHA (docker compose logs fqd). $(rollback_hint)"
    fi
    if printf '%s' "$fresh" | grep -q "Runtime ready"; then ready=1; break; fi
    sleep 1
done
[ "$ready" = 1 ] || die "the daemon did not log 'Runtime ready' within ${READY_WAIT}s (docker compose logs fqd). $(rollback_hint)"
ok "daemon ready"

for svc in "${STACK_SERVICES[@]}"; do
    want="$REPO/${IMAGE_OF[$svc]}:$SHA"
    got="$(running_image "$svc")"
    [ "$got" = "$want" ] || die "$svc runs '${got:-nothing}', not $want (docker compose ps)"
done
ok "fqd, github-watcher, fq-cron and fq-dashboard run $SHA"

# --- 7. prune local image history (the registry keeps every tag) ----------
for name in fq-dogfood github-watcher fq-cron fq-dashboard; do
    i=0
    while read -r tag; do
        [ -n "$tag" ] || continue
        case "$tag" in main-latest|"$SHA"|"<none>") continue ;; esac
        i=$((i + 1))
        [ "$i" -le "$KEEP_IMAGES" ] && continue
        docker rmi "$REPO/$name:$tag" >/dev/null 2>&1 && printf '    pruned %s:%s\n' "$name" "$tag"
    done < <(docker images --format '{{.CreatedAt}}\t{{.Tag}}' "$REPO/$name" | sort -r | cut -f2)
done

# --- done ------------------------------------------------------------------------
printf '\n\033[1;32m════════════════════════════════════════════════════\n'
printf '  DEPLOYED — factor-q dogfood stack @ %s\n' "$SHA"
docker compose ps --format '    {{.Service}}\t{{.State}}\t{{.Health}}\t{{.Image}}' 2>/dev/null || true
printf '    rollback: %s %s   history: docker images %s/fq-dogfood\n' "$0" "${CURRENT:-<sha>}" "$REPO"
printf '════════════════════════════════════════════════════\033[0m\n'
