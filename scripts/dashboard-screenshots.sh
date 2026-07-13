#!/usr/bin/env bash
# Screenshot every fq-dashboard page from canned fixtures.
#
#   scripts/dashboard-screenshots.sh [out-dir]     (default: dist/dashboard-screenshots)
#
# Pipeline: `fq-dashboard render-fixtures` writes deterministic static
# HTML (no daemon, no broker), then headless chromium screenshots each
# page over file:// — no server ever runs, so this works in sandboxes
# that kill background processes and is stable in CI (fixed fixture
# timestamps mean a visual diff is a rendering change, never the clock).
#
# Browser resolution: $CHROMIUM, else chromium/chrome on PATH, else a
# playwright-cache install (~/.cache/ms-playwright).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/dist/dashboard-screenshots}"
# Absolutise: the screenshots load via file:// URLs, which require
# absolute paths (a relative out-dir otherwise screenshots chromium's
# own ERR_INVALID_URL page — found by looking at the first run's PNGs).
mkdir -p "$out"
out="$(cd "$out" && pwd)"
html="$out/html"

find_browser() {
    if [ -n "${CHROMIUM:-}" ]; then echo "$CHROMIUM"; return; fi
    for c in chromium chromium-browser google-chrome google-chrome-stable; do
        if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return; fi
    done
    # Playwright's cached chromium (newest install wins).
    ls -1 "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux*/chrome 2>/dev/null | sort | tail -1
}

browser="$(find_browser)"
if [ -z "$browser" ]; then
    echo "no chromium/chrome found — set \$CHROMIUM to a browser binary" >&2
    exit 1
fi

mkdir -p "$html"
cargo run -q --manifest-path "$root/services/fq-dashboard/Cargo.toml" \
    -- render-fixtures --out "$html" >/dev/null

shot() {
    # --no-sandbox: the devbox chromium cannot create its own sandbox
    # inside the agent sandbox; the input is our own generated HTML.
    "$browser" --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
        --force-device-scale-factor=1 --window-size=1100,900 \
        --screenshot="$1" "file://$2" 2>/dev/null
}

count=0
for f in "$html"/*.html; do
    name="$(basename "$f" .html)"
    shot "$out/$name.png" "$f"
    echo "$out/$name.png"
    count=$((count + 1))
done
[ "$count" -gt 0 ] || { echo "no fixture pages were rendered" >&2; exit 1; }
