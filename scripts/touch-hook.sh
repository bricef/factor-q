#!/bin/sh
# Claude Code hook: append session timing events without writing to stdout.
(
    payload=$(cat) || exit 0
    field() {
        printf '%s\n' "$payload" | sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"\\]*\)".*/\1/p' | sed -n '1p'
    }

    session_id=$(field session_id)
    cwd=$(field cwd)
    event=$(field hook_event_name)
    [ -n "$session_id" ] && [ -n "$event" ] || exit 0

    state_home=${XDG_STATE_HOME:-"${HOME:-}/.local/state"}
    state_dir=$state_home/fq
    mkdir -p "$state_dir" 2>/dev/null || exit 0
    issue=
    if [ -r "$state_dir/current-issue" ]; then
        issue=$(sed -n '1p' "$state_dir/current-issue" 2>/dev/null)
    fi
    timestamp=$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null) || exit 0
    # Tabs and newlines would corrupt the line-oriented format. JSON strings
    # supplied by Claude Code do not contain literal newlines; strip tabs as a
    # final safeguard for hand-fed payloads.
    cwd=$(printf '%s' "$cwd" | sed 's/\t/ /g')
    printf '%s\t%s\t%s\t%s\t%s\n' \
        "$timestamp" "$event" "$session_id" "$cwd" "$issue" >>"$state_dir/touch.log" 2>/dev/null || :
) >/dev/null 2>&1
exit 0
