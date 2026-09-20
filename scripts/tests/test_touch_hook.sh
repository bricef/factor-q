#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/fq"
printf '871\n' >"$tmp/fq/current-issue"

stdout=$(printf '%s\n' \
    '{"session_id":"session-abc","cwd":"/tmp/factor q","hook_event_name":"UserPromptSubmit"}' \
    | XDG_STATE_HOME="$tmp" "$root/scripts/touch-hook.sh")
[ -z "$stdout" ] || { echo "touch hook wrote to stdout" >&2; exit 1; }
line=$(cat "$tmp/fq/touch.log")
printf '%s\n' "$line" | grep -Eq '^[0-9-]+T[0-9:]+Z'
printf '%s\n' "$line" | awk -F '\t' '
    $2 == "UserPromptSubmit" && $3 == "session-abc" &&
    $4 == "/tmp/factor q" && $5 == "871" { found=1 }
    END { exit !found }'

# Malformed input must remain silent and successful.
stdout=$(printf 'not json\n' | XDG_STATE_HOME="$tmp" "$root/scripts/touch-hook.sh")
[ -z "$stdout" ]
[ "$(wc -l <"$tmp/fq/touch.log")" -eq 1 ]
