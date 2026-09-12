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

if bash "$fq_ops" probe 2>/dev/null; then printf '  FAIL probe passes outside the scheduler\n'; failed=1
else printf '  ok   probe fails outside the scheduler\n'; fi

if bash "$fq_ops" help | grep -q 'fq-ops --version'; then printf '  ok   help prints the usage\n'
else printf '  FAIL help does not print the usage\n'; failed=1; fi

[ "$failed" = 0 ] && echo "fq-ops: all cases pass" || { echo "fq-ops: FAILED" >&2; exit 1; }
