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
#   deploy.sh --auto          unattended (cron): defer while an invocation is
#                             in flight, and roll back by itself when the new
#                             build does not come up — continuous delivery
#                             with the same drain, checks and rollback as a
#                             deploy by hand (ops/dogfood/crontab). A deploy,
#                             a rollback, a failure and a deferral that has
#                             lasted FQ_DEFER_WARN_HOURS go through notify.sh;
#                             the deploy message lists the commits that landed
#   deploy.sh --render-changes <from> <to> <owner/repo>
#                             print that list for the commit subjects on stdin
#                             and exit — the seam ops/dogfood/tests drives
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
# images, not on what .env says — the daemon has logged "Runtime ready"
# since it was started, and the watcher, scheduler and dashboard report
# healthy on their own probes.
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
HEALTH_WAIT="${HEALTH_WAIT:-90}"     # seconds to wait for the adapters' and dashboard's probes
KEEP_IMAGES="${KEEP_IMAGES:-5}"      # local image tags kept per name, newest first

# The services a deploy restarts, with the image each runs, in the order
# they are brought down (the reverse is compose's business on the way up).
STACK_SERVICES=(fq-cron fqd github-watcher fq-dashboard)
declare -A IMAGE_OF=([fqd]=fq-dogfood [github-watcher]=github-watcher [fq-cron]=fq-cron [fq-dashboard]=fq-dashboard)

AUTO=0
# --auto stays silent through the resolve/pull/verify preamble — an hourly
# cron run that changes nothing should leave one line in the log, not
# eight — and starts narrating once a deploy is actually going to happen.
QUIET=0
stamp() { [ "$AUTO" = 1 ] && date -u '+%Y-%m-%dT%H:%M:%SZ ' || true; }
log() { [ "$QUIET" = 1 ] && return 0; printf '\n\033[1;36m%s==> %s\033[0m\n' "$(stamp)" "$*"; }
ok()  { [ "$QUIET" = 1 ] && return 0; printf '\033[1;32m%s    ✓ %s\033[0m\n' "$(stamp)" "$*"; }
# Unattended, anything that needs a human goes through notify.sh
# (FQ_NOTIFY_HOOK); by hand, the operator is looking at the terminal.
notify() {  # $1 = subject, $2 = body
    [ "$AUTO" = 1 ] || return 0
    [ -x "$DOGFOOD/notify.sh" ] || return 0
    printf '%s\n' "$2" | "$DOGFOOD/notify.sh" "$1" || true
}
die() { printf '\n\033[1;31m%s✗ ERROR: %s\033[0m\n' "$(stamp)" "$*" >&2; notify "deploy FAILED" "$*"; exit 1; }
# A quiet exit for --auto: nothing to do, or not now. One line, exit 0, so
# a cron run that changed nothing leaves one line in the log and no mail.
defer() { printf '%s· %s\n' "$(stamp)" "$*"; exit 0; }
# A deferral is fine once and worrying after a day: an invocation stuck
# in flight, or a container never paired, keeps every merge off the host
# with nothing but a quiet line an hour in the log. .deploy.deferred
# remembers since when the same target has been waiting; past
# FQ_DEFER_WARN_HOURS (6) it is reported once, and the file goes when the
# deploy happens or the target changes.
deferring() {  # $1 = target sha, $2 = why
    local since="" notified="" now age file="$DOGFOOD/.deploy.deferred" hours
    hours="$(sed -n 's/^FQ_DEFER_WARN_HOURS=\(.*\)$/\1/p' .env | tail -1)"; hours="${hours:-6}"
    now="$(date +%s)"
    if [ -f "$file" ]; then IFS=$'\t' read -r sha since notified < "$file" || true; [ "${sha:-}" = "$1" ] || since=""; fi
    [ -n "$since" ] || { since="$now"; notified=""; }
    age=$(( (now - since) / 3600 ))
    if [ "$age" -ge "$hours" ] && [ -z "$notified" ]; then
        notify "deploy of $1 deferred for ${age}h" "$2"$'\n'"Every hourly run since $(date -u -d "@$since" '+%Y-%m-%dT%H:%MZ') has found the same. See ops/dogfood/README.md, \"Continuous delivery\"."
        notified=notified
    fi
    printf '%s\t%s\t%s\n' "$1" "$since" "$notified" > "$file"
    defer "$2"
}

# --- what a deploy carries ---------------------------------------------------
# The commits between the build that was live and the one just deployed,
# so the deploy message says what landed rather than only that something
# did. Asked of GitHub through the daemon container's own gh — it holds
# GH_TOKEN and the network the agents use, and the host keeps no current
# checkout (bootstrap.sh copies the scripts out; the clone stays where it
# was) — via the compare endpoint, which needs no fetch and reads a
# twelve-hex prefix. Best effort: the deploy is done by the time this is
# asked, so a failure is one line in the message, never an error.
changes_since() {  # $1 = from sha, $2 = to sha, $3 = owner/repo → subjects, newest first
    local out
    if ! out="$(docker compose exec -T fqd gh api "repos/$3/compare/$1...$2" \
            --jq '.commits | reverse | .[].commit.message | split("\n")[0]' 2>&1)"; then
        printf 'commit list unavailable: %s\n' "$(printf '%s\n' "$out" | grep -v '^$' | tail -1)"
        return 1
    fi
    printf '%s\n' "$out"
}
# The message body, from the subjects on stdin. User-facing commits —
# feat, fix, perf: the conventional types a reader of the dashboard would
# notice — are listed, up to six of a hundred characters each; the rest
# are counted by type, so a deploy of twelve docs commits is one line,
# not twelve. The compare link is where the whole list lives. Pushover
# caps a message at 1024 characters and notify.sh signs it, so the body
# stops at 900.
render_changes() {  # $1 = from sha, $2 = to sha, $3 = owner/repo; subjects on stdin
    local from="$1" to="$2" repo="$3" line kind total=0 facing=0 listed="" others="" body
    declare -A count=()
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        total=$((total + 1))
        case "$line" in
            feat|feat[\(:!]*|fix|fix[\(:!]*|perf|perf[\(:!]*)
                facing=$((facing + 1))
                [ "$facing" -le 6 ] && listed="$listed"$'\n'"• ${line:0:100}" ;;
            *)
                kind="${line%%:*}"; kind="${kind%%(*}"; kind="${kind%%!}"
                case "$kind" in "$line"|*" "*) kind="other" ;; esac   # no "type:" prefix
                count[$kind]=$(( ${count[$kind]:-0} + 1 )) ;;
        esac
    done
    if [ "${#count[@]}" -gt 0 ]; then
        others="$(for kind in "${!count[@]}"; do printf '%s\t%s\n' "${count[$kind]}" "$kind"; done \
            | sort -k1,1nr -k2,2 | awk -F'\t' '{printf "%s%s %s", (NR>1?", ":""), $2, $1}')"
    fi
    if [ "$total" = 0 ]; then
        body="no commits between $from and $to — the same build, or a step back"
    else
        body="from $from · $total commit$([ "$total" = 1 ] || echo s)$listed"
        [ "$facing" -gt 6 ] && body="$body"$'\n'"+$((facing - 6)) more user-facing"
        if [ "$facing" = 0 ]; then body="$body"$'\n'"no user-facing changes; $others"
        elif [ -n "$others" ]; then body="$body"$'\n'"+$((total - facing)) other: $others"; fi
    fi
    body="$body"$'\n'"https://github.com/$repo/compare/$from...$to"
    printf '%s\n' "${body:0:900}"
}
if [ "${1:-}" = "--render-changes" ]; then
    [ $# -eq 4 ] || { echo "usage: deploy.sh --render-changes <from> <to> <owner/repo>  (subjects on stdin)" >&2; exit 2; }
    render_changes "$2" "$3" "$4"
    exit 0
fi

FORCE=0
WANT="latest"
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --auto) AUTO=1; QUIET=1 ;;
        -h|--help) awk 'NR>1 && !/^#/ {exit} NR>1 {sub(/^# ?/, ""); print}' "$0"; exit 0 ;;
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
if ! flock -n 9; then
    [ "$AUTO" = 1 ] && defer "another deploy holds $DOGFOOD/.deploy.lock — skipping this run"
    die "another deploy holds $DOGFOOD/.deploy.lock"
fi

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
# How many invocations the running daemon has in flight, or "unknown"
# when it cannot be asked (no container, or its fq is not paired yet —
# the README's one-time step). Asked through the container's own client
# over the edge, like every other question to the daemon.
in_flight() {
    local cid out
    cid="$(docker compose ps -q --status running fqd 2>/dev/null | head -1)"
    [ -n "$cid" ] || { echo 0; return; }
    out="$(docker compose exec -T fqd fq invocation list --status in_flight --json 2>/dev/null)" || { echo unknown; return; }
    printf '%s' "$out" | { grep -o '"invocation_id"' || true; } | wc -l | tr -dc '0-9'
}

if [ "$FORCE" != 1 ] && [ "$CURRENT" = "$SHA" ]; then
    live=1
    for svc in "${STACK_SERVICES[@]}"; do
        [ "$(running_image "$svc")" = "$REPO/${IMAGE_OF[$svc]}:$SHA" ] || { live=0; break; }
    done
    if [ "$live" = 1 ]; then
        rm -f "$DOGFOOD/.deploy.deferred"
        [ "$AUTO" = 1 ] && defer "already running $SHA"
        ok "already running $SHA — nothing to do (--force to restart anyway)"
        exit 0
    fi
fi

# Unattended, a deploy waits its turn: the drain suspends in-flight work
# at a step boundary and resumes it under the new build, but a run that is
# mid tool-call when the deadline expires becomes ambiguous and can never
# be resumed (the README's "before any restart"). A daemon that cannot be
# asked is not assumed idle. The next cron run tries again.
if [ "$AUTO" = 1 ]; then
    busy="$(in_flight)"
    case "$busy" in
        0) ;;
        unknown) deferring "$SHA" "cannot ask the daemon whether it is idle (not paired? see README) — deferring $SHA" ;;
        *) deferring "$SHA" "$busy invocation(s) in flight — deferring $SHA" ;;
    esac
    rm -f "$DOGFOOD/.deploy.deferred"
    QUIET=0
    log "AUTO DEPLOY ${CURRENT:-<none>} → $SHA (daemon idle; images verified)"
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

# --- 5. point the stack at a tag, bring it up, verify --------------------------
# One function for the deploy and for the rollback: write the tag, up, wait
# for the daemon's "Runtime ready" since the start, then check every
# container's image. Returns non-zero with the reason on stdout's last
# line rather than dying, so --auto can roll back.
bring_up() {  # $1 = sha
    local tag="$1" started cid fresh ready svc want got
    if grep -q '^FQ_TAG=' .env; then
        sed -i "s/^FQ_TAG=.*/FQ_TAG=$tag/" .env
    else
        printf 'FQ_TAG=%s\n' "$tag" >> .env
    fi
    ok ".env: FQ_TAG=$tag"
    started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    log "Bringing the stack up at $tag"
    docker compose up -d --remove-orphans >/dev/null 2>&1 || { echo "docker compose up failed (docker compose logs fqd)"; return 1; }

    log "Waiting for the daemon's 'Runtime ready' (up to ${READY_WAIT}s)"
    cid="$(docker compose ps -q fqd | head -1)"
    [ -n "$cid" ] || { echo "no fqd container after up (docker compose ps)"; return 1; }
    ready=0
    for _ in $(seq 1 "$READY_WAIT"); do
        fresh="$(docker logs --since "$started" "$cid" 2>&1 || true)"
        if printf '%s' "$fresh" | grep -qiE 'registry validation failed|refus(e|ing)|panicked'; then
            echo "the daemon failed to start on $tag (docker compose logs fqd)"; return 1
        fi
        if printf '%s' "$fresh" | grep -q "Runtime ready"; then ready=1; break; fi
        sleep 1
    done
    [ "$ready" = 1 ] || { echo "the daemon did not log 'Runtime ready' within ${READY_WAIT}s (docker compose logs fqd)"; return 1; }
    ok "daemon ready"

    for svc in "${STACK_SERVICES[@]}"; do
        want="$REPO/${IMAGE_OF[$svc]}:$tag"
        got="$(running_image "$svc")"
        [ "$got" = "$want" ] || { echo "$svc runs '${got:-nothing}', not $want (docker compose ps)"; return 1; }
    done
    ok "fqd, github-watcher, fq-cron and fq-dashboard run $tag"

    # The other three images carry their own probes (ADR-0035 clause 8):
    # attached to the broker, serving. Wait for each to report healthy; an
    # image from before the probes reports nothing and is not waited on.
    # The daemon's probe is not waited on here — it needs the pairing that
    # a fresh instance does not have yet; "Runtime ready" is its signal.
    log "Waiting for the adapters' and the dashboard's probes (up to ${HEALTH_WAIT}s)"
    local pending="" state
    for _ in $(seq 1 "$HEALTH_WAIT"); do
        pending=""
        for svc in github-watcher fq-cron fq-dashboard; do
            cid="$(docker compose ps -q "$svc" 2>/dev/null | head -1)"
            [ -n "$cid" ] || continue
            state="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$cid" 2>/dev/null || true)"
            case "$state" in ""|healthy) ;; *) pending="$pending $svc=$state" ;; esac
        done
        [ -z "$pending" ] && break
        sleep 1
    done
    [ -z "$pending" ] || { echo "not healthy after ${HEALTH_WAIT}s:$pending (docker inspect --format '{{json .State.Health}}' <container>)"; return 1; }
    ok "github-watcher, fq-cron and fq-dashboard probes healthy"
}

[ -n "$CURRENT" ] && [ "$CURRENT" != "$SHA" ] && ok "moving from $CURRENT"
if ! reason="$(bring_up "$SHA")"; then
    printf '%s\n' "$reason" | grep -v '^$' | grep -vE '^\S*(==>|✓)' || true
    reason="$(printf '%s\n' "$reason" | tail -1)"
    # Unattended: the previous build is known good — put it back and say so.
    # By hand: stop here and say what to run; the operator may want to look
    # at the failed container first.
    if [ "$AUTO" = 1 ] && [ -n "$CURRENT" ] && [ "$CURRENT" != "$SHA" ]; then
        log "ROLLING BACK to $CURRENT — $reason"
        if bring_up "$CURRENT" >/dev/null; then
            printf '\033[1;33m%s    ⟲ rolled back to %s after %s failed: %s\033[0m\n' "$(stamp)" "$CURRENT" "$SHA" "$reason" >&2
            notify "rolled back to $CURRENT — $SHA failed" "$reason"$'\n'"The instance is on $CURRENT, the build it was on before. The failed containers went with the rollback; to look at the failure, deploy by hand — deploy.sh $SHA stops on the failed container instead of rolling back. deploy.sh --auto will try $SHA again next hour unless main moves on."
            exit 1
        fi
        die "rollback to $CURRENT ALSO failed — the stack needs a human (docker compose ps; docker compose logs fqd)"
    fi
    die "$reason. $(rollback_hint)"
fi

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
# What landed, for the message and the terminal alike. GHW_REPO is the
# repository the watcher already follows — the one these builds come from.
GHW_REPO="$(sed -n 's/^GHW_REPO=\(.*\)$/\1/p' .env | tail -1)"
if [ -z "$CURRENT" ] || [ "$CURRENT" = "$SHA" ]; then
    changes="first deploy of $SHA on this host, or a redeploy — nothing to compare"
elif [ -z "$GHW_REPO" ]; then
    changes="commit list unavailable: GHW_REPO not set in .env"
elif subjects="$(changes_since "$CURRENT" "$SHA" "$GHW_REPO")"; then
    changes="$(printf '%s\n' "$subjects" | render_changes "$CURRENT" "$SHA" "$GHW_REPO")"
else
    changes="$subjects"$'\n'"https://github.com/$GHW_REPO/compare/$CURRENT...$SHA"
fi
printf '\n\033[1;32m════════════════════════════════════════════════════\n'
printf '  DEPLOYED — factor-q dogfood stack @ %s\n' "$SHA"
notify "deployed $SHA" "$changes"$'\n\n'"$(docker compose ps --format '{{.Service}} {{.State}} {{.Health}}' 2>/dev/null | tr '\n' ';' | sed 's/;$//;s/;/; /g')"
printf '%s\n' "$changes" | sed 's/^/    /'
docker compose ps --format '    {{.Service}}\t{{.State}}\t{{.Health}}\t{{.Image}}' 2>/dev/null || true
printf '    rollback: %s %s   history: docker images %s/fq-dogfood\n' "$0" "${CURRENT:-<sha>}" "$REPO"
printf '════════════════════════════════════════════════════\033[0m\n'
