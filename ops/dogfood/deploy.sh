#!/usr/bin/env bash
# ops/dogfood/deploy.sh — fetch-verify-swap deploy for the dogfood stack (#102).
#
#   deploy.sh                 deploy the newest main-latest channel build
#   deploy.sh <sha>           deploy an already-fetched build from
#                             releases/<sha>/ — i.e. rollback (prefix ok)
#   deploy.sh --force [...]   proceed even if already running the target
#
# The host never compiles. Artifacts come from the rolling `main-latest`
# pre-release (.github/workflows/main-artifacts.yml): one checksummed
# tarball holding fq, fq-cas, fq-dashboard, github-watcher and their
# launchers, all stamped with the commit SHA they were built from. Every
# deployed build is kept under releases/<sha>/ and `current` symlinks
# the active one, so rollback is `deploy.sh <previous-sha>` — local, no
# network.
#
# Contract: exits 0 ONLY when all three processes (daemon, watcher,
# dashboard) are confirmed running from releases/<sha>/ (checked via
# /proc/<pid>/exe, not just log lines). The dashboard must move in
# lockstep with the daemon: the read-service RPC uses a length-framed
# binary codec, so a dashboard from another build fails to decode the
# daemon's responses and renders "runtime unreachable" (the #154-skew
# incident, 2026-07-14).
#
# Bring-down is graceful (ADR-0027): `fq drain` suspends in-flight
# invocations at a step boundary (state on the WAL) and the process exits
# on its own; recovery resumes them under the new binary. Past the
# bounded wait the fallback escalates: a *confirmed* stop via
# `fq down --now` (#63 — clean teardown, worker deregistered, exit
# observed), then SIGINT as the true last resort for a daemon that
# predates `fq down` or is too wedged to service control messages.
# The watcher and dashboard are stateless: SIGTERM.
#
# No health-gate / auto-rollback yet — that is the next slice of #102.
set -euo pipefail

DOGFOOD="${FQ_DOGFOOD:-$HOME/fq-dogfood}"
REPO_SLUG="${FQ_REPO_SLUG:-bricef/factor-q}"
TARGET="${FQ_TARGET:-x86_64-unknown-linux-musl}"
CHANNEL="${FQ_CHANNEL:-main-latest}"
DRAIN_WAIT="${DRAIN_WAIT:-180}"   # seconds to wait for a graceful drain
READY_WAIT="${READY_WAIT:-90}"    # seconds to wait for daemon "Runtime ready"
KEEP_RELEASES="${KEEP_RELEASES:-5}"

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
ok()  { printf '\033[1;32m    ✓ %s\033[0m\n' "$*"; }
die() { printf '\n\033[1;31m✗ ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

FORCE=0
WANT="latest"
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -*) die "unknown flag: $arg" ;;
        *) WANT="$arg" ;;
    esac
done

cd "$DOGFOOD" 2>/dev/null || die "dogfood dir not found: $DOGFOOD (set FQ_DOGFOOD)"
mkdir -p releases logs
[ -f fq.toml ] || die "no fq.toml in $DOGFOOD — the instance config stays host-side"
[ -f .secrets/env ] || die "no .secrets/env in $DOGFOOD — start from ops/dogfood/env.example"

# The embedded-SHA readers. fq prints "fq <semver> (<sha> <target>)";
# github-watcher prints "github-watcher <sha>". Both stamp 12 hex chars
# with a "-dirty" suffix on an unclean build tree.
fq_sha()      { "$1" --version 2>/dev/null | sed -nE 's/.*\(([0-9a-f]+(-dirty)?) .*/\1/p'; }
watcher_sha() { "$1" --version 2>&1 | awk '{print $2}'; }

# --- 1. resolve the build to deploy → $REL (releases/<sha>), $SHA -------
if [ "$WANT" = "latest" ]; then
    command -v gh >/dev/null || die "gh CLI required to fetch the $CHANNEL channel"
    log "Fetching $CHANNEL ($TARGET) from $REPO_SLUG"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    gh release download "$CHANNEL" -R "$REPO_SLUG" --pattern "*${TARGET}*" -D "$tmp" \
        || die "download failed — has .github/workflows/main-artifacts.yml published a build yet?"
    (cd "$tmp" && sha256sum --check --quiet ./*.sha256) || die "sha256 verification failed"
    ok "checksum verified"

    mkdir "$tmp/x"
    tar -xzf "$tmp"/*.tar.gz -C "$tmp/x"
    chmod +x "$tmp/x/fq" "$tmp/x/fq-cas" "$tmp/x/fq-dashboard" "$tmp/x/github-watcher" "$tmp/x"/*.sh

    SHA="$(fq_sha "$tmp/x/fq")"
    [ -n "$SHA" ] || die "could not read the embedded SHA from the downloaded fq"
    case "$SHA" in *-dirty*) die "channel artifact is dirty-stamped ($SHA) — refusing" ;; esac
    WSHA="$(watcher_sha "$tmp/x/github-watcher")"
    [ "$WSHA" = "$SHA" ] || die "bundle mismatch: fq is $SHA but github-watcher is $WSHA"
    ok "bundle is main @ $SHA (fq and watcher agree)"

    REL="releases/$SHA"
    if [ -d "$REL" ]; then
        ok "$REL already installed — reusing it"
    else
        mkdir "$REL"
        cp "$tmp/x/"* "$REL/"
        ok "installed $REL"
    fi
else
    # Rollback / explicit-SHA mode: deploy from the local history only.
    matches=()
    for d in "releases/$WANT"*; do [ -d "$d" ] && matches+=("$d"); done
    [ "${#matches[@]}" -ge 1 ] || die "no releases/$WANT* on this host — only 'latest' can fetch"
    [ "${#matches[@]}" -eq 1 ] || die "ambiguous sha prefix '$WANT': ${matches[*]}"
    REL="${matches[0]}"
    SHA="$(basename "$REL")"
    BSHA="$(fq_sha "$REL/fq")"
    [ "$BSHA" = "$SHA" ] || die "$REL/fq reports $BSHA, not $SHA — corrupted release dir"
    ok "deploying local $REL"
fi

# --- 2. early exit when the target is already live -----------------------
ACTIVE="$(readlink current 2>/dev/null || true)"
DAEMON_PID="$(pgrep -x fq | head -1 || true)"
if [ "$FORCE" != 1 ] && [ "$ACTIVE" = "$REL" ] && [ -n "$DAEMON_PID" ]; then
    exe="$(readlink "/proc/$DAEMON_PID/exe" 2>/dev/null || true)"
    if [ "$exe" = "$DOGFOOD/$REL/fq" ] && pgrep -x github-watcher >/dev/null \
        && pgrep -x fq-dashboard >/dev/null; then
        ok "already running $SHA — nothing to do (--force to restart anyway)"
        exit 0
    fi
fi

# --- 3. graceful bring-down ----------------------------------------------
if [ -n "$DAEMON_PID" ]; then
    log "Draining daemon (PID $DAEMON_PID) via fq drain — up to ${DRAIN_WAIT}s"
    "$REL/fq" --config "$DOGFOOD/fq.toml" drain \
        || printf '    (drain publish returned non-zero; waiting for exit anyway)\n'
    for _ in $(seq 1 "$DRAIN_WAIT"); do
        kill -0 "$DAEMON_PID" 2>/dev/null || break
        sleep 1
    done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        # Escalation 1 — a *confirmed* stop (#63): `--now` skips the
        # already-attempted drain; the daemon tears down cleanly,
        # DEREGISTERS its worker (no stale-worker cruft, #64/#65), and
        # `fq down` exits zero only after observing the daemon's own
        # system.shutdown event. Against a daemon that predates
        # `fq down`, or one too wedged to service control messages, it
        # times out non-zero and we fall through to the signal.
        printf '    drain deadline exceeded — requesting confirmed stop (fq down --now)\n'
        if "$REL/fq" --config "$DOGFOOD/fq.toml" down --now; then
            printf '    confirmed stop\n'
        else
            # Escalation 2, last resort: SIGINT is crash-equivalent —
            # the worker registration goes stale and the next start's
            # recovery resumes whatever was in flight.
            printf '    no confirmation from fq down — hard-stopping (SIGINT)\n'
            kill -INT "$DAEMON_PID" 2>/dev/null || true
        fi
        for _ in $(seq 1 20); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 1; done
    fi
    kill -0 "$DAEMON_PID" 2>/dev/null && die "daemon PID $DAEMON_PID would not stop"
    ok "daemon stopped"
else
    ok "no daemon running"
fi

for wpid in $(pgrep -x github-watcher || true); do
    log "Stopping watcher (PID $wpid)"
    kill -TERM "$wpid" 2>/dev/null || true
    for _ in $(seq 1 15); do kill -0 "$wpid" 2>/dev/null || break; sleep 1; done
    kill -0 "$wpid" 2>/dev/null && kill -KILL "$wpid" 2>/dev/null || true
    ok "watcher $wpid stopped"
done

# The dashboard must not outlive the flip: a stale binary cannot decode
# the new daemon's read-service responses (see the header contract).
for dpid in $(pgrep -x fq-dashboard || true); do
    log "Stopping dashboard (PID $dpid)"
    kill -TERM "$dpid" 2>/dev/null || true
    for _ in $(seq 1 15); do kill -0 "$dpid" 2>/dev/null || break; sleep 1; done
    kill -0 "$dpid" 2>/dev/null && kill -KILL "$dpid" 2>/dev/null || true
    ok "dashboard $dpid stopped"
done

# --- 4. flip the symlink atomically ---------------------------------------
rm -f current.new
ln -s "$REL" current.new
mv -Tf current.new current
ok "current -> $REL"

# --- 5. relaunch all three (detached), verifying against fresh log lines --
daemon_log_lines="$(wc -l < logs/fq-run.log 2>/dev/null || echo 0)"
watcher_log_lines="$(wc -l < logs/watcher.log 2>/dev/null || echo 0)"

log "Relaunching daemon (current/run.sh)"
setsid ./current/run.sh >> logs/fq-run.log 2>&1 </dev/null &
log "Relaunching watcher (current/watcher.sh)"
setsid ./current/watcher.sh >> logs/watcher.log 2>&1 </dev/null &
log "Relaunching dashboard (current/dashboard.sh)"
setsid ./current/dashboard.sh >> logs/dashboard.log 2>&1 </dev/null &

log "Verifying daemon startup (up to ${READY_WAIT}s)"
ready=0
for _ in $(seq 1 "$READY_WAIT"); do
    fresh="$(tail -n +"$((daemon_log_lines + 1))" logs/fq-run.log 2>/dev/null || true)"
    if printf '%s' "$fresh" | grep -qiE 'registry validation failed|refus(e|ing)|panicked'; then
        die "daemon failed to start (see logs/fq-run.log)"
    fi
    if printf '%s' "$fresh" | grep -q "Runtime ready"; then ready=1; break; fi
    sleep 1
done
[ "$ready" = 1 ] || die "daemon did not reach 'Runtime ready' within ${READY_WAIT}s (see logs/fq-run.log)"

NEW_DAEMON="$(pgrep -x fq | head -1 || true)"
[ -n "$NEW_DAEMON" ] || die "no fq process after relaunch"
exe="$(readlink "/proc/$NEW_DAEMON/exe" 2>/dev/null || true)"
[ "$exe" = "$DOGFOOD/$REL/fq" ] \
    || die "daemon PID $NEW_DAEMON runs $exe, not $DOGFOOD/$REL/fq"
ok "daemon up (PID $NEW_DAEMON) from $REL, Runtime ready"

log "Verifying watcher startup"
sleep 4
NEW_WATCHER="$(pgrep -x github-watcher | head -1 || true)"
[ -n "$NEW_WATCHER" ] || die "watcher not running after relaunch (see logs/watcher.log)"
wexe="$(readlink "/proc/$NEW_WATCHER/exe" 2>/dev/null || true)"
[ "$wexe" = "$DOGFOOD/$REL/github-watcher" ] \
    || die "watcher PID $NEW_WATCHER runs $wexe, not $DOGFOOD/$REL/github-watcher"
watcher_fresh="$(tail -n +"$((watcher_log_lines + 1))" logs/watcher.log 2>/dev/null || true)"
if printf '%s' "$watcher_fresh" | grep -qi 'gh auth login\|poll cycle failed'; then
    printf '\033[1;33m    ⚠ watcher is up but its GitHub auth is failing — check GH_TOKEN in .secrets/env (see logs/watcher.log)\033[0m\n'
else
    ok "watcher up (PID $NEW_WATCHER) from $REL"
fi

log "Verifying dashboard startup"
NEW_DASHBOARD="$(pgrep -x fq-dashboard | head -1 || true)"
[ -n "$NEW_DASHBOARD" ] || die "dashboard not running after relaunch (see logs/dashboard.log)"
dexe="$(readlink "/proc/$NEW_DASHBOARD/exe" 2>/dev/null || true)"
[ "$dexe" = "$DOGFOOD/$REL/fq-dashboard" ] \
    || die "dashboard PID $NEW_DASHBOARD runs $dexe, not $DOGFOOD/$REL/fq-dashboard"
ok "dashboard up (PID $NEW_DASHBOARD) from $REL"

# --- 6. prune old releases (keep the newest KEEP_RELEASES, never $REL) ---
i=0
for d in $(ls -1t releases); do
    [ -d "releases/$d" ] || continue
    i=$((i + 1))
    [ "$i" -le "$KEEP_RELEASES" ] && continue
    [ "releases/$d" = "$REL" ] && continue
    rm -rf "releases/${d:?}"
    printf '    pruned releases/%s\n' "$d"
done

# --- done ------------------------------------------------------------------
printf '\n\033[1;32m════════════════════════════════════════════════════\n'
printf '  DEPLOYED — factor-q dogfood stack @ %s\n' "$SHA"
printf '    daemon    PID %-8s %s\n' "$NEW_DAEMON" "$("$REL/fq" --version)"
printf '    watcher   PID %-8s %s\n' "$NEW_WATCHER" "$("$REL/github-watcher" --version 2>&1)"
printf '    dashboard PID %-8s %s/fq-dashboard (no --version; verified via /proc)\n' "$NEW_DASHBOARD" "$REL"
printf '    rollback: ops/dogfood/deploy.sh <sha>   history: ls %s/releases\n' "$DOGFOOD"
printf '════════════════════════════════════════════════════\033[0m\n'
