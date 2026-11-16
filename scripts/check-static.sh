#!/bin/sh
# Fail unless BINARY links only what every system has:
#   Linux: nothing at all (a static musl executable)
#   macOS: only /usr/lib and /System/Library (libSystem, frameworks)
set -eu
bin=${1:?usage: check-static.sh BINARY}
case "$(file -b "$bin")" in
*Mach-O*)
    bad=$(otool -L "$bin" | tail -n +2 | awk '{print $1}' | grep -v -e '^/usr/lib/' -e '^/System/Library/' || true)
    ;;
*ELF*)
    if file -b "$bin" | grep -q -e 'statically linked' -e 'static-pie linked'; then
        bad=""
    else
        bad=$(file -b "$bin")
    fi
    ;;
*) echo "check-static: unknown binary format: $(file -b "$bin")" >&2; exit 2 ;;
esac
if [ -n "$bad" ]; then
    echo "check-static: $bin links non-system libraries:" >&2
    echo "$bad" >&2
    exit 1
fi
echo "check-static: $bin is self-contained"
