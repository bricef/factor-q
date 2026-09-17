#!/usr/bin/env bash
# The deploy's refresh of the tracked files — a build's compose.yml and
# infra/ laid over an instance's — driven through the seam
# `deploy.sh --refresh-tracked <build stack dir> <instance dir>` on two
# scratch trees, with no image and no docker. What is checked: a file
# that differs is replaced with the build's; one that matches is left
# untouched; one the build does not carry is left as it was and said so;
# and the host-authored files beside them — .env, the override, the
# secrets — are never touched, which is the property the whole design
# rests on.
#
#   ops/dogfood/tests/refresh-tracked.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
deploy="$here/../deploy.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
build="$work/build"; inst="$work/instance"
mkdir -p "$build/infra" "$inst/infra" "$inst/.secrets"
failed=0
expect() {  # expect <name> <command...>: the command's exit status is the verdict
    local name="$1"; shift
    if "$@"; then printf '  ok   %s\n' "$name"
    else printf '  FAIL %s\n' "$name"; failed=1; fi
}
refresh() { out="$(bash "$deploy" --refresh-tracked "$build" "$inst" 2>&1)"; rc=$?; }
said() { [ "$rc" = 0 ] && grep -q "$1" <<<"$out"; }
unchanged() { [ "$(snapshot)" = "$before" ]; }
only_compose_refreshed() { [ "$rc" = 0 ] && [ "$out" = "    compose.yml: refreshed" ]; }
snapshot() { (cd "$inst" && find . -type f | sort | xargs md5sum); }
content() { [ "$(cat "$1")" = "$2" ]; }

# The instance as bootstrap left it, plus what a human wrote.
printf 'services: {fqd: {memory: 24g}}\n' > "$inst/compose.yml"
for f in nats.conf Caddyfile Caddyfile.internal; do printf 'old %s\n' "$f" > "$inst/infra/$f"; done
printf 'FQ_TAG=abcdef123456\n' > "$inst/.env"
printf 'services: {caddy: {}}\n' > "$inst/compose.override.yml"
printf 'GH_TOKEN=secret\n' > "$inst/.secrets/env"

# 1. The build matches the instance: nothing changes, and it says so.
cp "$inst/compose.yml" "$build/compose.yml"; cp "$inst"/infra/* "$build/infra/"
before="$(snapshot)"
refresh
expect "a matching build changes nothing and says so" said 'already match the build'
expect "and every file is byte-identical afterwards" unchanged

# 2. The build's compose.yml differs: it is laid over, the rest untouched.
printf 'services: {fqd: {memory: 12g}}\n' > "$build/compose.yml"
refresh
expect "a changed compose.yml is reported, and only it" only_compose_refreshed
expect "the instance now has the build's compose.yml" cmp -s "$build/compose.yml" "$inst/compose.yml"
expect "an unchanged config is left alone" content "$inst/infra/nats.conf" "old nats.conf"
expect "no temporary file is left behind" [ ! -e "$inst/compose.yml.new" ]

# 3. A build without one of the files (older than the file) leaves the
#    instance's copy and says so, rather than deleting it.
rm "$build/infra/Caddyfile.internal"
refresh
expect "a file the build does not carry is left as is, and said so" said 'infra/Caddyfile.internal: not in this build, left as is'
expect "and it is still there" content "$inst/infra/Caddyfile.internal" "old Caddyfile.internal"

# 4. Host-authored files are never touched, whatever the build carries.
printf 'FQ_TAG=999999999999\n' > "$build/.env"
printf 'GH_TOKEN=leak\n' > "$build/env"
refresh
expect ".env is never touched" content "$inst/.env" "FQ_TAG=abcdef123456"
expect "nor the override" content "$inst/compose.override.yml" "services: {caddy: {}}"
expect "nor the secrets" content "$inst/.secrets/env" "GH_TOKEN=secret"

# 5. The seam refuses a wrong call rather than guessing a directory.
if bash "$deploy" --refresh-tracked "$build" >/dev/null 2>&1; then printf '  FAIL a missing argument was accepted\n'; failed=1
else printf '  ok   a missing argument is refused\n'; fi

if [ "$failed" = 0 ]; then echo "refresh-tracked: all cases pass"; else echo "refresh-tracked: FAILED" >&2; exit 1; fi
