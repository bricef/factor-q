#!/usr/bin/env bash
# ops/dogfood/notify.sh — one message to wherever the operator looks.
#
#   notify.sh <subject>              body on stdin (may be empty)
#   notify.sh <subject> <body...>    body from the arguments
#   notify.sh --test                 send a test message through the hook
#
# The unattended scripts call this for anything that needs a human:
# deploy.sh --auto for a deploy, a rollback, a failure, or a deferral
# that has gone on too long; hygiene.sh for a threshold crossed;
# backup.sh --auto for a failed backup. The crontab sends every script's
# output to a file under logs/, so cron mail never fires — without a
# channel of its own a warning would sit in a log until someone looked.
#
# The channel is FQ_NOTIFY_HOOK in $FQ_DOGFOOD/.env: a shell command, run
# with the subject as $1 and the body on stdin (examples in .env.example:
# ntfy, Slack, mail). Every message is also appended to logs/notify.log,
# hook or no hook, so the host keeps its own record; with no hook that is
# where it stays, and this script says so on stderr — one line in the
# calling script's log, next to the thing it could not deliver.
#
# Never fails its caller: a broken hook is reported, not propagated. The
# exit status is the hook's, for `notify.sh --test`.
set -uo pipefail

DOGFOOD="${FQ_DOGFOOD:-$HOME/fq-dogfood}"
cd "$DOGFOOD" 2>/dev/null || { echo "notify: dogfood dir not found: $DOGFOOD" >&2; exit 2; }
envval() { sed -n "s/^$1=\(.*\)$/\1/p" .env 2>/dev/null | tail -1; }
HOOK="${FQ_NOTIFY_HOOK:-$(envval FQ_NOTIFY_HOOK)}"

[ $# -ge 1 ] || { echo "usage: notify.sh <subject> [body...]   (body on stdin otherwise)" >&2; exit 2; }
if [ "$1" = "--test" ]; then
    set -- "test message" "notify.sh works: this reached you through FQ_NOTIFY_HOOK."
fi
subject="$1"; shift
if [ $# -gt 0 ]; then body="$*"; elif [ ! -t 0 ]; then body="$(cat)"; else body=""; fi
host="$(hostname -s 2>/dev/null || hostname)"
stamp="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

mkdir -p logs
{
    printf '%s %s\n' "$stamp" "$subject"
    [ -n "$body" ] && printf '%s\n' "$body" | sed 's/^/    /'
} >> logs/notify.log

if [ -z "$HOOK" ]; then
    echo "notify: no FQ_NOTIFY_HOOK in $DOGFOOD/.env — \"$subject\" is in logs/notify.log and nowhere else" >&2
    exit 0
fi
printf '%s\n\n— %s, %s' "$body" "$host" "$stamp" | bash -c "$HOOK" notify "fq-dogfood: $subject"
rc=$?
[ "$rc" = 0 ] || echo "notify: FQ_NOTIFY_HOOK exited $rc for \"$subject\" (the message is in logs/notify.log)" >&2
exit "$rc"
