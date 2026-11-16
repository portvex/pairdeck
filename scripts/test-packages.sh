#!/usr/bin/env bash
# Install the Linux packages in DIST with each distribution's own package manager and
# run pdeck, so a package a distribution would refuse never reaches a release.
#   scripts/test-packages.sh DIST VERSION ARCH     (ARCH: x86_64 or aarch64)
# Needs Docker and sudo (the .deb is installed on the host, a Debian/Ubuntu runner).
set -euo pipefail
dist=$(cd "${1:?usage: test-packages.sh DIST VERSION ARCH}" && pwd)
export VERSION=${2:?usage: test-packages.sh DIST VERSION ARCH}
arch=${3:?usage: test-packages.sh DIST VERSION ARCH}

check=$(mktemp -d)
cat > "$check/check.sh" <<'EOF'
set -e
pdeck --version | grep -q "$VERSION"
echo "ok: $(pdeck --version)"
EOF
run() { docker run --rm -e VERSION -v "$dist:/dist:ro" -v "$check:/t:ro" "$@"; }

echo "== Debian (.deb, on this host)"
# Container images drop man pages, so the man page is checked here only.
sudo dpkg -i "$dist"/*.deb
sh "$check/check.sh"
man -w pdeck
sudo dpkg -r pairdeck

echo "== Fedora (.rpm)"
run fedora:latest sh -c 'rpm -i /dist/*.rpm && sh /t/check.sh'

echo "== Alpine (.apk)"
run alpine:latest sh -c 'apk add --no-cache --allow-untrusted /dist/*.apk && sh /t/check.sh'

if [ "$arch" = x86_64 ]; then
    # Arch publishes x86_64 images only.
    echo "== Arch (.pkg.tar.zst)"
    run archlinux:latest sh -c 'pacman -U --noconfirm /dist/*.pkg.tar.zst && sh /t/check.sh'
fi

echo "== standalone executable (busybox, no libc)"
run busybox:latest "/dist/pdeck-$VERSION-$arch-linux" --version
