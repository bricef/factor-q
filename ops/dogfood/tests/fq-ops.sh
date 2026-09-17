#!/usr/bin/env bash
# The ops image's entrypoint, driven outside the image: FQ_OPS_LIB and
# FQ_OPS_ETC point it at a scratch tree, so the version answer, the verb
# dispatch and the refusals are checked without docker or supercronic.
# What needs the image — `check`, the crontab parse, the toolset — is
# `just docker-check`'s.
#
#   ops/dogfood/tests/fq-ops.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
fq_ops="$here/../fq-ops"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/lib" "$work/etc"
printf 'abcdef123456\n' > "$work/etc/version"
printf '#!/usr/bin/env bash\nprintf "notify: %%s\\n" "$*"\n' > "$work/lib/notify.sh"
chmod +x "$work/lib/notify.sh"
export FQ_OPS_LIB="$work/lib" FQ_OPS_ETC="$work/etc"
failed=0

check() {  # check <name> <want> <<< got
    local name="$1" want="$2" got
    got="$(cat)"
    if [ "$got" = "$want" ]; then printf '  ok   %s\n' "$name"
    else printf '  FAIL %s\n       want: %s\n       got:  %s\n' "$name" "$want" "$got"; failed=1; fi
}

check "--version names the stamped commit" "fq-ops abcdef123456" <<< "$(bash "$fq_ops" --version)"
check "version is the same answer" "fq-ops abcdef123456" <<< "$(bash "$fq_ops" version)"
check "a verb runs its script with the arguments" "notify: deploy FAILED body" <<< "$(bash "$fq_ops" notify deploy FAILED body)"

if bash "$fq_ops" frobnicate >/dev/null 2>&1; then printf '  FAIL an unknown verb exited 0\n'; failed=1
else printf '  ok   an unknown verb is refused (exit %s)\n' "$(bash "$fq_ops" frobnicate >/dev/null 2>&1; echo $?)"; fi
[ "$(bash "$fq_ops" frobnicate 2>&1 >/dev/null; true)" != "" ] && printf '  ok   the refusal says which verb\n' || { printf '  FAIL the refusal is silent\n'; failed=1; }

if bash "$fq_ops" probe 2>/dev/null; then printf '  FAIL probe passes with no scheduler process\n'; failed=1
else printf '  ok   probe fails with no scheduler process\n'; fi
# A process whose comm is `supercronic` — anywhere, not PID 1: the service
# runs under an init — is what the probe looks for. A symlink's basename
# is the comm of what it executes.
ln -s "$(command -v sleep)" "$work/supercronic"
"$work/supercronic" 30 & fake=$!
if bash "$fq_ops" probe 2>/dev/null; then printf '  ok   probe passes while a supercronic process runs\n'
else printf '  FAIL probe fails with a supercronic process running\n'; failed=1; fi
kill "$fake" 2>/dev/null; wait "$fake" 2>/dev/null

if bash "$fq_ops" help | grep -q 'fq-ops --version'; then printf '  ok   help prints the usage\n'
else printf '  FAIL help does not print the usage\n'; failed=1; fi

# The stack definition the image carries, streamed as a tar for the
# deploy to lay over the instance's — and refused when the image has
# none, so a deploy never lays out an empty directory.
if bash "$fq_ops" stack >/dev/null 2>&1; then printf '  FAIL stack streams with no stack directory in the image\n'; failed=1
else printf '  ok   stack is refused when the image carries none\n'; fi
mkdir -p "$work/etc/stack/infra"
printf 'services: {}\n' > "$work/etc/stack/compose.yml"
for f in nats.conf Caddyfile Caddyfile.internal; do printf '# %s\n' "$f" > "$work/etc/stack/infra/$f"; done
check "stack streams compose.yml and infra/ as a tar" \
    "compose.yml infra/Caddyfile infra/Caddyfile.internal infra/nats.conf" \
    <<< "$(bash "$fq_ops" stack | tar -tf - | sort | tr '\n' ' ' | sed 's/ $//')"
# `check` with the toolset stubbed on PATH (each answers nothing and exits
# 0), the four other scripts present and an empty crontab: what is left
# for it to judge is the stack definition.
mkdir -p "$work/bin"
for tool in supercronic docker jq curl; do printf '#!/bin/sh\nexit 0\n' > "$work/bin/$tool"; chmod +x "$work/bin/$tool"; done
for s in deploy hygiene backup restore; do printf '#!/usr/bin/env bash\n' > "$work/lib/$s.sh"; chmod +x "$work/lib/$s.sh"; done
: > "$work/etc/crontab"
check "check counts the stack definition present" "fq-ops abcdef123456: crontab parses, toolset answers, 5 scripts and the stack definition present" \
    <<< "$(PATH="$work/bin:$PATH" bash "$fq_ops" check 2>/dev/null | tail -1)"
rm "$work/etc/stack/infra/nats.conf"
if PATH="$work/bin:$PATH" bash "$fq_ops" check >/dev/null 2>&1; then printf '  FAIL check passes with a stack file missing\n'; failed=1
else printf '  ok   check fails when a stack file is missing\n'; fi

[ "$failed" = 0 ] && echo "fq-ops: all cases pass" || { echo "fq-ops: FAILED" >&2; exit 1; }
