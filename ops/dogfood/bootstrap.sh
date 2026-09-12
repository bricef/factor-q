#!/usr/bin/env bash
# ops/dogfood/bootstrap.sh — provision a fresh, dedicated Debian or Ubuntu
# host for the dogfood stack (ADR-0035). Run as root, and run it again
# whenever the tracked files it lays out change: it is idempotent and
# never overwrites a secret, an .env, or a volume.
#
#   sudo ops/dogfood/bootstrap.sh                 # from a checkout
#   curl -fsSL https://raw.githubusercontent.com/bricef/factor-q/main/ops/dogfood/bootstrap.sh | sudo bash
#                                                 # from nothing: clones the repo to /opt/factor-q first
#
# What it does, in order: installs Docker Engine and the compose plugin
# from Docker's apt repository (plus git); asks the distribution's init
# to run the container runtime — the only thing we ever ask of it;
# creates the deploy user (default `fq`) in the docker group; lays out
# ~fq/fq-dogfood with compose.yml, infra/, an .env and the four secrets
# files from their templates (a broker token and a dashboard session
# secret generated on first run), writing the four host facts the ops
# service needs into .env; removes a host crontab and script copies left
# from before ADR-0036 (the stack schedules its own operations now — the
# hourly deploy, hygiene, the nightly backup — from the `ops` service);
# and prints the steps only a human can do — write the provider key and
# GH_TOKEN, seed the volume, run the first deploy, pair, mint the
# dashboard token.
#
# Assumes a dedicated host: nothing else listens on 443, 9470 or 9472,
# and the box is ours to configure. Inbound 443 and 22 are the
# provider's firewall's business, not this script's.
#
# Knobs: FQ_USER (fq), FQ_REPO_URL (https://github.com/bricef/factor-q),
# FQ_REF (main), FQ_REPO_DIR (/opt/factor-q — used only when not run from
# a checkout).
set -euo pipefail

FQ_USER="${FQ_USER:-fq}"
FQ_REPO_URL="${FQ_REPO_URL:-https://github.com/bricef/factor-q}"
FQ_REF="${FQ_REF:-main}"
FQ_REPO_DIR="${FQ_REPO_DIR:-/opt/factor-q}"

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
ok()  { printf '\033[1;32m    ✓ %s\033[0m\n' "$*"; }
die() { printf '\n\033[1;31m✗ ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root (sudo)"
command -v apt-get >/dev/null || die "this script provisions Debian/Ubuntu (apt); adapt it for anything else"
. /etc/os-release 2>/dev/null || die "cannot read /etc/os-release"
case "${ID:-}" in debian|ubuntu) ;; *) die "unsupported distribution '${ID:-?}' — Debian or Ubuntu" ;; esac
export DEBIAN_FRONTEND=noninteractive

# --- 1. the source of the tracked files -----------------------------------------------
# Run from a checkout: this directory. Run standalone (piped from curl):
# clone the repository first, so a later re-run comes from the same place.
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)"
if [ -n "$SELF_DIR" ] && [ -f "$SELF_DIR/compose.yml" ] && [ -f "$SELF_DIR/deploy.sh" ]; then
    SRC="$SELF_DIR"
else
    log "Fetching the repository to $FQ_REPO_DIR ($FQ_REF)"
    apt-get update -qq && apt-get install -y -qq git >/dev/null
    if [ -d "$FQ_REPO_DIR/.git" ]; then
        git -C "$FQ_REPO_DIR" fetch -q origin "$FQ_REF" && git -C "$FQ_REPO_DIR" checkout -q -B "$FQ_REF" "origin/$FQ_REF"
    else
        git clone -q --branch "$FQ_REF" "$FQ_REPO_URL" "$FQ_REPO_DIR"
    fi
    SRC="$FQ_REPO_DIR/ops/dogfood"
    ok "$SRC at $(git -C "$FQ_REPO_DIR" rev-parse --short=12 HEAD)"
fi
for f in compose.yml .env.example env.example dashboard.env.example infra/nats.conf infra/Caddyfile infra/Caddyfile.internal; do
    [ -f "$SRC/$f" ] || die "missing $SRC/$f — an incomplete checkout?"
done

# --- 2. Docker Engine + compose, from Docker's repository -------------------------------
if docker compose version >/dev/null 2>&1; then
    ok "docker $(docker --version | sed 's/Docker version //;s/,.*//') with compose $(docker compose version --short) already installed"
else
    log "Installing Docker Engine and the compose plugin"
    apt-get update -qq
    apt-get install -y -qq ca-certificates curl gnupg >/dev/null
    install -m 0755 -d /etc/apt/keyrings
    if [ ! -f /etc/apt/keyrings/docker.asc ]; then
        curl -fsSL "https://download.docker.com/linux/$ID/gpg" -o /etc/apt/keyrings/docker.asc
        chmod a+r /etc/apt/keyrings/docker.asc
    fi
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/$ID ${VERSION_CODENAME} stable" \
        > /etc/apt/sources.list.d/docker.list
    apt-get update -qq
    apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-compose-plugin >/dev/null
    ok "docker $(docker --version | sed 's/Docker version //;s/,.*//') with compose $(docker compose version --short)"
fi
# The distribution's init runs the container runtime. That is the one
# thing we ask of it; everything of ours is supervised by compose.
if command -v systemctl >/dev/null 2>&1; then
    systemctl enable --now docker >/dev/null 2>&1 || true
fi
docker info >/dev/null 2>&1 || die "the docker daemon is not running"
apt-get install -y -qq git >/dev/null 2>&1 || true

# --- 3. the deploy user -----------------------------------------------------------------------
if id "$FQ_USER" >/dev/null 2>&1; then
    ok "user $FQ_USER exists"
else
    useradd -m -s /bin/bash "$FQ_USER"
    ok "created user $FQ_USER"
fi
usermod -aG docker "$FQ_USER"
HOME_DIR="$(getent passwd "$FQ_USER" | cut -d: -f6)"
DOGFOOD="$HOME_DIR/fq-dogfood"

# --- 4. the tree ------------------------------------------------------------------------------
log "Laying out $DOGFOOD"
install -d -o "$FQ_USER" -g "$FQ_USER" -m 755 "$DOGFOOD" "$DOGFOOD/infra" "$DOGFOOD/logs" "$DOGFOOD/backups"
install -d -o "$FQ_USER" -g "$FQ_USER" -m 700 "$DOGFOOD/.secrets"
# Tracked files: always refreshed — this is how a change to the stack or
# a script reaches the host.
for f in compose.yml infra/nats.conf infra/Caddyfile infra/Caddyfile.internal; do
    install -o "$FQ_USER" -g "$FQ_USER" -m 644 "$SRC/$f" "$DOGFOOD/$f"
done
ok "compose.yml, infra/ (both Caddyfiles) refreshed"
# The scripts live in the fq-ops image now (ADR-0036) and run as
# `docker compose run --rm ops <verb>`; a copy left here from before is
# a stale one somebody might run.
stale=""
for f in deploy.sh hygiene.sh backup.sh restore.sh notify.sh; do
    [ -f "$DOGFOOD/$f" ] && { rm -f "$DOGFOOD/$f"; stale="$stale $f"; }
done
[ -z "$stale" ] || ok "removed the pre-ADR-0036 script copies:$stale — the ops service runs them from its image"

# Host-authored files: created from their templates once, never touched again.
seed() {  # $1 = template, $2 = destination, $3 = mode
    if [ -f "$2" ]; then ok "$(basename "$2") exists — kept"; return 1; fi
    install -o "$FQ_USER" -g "$FQ_USER" -m "$3" "$1" "$2"; ok "$(basename "$2") created from $(basename "$1")"
}
seed "$SRC/.env.example" "$DOGFOOD/.env" 644 || true
# The host as the ops service sees it (ADR-0036): four values compose
# refuses to start the service without. Filled when the template left
# them empty, appended when an .env predates them, never changed once
# set — the same rule as every other host-authored value.
ensure_env() {  # $1 = KEY, $2 = value
    if grep -q "^$1=" "$DOGFOOD/.env"; then
        [ -n "$(sed -n "s/^$1=\(.*\)$/\1/p" "$DOGFOOD/.env" | tail -1)" ] && return 0
        sed -i "s#^$1=.*#$1=$2#" "$DOGFOOD/.env"
    else
        printf '%s=%s\n' "$1" "$2" >> "$DOGFOOD/.env"
    fi
    ok ".env: $1=$2"
}
ensure_env FQ_DOGFOOD "$DOGFOOD"
ensure_env FQ_UID "$(id -u "$FQ_USER")"
ensure_env FQ_DOCKER_GID "$(getent group docker | cut -d: -f3)"
ensure_env FQ_HOST "$(hostname -s 2>/dev/null || hostname)"
if seed "$SRC/env.example" "$DOGFOOD/.secrets/env" 600; then
    token="$(openssl rand -hex 32 2>/dev/null || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    sed -i "s/^FQ_NATS_TOKEN=.*/FQ_NATS_TOKEN=$token/; s#^GHW_NATS_URL=.*#GHW_NATS_URL=nats://$token@nats:4222#; s#^FQCRON_NATS_URL=.*#FQCRON_NATS_URL=nats://$token@nats:4222#" "$DOGFOOD/.secrets/env"
    if [ ! -f "$DOGFOOD/.secrets/nats-auth.conf" ]; then
        printf 'authorization { token: "%s" }\n' "$token" > "$DOGFOOD/.secrets/nats-auth.conf"
        chown "$FQ_USER:$FQ_USER" "$DOGFOOD/.secrets/nats-auth.conf"; chmod 600 "$DOGFOOD/.secrets/nats-auth.conf"
        ok "nats-auth.conf created — one generated broker token, in all four places"
    fi
fi
[ -f "$DOGFOOD/.secrets/nats-auth.conf" ] || die ".secrets/env exists but .secrets/nats-auth.conf does not — write it with the token .secrets/env carries"
seed "$SRC/dashboard.env.example" "$DOGFOOD/.secrets/dashboard.env" 600 || true
if [ ! -f "$DOGFOOD/.secrets/caddy.env" ]; then
    cookie="$(openssl rand -hex 32 2>/dev/null || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    printf '# DASH_USER and DASH_HASH (docker run --rm caddy:2 caddy hash-password) gate the dashboard.\nDASH_USER=\nDASH_HASH=\nDASH_COOKIE=%s\n# On a host with no public address: the tunnel-side address Caddy binds (infra/Caddyfile.internal, README "An internal host").\n# DASH_INTERNAL_ADDR=\n' "$cookie" > "$DOGFOOD/.secrets/caddy.env"
    chown "$FQ_USER:$FQ_USER" "$DOGFOOD/.secrets/caddy.env"; chmod 600 "$DOGFOOD/.secrets/caddy.env"
    ok "caddy.env created — DASH_COOKIE generated; DASH_USER and DASH_HASH are yours to fill"
else
    ok "caddy.env exists — kept"
fi

# --- 5. the schedule ----------------------------------------------------------------------------
# Lives in the stack (ADR-0036): the `ops` service runs the image's
# crontab — the hourly deploy, hygiene, the nightly backup — from the
# moment `docker compose up -d` brings it up. A host crontab from before
# would run the deploy twice an hour from two places; take ours out.
if command -v crontab >/dev/null 2>&1 && crontab -l -u "$FQ_USER" 2>/dev/null | grep -q 'deploy.sh --auto'; then
    crontab -r -u "$FQ_USER"
    ok "host crontab removed — the ops service schedules the deploy, hygiene and backup now"
fi

# --- done --------------------------------------------------------------------------------------------
printf '\n\033[1;32m════════════════════════════════════════════════════\n'
printf '  BOOTSTRAPPED — %s for user %s\n' "$DOGFOOD" "$FQ_USER"
printf '════════════════════════════════════════════════════\033[0m\n'
cat <<NEXT

Left for you (ops/dogfood/README.md, "Bootstrap"):
  1. $DOGFOOD/.secrets/env         — ANTHROPIC_API_KEY, OPENROUTER_API_KEY, GH_TOKEN (the broker token is already in)
                                     one key per provider fqd.toml declares, or the daemon will not start
     $DOGFOOD/.secrets/caddy.env   — DASH_USER, DASH_HASH
     no public address?            — DASH_INTERNAL_ADDR in caddy.env + compose.override.yml (README, "An internal host")
     docker login ghcr.io          — as $FQ_USER, if the packages are private
     $DOGFOOD/.env                 — FQ_NOTIFY_HOOK, where a rollback or a warning should reach you
                                     (then, as $FQ_USER in $DOGFOOD: docker compose run --rm ops notify --test)
     $DOGFOOD/.env                 — FQ_TAG: a build, so the ops image itself can be pulled
                                     (docker run --rm ghcr.io/bricef/fq-dogfood:main-latest --version)
  2. Seed the instance volume: fqd.toml (edge bind 0.0.0.0:9470, workspace path, token_env),
     fq-cron.toml, agents/ — or docker compose run --rm ops restore <backup-set> --yes to bring an instance across.
  3. As $FQ_USER in $DOGFOOD: docker compose run --rm ops deploy      # first deploy: pulls main-latest, brings the stack up
  4. Pair the container's fq, then mint the dashboard token into .secrets/dashboard.env
     and 'docker compose up -d fq-dashboard'.
The ops service's schedule is live once the stack is up: deploy --auto will not deploy until
the daemon can be asked whether it is idle, i.e. until step 4 is done.
NEXT
