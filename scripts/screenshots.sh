#!/bin/sh
# Regenerate the README screenshots in docs/screenshots/.
# Needs Google Chrome (or CHROME=/path/to/chromium) and the JetBrainsMono Nerd Font.
set -eu
cd "$(dirname "$0")/.."

CHROME=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
html=target/screenshots/html
out=docs/screenshots
mkdir -p "$html" "$out"

bin=$(cargo test --test screenshots --no-run 2>&1 | sed -n 's/.*(\(target\/[^)]*screenshots-[^)]*\)).*/\1/p' | tail -1)
[ -n "$bin" ] || { echo "could not build the screenshots test" >&2; exit 1; }

PAIRDECK_SHOTS_DIR="$PWD/$html" "$bin" --ignored --nocapture --test-threads=1 |
    sed -n 's/.*SHOT //p' |
    while read -r name w h; do
        rm -f "$out/$name.png"
        for _ in 1 2 3; do
            "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=2 \
            --window-size="$w,$h" --screenshot="$PWD/$out/$name.png" "file://$PWD/$html/$name.html" 2>/dev/null || true
            [ -s "$out/$name.png" ] && break
        done
        echo "$out/$name.png"
    done
