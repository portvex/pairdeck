#!/bin/sh
# Render the Homebrew formula for a release:
#   scripts/homebrew-formula.sh VERSION SHA256SUMS > pairdeck.rb
# VERSION is without the leading "v"; SHA256SUMS is the release's checksum file.
set -eu
version=${1:?usage: homebrew-formula.sh VERSION SHA256SUMS}
sums=${2:?usage: homebrew-formula.sh VERSION SHA256SUMS}
cd "$(dirname "$0")/.."

sha() {
    s=$(awk -v f="pairdeck-$version-$1.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$sums")
    [ -n "$s" ] || { echo "homebrew-formula: no checksum for pairdeck-$version-$1.tar.gz" >&2; exit 1; }
    echo "$s"
}

sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHA256_UNIVERSAL_APPLE_DARWIN@/$(sha universal-apple-darwin)/" \
    -e "s/@SHA256_AARCH64_UNKNOWN_LINUX_MUSL@/$(sha aarch64-unknown-linux-musl)/" \
    -e "s/@SHA256_X86_64_UNKNOWN_LINUX_MUSL@/$(sha x86_64-unknown-linux-musl)/" \
    packaging/homebrew/pairdeck.rb.in
