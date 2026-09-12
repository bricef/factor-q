#!/usr/bin/env bash
# Regenerate the fq-dashboard icon set in services/fq-dashboard/assets/.
#
#   scripts/dashboard-icon.sh
#
# Two steps, both deterministic:
#   1. icon.svg — the "fq" mark as paths, extracted from DejaVu Sans Mono
#      Bold by dashboard-icon.py (fontTools; run through `uv run` so no
#      system install is needed).
#   2. apple-touch-icon.png (180px, iOS), icon-192.png and icon-512.png
#      (the web app manifest, Android) — headless chromium screenshots
#      of the SVG at each size, over file://, exactly as
#      dashboard-screenshots.sh renders pages. Opaque on purpose: iOS
#      paints a transparent corner black.
#
# Browser resolution: $CHROMIUM, else chromium/chrome on PATH, else a
# playwright-cache install (~/.cache/ms-playwright).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
assets="$root/services/fq-dashboard/assets"

find_browser() {
    if [ -n "${CHROMIUM:-}" ]; then echo "$CHROMIUM"; return; fi
    for c in chromium chromium-browser google-chrome google-chrome-stable; do
        if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return; fi
    done
    ls -1 "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux*/chrome 2>/dev/null | sort | tail -1
}

browser="$(find_browser)"
if [ -z "$browser" ]; then
    echo "no chromium/chrome found — set \$CHROMIUM to a browser binary" >&2
    exit 1
fi
command -v uv >/dev/null 2>&1 || { echo "uv is needed to run dashboard-icon.py with fontTools" >&2; exit 1; }

uv run -q --with fonttools python3 "$root/scripts/dashboard-icon.py" > "$assets/icon.svg"
echo "$assets/icon.svg"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$assets/icon.svg" "$work/icon.svg"

render() {
    # render <size> <out.png>: an <img> of the SVG at exactly <size> CSS px
    # in a <size>-px window. --no-sandbox as in dashboard-screenshots.sh.
    local size="$1" out="$2"
    printf '<!doctype html><meta charset="utf-8"><body style="margin:0;background:#14161a"><img src="icon.svg" style="display:block;width:%spx;height:%spx">' \
        "$size" "$size" > "$work/wrap-$size.html"
    "$browser" --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
        --force-device-scale-factor=1 --window-size="$size,$size" \
        --screenshot="$out" "file://$work/wrap-$size.html" 2>/dev/null
    echo "$out"
}

render 180 "$assets/apple-touch-icon.png"
render 192 "$assets/icon-192.png"
render 512 "$assets/icon-512.png"
