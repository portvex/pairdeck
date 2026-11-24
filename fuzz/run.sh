#!/bin/sh
# Fuzzing helper (libFuzzer via cargo-fuzz, needs a nightly toolchain from rustup).
#   ./fuzz/run.sh                 replay regressions, then fuzz every target for 60s
#   ./fuzz/run.sh archive 600     fuzz one target for 600s
#   ./fuzz/run.sh replay          only replay the saved regression inputs
set -e
cd "$(dirname "$0")"
# rustup's proxies must win over a Homebrew/system rustc for `+nightly` to work.
export PATH="$HOME/.cargo/bin:$PATH"
TARGETS=$(cargo +nightly fuzz list)
# cargo-fuzz builds for the triple it was itself built for unless told
# otherwise; a static (musl) cargo-fuzz, as CI installs, would pick musl,
# where sanitizers cannot run. Always build for nightly rustc's host.
HOST=$(rustc +nightly -vV | sed -n 's/^host: //p')
[ -n "$HOST" ] || { echo "fuzz: cannot determine the nightly host triple" >&2; exit 1; }

replay() {
    for t in $TARGETS; do
        [ -d "regressions/$t" ] || continue
        echo "== replay $t"
        cargo +nightly fuzz run --target "$HOST" "$t" "regressions/$t" -- -runs=0
    done
}

fuzz() {
    t=$1
    secs=${2:-60}
    mkdir -p "corpus/$t"
    [ -d "seeds/$t" ] && cp -n seeds/"$t"/* "corpus/$t/" 2>/dev/null || true
    echo "== fuzz $t for ${secs}s"
    cargo +nightly fuzz run --target "$HOST" "$t" "corpus/$t" -- -max_total_time="$secs" -rss_limit_mb=2048 -timeout=10
}

case "$1" in
    replay) replay ;;
    "") replay; for t in $TARGETS; do fuzz "$t" 60; done ;;
    *) fuzz "$1" "$2" ;;
esac
