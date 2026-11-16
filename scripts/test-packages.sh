#!/usr/bin/env bash
# Install the Linux packages in DIST with each distribution's own package manager and
# run pdeck, so a package a distribution would refuse never reaches a release.
#   scripts/test-packages.sh DIST VERSION ARCH     (ARCH: x86_64 or aarch64)
# Needs only Docker. Every test runs in a container of ARCH, so x86_64 packages can be
# tested on an ARM machine (with amd64 emulation) and vice versa.
set -euo pipefail
dist=$(cd "${1:?usage: test-packages.sh DIST VERSION ARCH}" && pwd)
export VERSION=${2:?usage: test-packages.sh DIST VERSION ARCH}
arch=${3:?usage: test-packages.sh DIST VERSION ARCH}
case "$arch" in
  x86_64) platform=linux/amd64 deb=amd64 ;;
  aarch64) platform=linux/arm64 deb=arm64 ;;
  *) echo "test-packages: ARCH must be x86_64 or aarch64, not $arch" >&2; exit 2 ;;
esac

check=$(mktemp -d)
cat > "$check/check.sh" <<'EOF'
set -e
pdeck --version | grep -q "$VERSION"
echo "ok: $(pdeck --version)"
EOF
chmod -R a+rX "$check"
run() { docker run --rm --platform "$platform" -e VERSION -v "$dist:/dist:ro" -v "$check:/t:ro" "$@"; }

echo "== Debian (.deb)"
# Container images drop man pages on install, so the package listing is checked instead.
run debian:stable-slim sh -c "dpkg -i /dist/*_$deb.deb && sh /t/check.sh &&
  dpkg -c /dist/*_$deb.deb | grep usr/share/man/man1/pdeck.1.gz >/dev/null"

echo "== Fedora (.rpm)"
# dnf, as users install it: plain `rpm -i` (rpm 6) refuses unsigned packages.
run fedora:latest sh -c "dnf install -y -q /dist/*.$arch.rpm && sh /t/check.sh"

echo "== Alpine (.apk)"
run alpine:latest sh -c "apk add --no-cache --allow-untrusted /dist/*_$arch.apk && sh /t/check.sh"

if [ "$arch" = x86_64 ]; then
    # Arch publishes x86_64 images only.
    echo "== Arch (.pkg.tar.zst)"
    run archlinux:latest sh -c "pacman -U --noconfirm /dist/*-$arch.pkg.tar.zst && sh /t/check.sh"
fi

echo "== standalone executable (busybox, no libc)"
run busybox:latest "/dist/pdeck-$VERSION-$arch-linux" --version
