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
# from Docker's apt repository (plus git, cron); asks the distribution's
# init to run the container runtime — the only thing we ever ask of it;
# creates the deploy user (default `fq`) in the docker group; lays out
# ~fq/fq-dogfood with compose.yml, infra/, the four scripts, an .env and
# the four secrets files from their templates (a broker token and a
# dashboard session secret generated on first run); installs the crontab
# (deploy.sh --auto hourly, hygiene.sh, a nightly backup.sh); and prints
# the steps only a human can do — write the provider key and GH_TOKEN,
# seed the volume, run the first deploy, pair, mint the dashboard token.
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
for f in compose.yml deploy.sh hygiene.sh backup.sh restore.sh crontab .env.example env.example dashboard.env.example infra/nats.conf infra/Caddyfile; do
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
apt-get install -y -qq cron git >/dev/null 2>&1 || true

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
for f in compose.yml infra/nats.conf infra/Caddyfile; do
    install -o "$FQ_USER" -g "$FQ_USER" -m 644 "$SRC/$f" "$DOGFOOD/$f"
done
for f in deploy.sh hygiene.sh backup.sh restore.sh; do
    install -o "$FQ_USER" -g "$FQ_USER" -m 755 "$SRC/$f" "$DOGFOOD/$f"
done
ok "compose.yml, infra/, deploy.sh, hygiene.sh, backup.sh, restore.sh (refreshed)"

# Host-authored files: created from their templates once, never touched again.
seed() {  # $1 = template, $2 = destination, $3 = mode
    if [ -f "$2" ]; then ok "$(basename "$2") exists — kept"; return 1; fi
    install -o "$FQ_USER" -g "$FQ_USER" -m "$3" "$1" "$2"; ok "$(basename "$2") created from $(basename "$1")"
}
seed "$SRC/.env.example" "$DOGFOOD/.env" 644 || true
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
    printf '# DASH_USER and DASH_HASH (docker run --rm caddy:2 caddy hash-password) gate the dashboard.\nDASH_USER=\nDASH_HASH=\nDASH_COOKIE=%s\n' "$cookie" > "$DOGFOOD/.secrets/caddy.env"
    chown "$FQ_USER:$FQ_USER" "$DOGFOOD/.secrets/caddy.env"; chmod 600 "$DOGFOOD/.secrets/caddy.env"
    ok "caddy.env created — DASH_COOKIE generated; DASH_USER and DASH_HASH are yours to fill"
else
    ok "caddy.env exists — kept"
fi

# --- 5. the schedule ----------------------------------------------------------------------------
if command -v crontab >/dev/null 2>&1; then
    sed "s#^FQ_DOGFOOD=.*#FQ_DOGFOOD=$DOGFOOD#" "$SRC/crontab" | crontab -u "$FQ_USER" -
    ok "crontab installed for $FQ_USER (deploy.sh --auto hourly, hygiene.sh every 30 min, backup.sh nightly)"
else
    printf '\033[1;33m    ⚠ no crontab on this host — install cron, then: sed "s#^FQ_DOGFOOD=.*#FQ_DOGFOOD=%s#" %s/crontab | crontab -u %s -\033[0m\n' "$DOGFOOD" "$SRC" "$FQ_USER"
fi

# --- done --------------------------------------------------------------------------------------------
printf '\n\033[1;32m════════════════════════════════════════════════════\n'
printf '  BOOTSTRAPPED — %s for user %s\n' "$DOGFOOD" "$FQ_USER"
printf '════════════════════════════════════════════════════\033[0m\n'
cat <<NEXT

Left for you (ops/dogfood/README.md, "Bootstrap"):
  1. $DOGFOOD/.secrets/env         — ANTHROPIC_API_KEY, GH_TOKEN (the broker token is already in)
     $DOGFOOD/.secrets/caddy.env   — DASH_USER, DASH_HASH
     docker login ghcr.io          — as $FQ_USER, if the packages are private
  2. Seed the instance volume: fqd.toml (edge bind 0.0.0.0:9470, workspace path, token_env),
     fq-cron.toml, agents/ — or restore.sh <backup-set> to bring an instance across.
  3. sudo -iu $FQ_USER $DOGFOOD/deploy.sh      # first deploy: pulls main-latest, brings the stack up
  4. Pair the container's fq, then mint the dashboard token into .secrets/dashboard.env
     and 'docker compose up -d fq-dashboard'.
The crontab is already active: deploy.sh --auto will not deploy until the daemon can be asked
whether it is idle, i.e. until step 4 is done.
NEXT
