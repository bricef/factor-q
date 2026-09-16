#!/usr/bin/env bash
# The restore guard distinguishes image-seeded directory layout from instance
# content. Exercise the classifier directly against temporary directories; the
# ops CI environment has the compose plugin but intentionally needs no daemon.
#
#   ops/dogfood/tests/volume-content.sh        (just ops-ci)
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
code="$(mktemp)"
trap 'rm -rf "$work" "$code"' EXIT

# Extract the function restore.sh injects into its one-off probe container, so
# these tests execute the production classifier rather than a copy of it.
sed -n '/^volume_content() {$/,/^}$/p' "$here/../restore.sh" > "$code"
# shellcheck source=/dev/null
source "$code"

failed=0
expect_empty() {
    local name="$1" got
    got="$(volume_content "$work")"
    if [ -z "$got" ]; then printf '  ok   %s\n' "$name"
    else printf '  FAIL %s: reported %s\n' "$name" "$got"; failed=1; fi
}
expect_content() {
    local name="$1" got
    got="$(volume_content "$work")"
    if [ -n "$got" ]; then printf '  ok   %s\n' "$name"
    else printf '  FAIL %s: reported empty\n' "$name"; failed=1; fi
}

mkdir -p "$work"/{agents,state/client,cache,workspace,build,home,lost+found}
expect_empty "the image-seeded directory skeleton is empty"

touch "$work/state/client/connections.toml"
expect_content "a file below state/client is instance content"
rm "$work/state/client/connections.toml"

for dir in agents cache workspace build home; do
    touch "$work/$dir/instance-file"
    expect_content "a file below $dir is instance content"
    rm "$work/$dir/instance-file"
done

mkdir "$work/unexpected"
expect_content "an unknown empty top-level directory is content"
rmdir "$work/unexpected"

touch "$work/lost+found/fsck-artifact"
expect_empty "lost+found contents are ignored"

[ "$failed" = 0 ] && echo "volume-content: all cases pass" || { echo "volume-content: FAILED" >&2; exit 1; }
