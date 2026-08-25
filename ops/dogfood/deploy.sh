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
# tarball holding fq, fqd, fq-cas, fq-dashboard, github-watcher and their
# launchers, all stamped with the commit SHA they were built from. Every
# deployed build is kept under releases/<sha>/ and `current` symlinks
# the active one, so rollback is `deploy.sh <previous-sha>` — local, no
# network.
#
# Contract: exits 0 ONLY when the daemon, watcher and dashboard — plus
# fq-cron on hosts that have fq-cron.toml — are confirmed running from
# releases/<sha>/ (checked via /proc/<pid>/exe, not just log lines).
# Every process lookup here resolves /proc/<pid>/exe rather than trusting
# the process name, so a dev build left running from a worktree cannot
# shadow the real one — see pids_under() below.
# The dashboard must move in lockstep with the daemon. The reason is no
# longer the codec — it reads over the edge now, which is JSON in a
# stable envelope, so an added field no longer breaks an older reader
# the way the length-framed binary wire it replaced did (the #154-skew
# incident, 2026-07-14). It is that the two share the contract types
# they exchange (fq_runtime::surface, fq_runtime::views): a field
# REMOVED or renamed on one side is still a decode failure on the
# other. Lockstep is cheap and the failure is quiet, so it stays.
#
# Bring-down is graceful (ADR-0027): `fq down` suspends in-flight
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

# --- process selection: match /proc/<pid>/exe, never the name alone -------
# A dev build left running from .claude/worktrees/<x>/target/debug/fq is
# also called `fqd`, and `pgrep -x fqd | head -1` returns the LOWEST pid — so
# a stray that outlives a daemon restart silently shadows the real process.
# Seen 2026-07-27: 17 orphaned worktree daemons (oldest 11 days) made the
# post-launch check report "daemon PID N runs …/target/debug/fq (deleted)"
# and exit 1 on a deploy that had in fact relaunched correctly. The same
# lookup picks the bring-down target, so the failure mode is worse than a
# bad message: once the real daemon's pid sorts above a stray's, `fq down`
# and the SIGINT escalation aim at the stray while the real daemon keeps
# running — and the deploy aborts having already stopped cron and watcher.
#
# $1 = process name, $2 = required exe prefix (with trailing slash).
pids_under() {
    local name="$1" prefix="$2" p exe
    for p in $(pgrep -x "$name" 2>/dev/null || true); do
        exe="$(readlink "/proc/$p/exe" 2>/dev/null || true)"
        # A replaced-on-disk binary reads back as "<path> (deleted)".
        case "${exe% (deleted)}" in "$prefix"*) printf '%s\n' "$p" ;; esac
    done
}
# Bring-down: any release of this host's install — the running stack
# legitimately predates $REL (that is the point of a deploy).
pids_installed() { pids_under "$1" "$DOGFOOD/releases/"; }
# Verification: the exact release we just flipped `current` to.
pids_release()   { pids_under "$1" "$DOGFOOD/$REL/"; }

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
[ -f fqd.toml ] || die "no fqd.toml in $DOGFOOD — the instance config stays host-side\
   (the daemon's config was fq.toml before the fq/fqd split; rename it)"
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
    chmod +x "$tmp/x/fq" "$tmp/x/fq-cas" "$tmp/x/fq-dashboard" "$tmp/x/github-watcher" "$tmp/x/fq-cron" "$tmp/x"/*.sh

    SHA="$(fq_sha "$tmp/x/fq")"
    [ -n "$SHA" ] || die "could not read the embedded SHA from the downloaded fq"
    case "$SHA" in *-dirty*) die "channel artifact is dirty-stamped ($SHA) — refusing" ;; esac
    WSHA="$(watcher_sha "$tmp/x/github-watcher")"
    [ "$WSHA" = "$SHA" ] || die "bundle mismatch: fq is $SHA but github-watcher is $WSHA"
    # The dashboard gained --version with #168 (prints "fq-dashboard
    # <sha>"); include it in the coherence check. Tolerate a bundle
    # predating the flag (empty DSHA) rather than dying on it.
    DSHA="$("$tmp/x/fq-dashboard" --version 2>/dev/null | awk '{print $2}')"
    if [ -n "$DSHA" ]; then
        [ "$DSHA" = "$SHA" ] || die "bundle mismatch: fq is $SHA but fq-dashboard is $DSHA"
        ok "bundle is main @ $SHA (fq, watcher and dashboard agree)"
    else
        ok "bundle is main @ $SHA (fq and watcher agree; dashboard predates --version)"
    fi

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
DAEMON_PID="$(pids_installed fqd | head -1 || true)"
CRON_OK=1
if [ -f fq-cron.toml ]; then
    CRON_PID="$(pids_installed fq-cron | head -1 || true)"
    cron_exe="$(readlink "/proc/$CRON_PID/exe" 2>/dev/null || true)"
    [ "$cron_exe" = "$DOGFOOD/$REL/fq-cron" ] || CRON_OK=0
fi
if [ "$FORCE" != 1 ] && [ "$ACTIVE" = "$REL" ] && [ -n "$DAEMON_PID" ]; then
    exe="$(readlink "/proc/$DAEMON_PID/exe" 2>/dev/null || true)"
    if [ "$exe" = "$DOGFOOD/$REL/fqd" ] && [ -n "$(pids_release github-watcher)" ] \
        && [ -n "$(pids_release fq-dashboard)" ] && [ "$CRON_OK" = 1 ]; then
        ok "already running $SHA — nothing to do (--force to restart anyway)"
        exit 0
    fi
fi

# --- 3. graceful bring-down ----------------------------------------------
# The scheduler goes down first so no new fires land mid-drain; anything
# already published rides out the restart broker-side (fq-cron DESIGN
# D5/D6).
for cpid in $(pids_installed fq-cron); do
    log "Stopping cron (PID $cpid)"
    kill -TERM "$cpid" 2>/dev/null || true
    for _ in $(seq 1 15); do kill -0 "$cpid" 2>/dev/null || break; sleep 1; done
    kill -0 "$cpid" 2>/dev/null && kill -KILL "$cpid" 2>/dev/null || true
    ok "cron $cpid stopped"
done

if [ -n "$DAEMON_PID" ]; then
    DRAIN_CLI="$REL/fq"
    [ ! -x ./current/fq ] || DRAIN_CLI=./current/fq
    # Where to dial. `fq` is a client now: its own --config is `fq.toml`,
    # which is not this file, and it reads no daemon config at all — so
    # the address comes from the daemon's own `[edge] bind`, read here.
    # The token still has to come from a pairing (`fq connect`); without
    # one this fails and the escalation below stops the daemon instead.
    EDGE_ADDR="$(sed -n 's/^ *bind *= *"\([^"]*\)".*/\1/p' "$DOGFOOD/fqd.toml" | head -1)"
    [ -n "$EDGE_ADDR" ] || EDGE_ADDR=127.0.0.1:9470
    log "Stopping daemon (PID $DAEMON_PID) via confirmed fq down using $DRAIN_CLI"
    "$DRAIN_CLI" --addr "$EDGE_ADDR" down \
        || printf '    (confirmed drain failed; escalating)\n'
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        # Escalation 1: `--now` skips the already-attempted drain;
        # the daemon tears down cleanly,
        # DEREGISTERS its worker (no stale-worker cruft, #64/#65), and
        # `fq down` exits zero only after observing the daemon's own
        # system.shutdown event. If wedged, fall through to the signal.
        printf '    graceful stop failed — requesting immediate stop (fq down --now)\n'
        if "$DRAIN_CLI" --addr "$EDGE_ADDR" down --now; then
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

for wpid in $(pids_installed github-watcher); do
    log "Stopping watcher (PID $wpid)"
    kill -TERM "$wpid" 2>/dev/null || true
    for _ in $(seq 1 15); do kill -0 "$wpid" 2>/dev/null || break; sleep 1; done
    kill -0 "$wpid" 2>/dev/null && kill -KILL "$wpid" 2>/dev/null || true
    ok "watcher $wpid stopped"
done

# The dashboard must not outlive the flip: a stale binary may not decode
# the new daemon's contract types (see the header contract).
for dpid in $(pids_installed fq-dashboard); do
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

# --- 5. relaunch services (detached), verifying against fresh log lines --
daemon_log_lines="$(wc -l < logs/fq-run.log 2>/dev/null || echo 0)"
watcher_log_lines="$(wc -l < logs/watcher.log 2>/dev/null || echo 0)"

log "Relaunching daemon (current/run.sh)"
setsid ./current/run.sh >> logs/fq-run.log 2>&1 </dev/null &
log "Relaunching watcher (current/watcher.sh)"
setsid ./current/watcher.sh >> logs/watcher.log 2>&1 </dev/null &
log "Relaunching dashboard (current/dashboard.sh)"
setsid ./current/dashboard.sh >> logs/dashboard.log 2>&1 </dev/null &
if [ -f fq-cron.toml ]; then
    log "Relaunching cron (current/cron.sh)"
    setsid ./current/cron.sh >> logs/cron.log 2>&1 </dev/null &
else
    log "Skipping cron relaunch (fq-cron.toml not found)"
fi

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

NEW_DAEMON="$(pids_release fqd | head -1 || true)"
[ -n "$NEW_DAEMON" ] || die "no fqd process from $REL after relaunch (see logs/fq-run.log)"
exe="$(readlink "/proc/$NEW_DAEMON/exe" 2>/dev/null || true)"
[ "$exe" = "$DOGFOOD/$REL/fqd" ] \
    || die "daemon PID $NEW_DAEMON runs $exe, not $DOGFOOD/$REL/fqd"
ok "daemon up (PID $NEW_DAEMON) from $REL, Runtime ready"

log "Verifying watcher startup"
sleep 4
NEW_WATCHER="$(pids_release github-watcher | head -1 || true)"
[ -n "$NEW_WATCHER" ] || die "no github-watcher from $REL after relaunch (see logs/watcher.log)"
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
NEW_DASHBOARD="$(pids_release fq-dashboard | head -1 || true)"
[ -n "$NEW_DASHBOARD" ] || die "no fq-dashboard from $REL after relaunch (see logs/dashboard.log)"
dexe="$(readlink "/proc/$NEW_DASHBOARD/exe" 2>/dev/null || true)"
[ "$dexe" = "$DOGFOOD/$REL/fq-dashboard" ] \
    || die "dashboard PID $NEW_DASHBOARD runs $dexe, not $DOGFOOD/$REL/fq-dashboard"
ok "dashboard up (PID $NEW_DASHBOARD) from $REL"

if [ -f fq-cron.toml ]; then
    log "Verifying cron startup"
    NEW_CRON="$(pids_release fq-cron | head -1 || true)"
    [ -n "$NEW_CRON" ] || die "no fq-cron from $REL after relaunch (see logs/cron.log)"
    cexe="$(readlink "/proc/$NEW_CRON/exe" 2>/dev/null || true)"
    [ "$cexe" = "$DOGFOOD/$REL/fq-cron" ] \
        || die "cron PID $NEW_CRON runs $cexe, not $DOGFOOD/$REL/fq-cron"
    ok "cron up (PID $NEW_CRON) from $REL"
fi

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
printf '    dashboard PID %-8s %s\n' "$NEW_DASHBOARD" "$("$REL/fq-dashboard" --version 2>/dev/null || echo 'fq-dashboard (predates --version)')"
if [ -f fq-cron.toml ]; then
    printf '    cron      PID %-8s %s\n' "$NEW_CRON" "$("$REL/fq-cron" --version 2>/dev/null || echo 'fq-cron (predates --version)')"
fi
printf '    rollback: ops/dogfood/deploy.sh <sha>   history: ls %s/releases\n' "$DOGFOOD"
printf '════════════════════════════════════════════════════\033[0m\n'
