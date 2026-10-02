#!/bin/sh
# Render the website's social preview image, website/public/og.png, from
# website/og/og.html. The PNG is committed so the website deploys without
# a browser.
#
# Requires: Google Chrome or Chromium (set CHROME to override), and network
# access for the Geist Mono web font.
set -eu

cd "$(dirname "$0")/.."

CHROME="${CHROME:-}"
if [ -z "$CHROME" ]; then
    for candidate in \
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
        google-chrome chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1 || [ -x "$candidate" ]; then
            CHROME="$candidate"
            break
        fi
    done
fi
if [ -z "$CHROME" ]; then
    echo "error: need Chrome or Chromium; set CHROME to its path" >&2
    exit 1
fi

OUT="website/public/og.png"
"$CHROME" --headless --disable-gpu --hide-scrollbars \
    --force-device-scale-factor=1 --window-size=1200,630 \
    --virtual-time-budget=5000 \
    --screenshot="$PWD/$OUT" "file://$PWD/website/og/og.html" 2>/dev/null

file "$OUT"
