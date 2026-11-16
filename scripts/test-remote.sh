#!/bin/sh
# Integration tests for the remote backends against real, throwaway servers.
#
#   scripts/test-remote.sh            all of: sftp s3 webdav
#   scripts/test-remote.sh s3 webdav  only these
#
# Servers, in order of preference (nothing touches your real ~/.ssh or ~/.aws):
#   sftp    a non-root sshd started by the test (OpenSSH needed), else Docker
#           (linuxserver/openssh-server)
#   s3      `rclone serve s3` started by the test, else Docker (bitnami/minio)
#   webdav  `rclone serve webdav` started by the test, else Docker (rclone/rclone)
# Set PAIRDECK_FORCE_DOCKER=1 to use Docker even when local tools exist.
# Requirements: cargo, and either OpenSSH + rclone, or a running Docker daemon.
set -eu
cd "$(dirname "$0")/.."

CONTAINERS=""
cleanup() {
    for c in $CONTAINERS; do docker rm -f "$c" >/dev/null 2>&1 || true; done
    [ -n "${KEYDIR:-}" ] && rm -rf "$KEYDIR"
}
trap cleanup EXIT INT TERM

have() { command -v "$1" >/dev/null 2>&1; }
use_docker() { [ "${PAIRDECK_FORCE_DOCKER:-}" = 1 ]; }
docker_ok() { have docker && docker info >/dev/null 2>&1; }

free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'
}

wait_port() {
    i=0
    while ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && ! nc -z 127.0.0.1 "$1" 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 120 ] && { echo "server on port $1 did not start" >&2; exit 1; }
        sleep 0.5
    done
}

run() { cargo test --test "$1" -- --ignored --nocapture; }

sftp() {
    if ! use_docker && { have sshd || [ -x /opt/homebrew/sbin/sshd ] || [ -x /usr/sbin/sshd ]; }; then
        run sftp_integration
        return
    fi
    docker_ok || { echo "sftp: no sshd and no Docker daemon" >&2; exit 1; }
    KEYDIR=$(mktemp -d)
    ssh-keygen -q -t ed25519 -N "" -f "$KEYDIR/key"
    port=$(free_port)
    c=pairdeck-test-sftp-$$
    docker run -d --rm --name "$c" -p "127.0.0.1:$port:2222" \
        -e PUBLIC_KEY="$(cat "$KEYDIR/key.pub")" -e USER_NAME=pairdeck \
        lscr.io/linuxserver/openssh-server:latest >/dev/null
    CONTAINERS="$CONTAINERS $c"
    wait_port "$port"
    sleep 3 # key setup inside the container
    PAIRDECK_TEST_SFTP_HOST=127.0.0.1 PAIRDECK_TEST_SFTP_PORT=$port PAIRDECK_TEST_SFTP_USER=pairdeck \
        PAIRDECK_TEST_SFTP_KEY="$KEYDIR/key" PAIRDECK_TEST_SFTP_DIR=/config run sftp_integration
}

s3() {
    if ! use_docker && have rclone; then
        run s3_integration
        return
    fi
    docker_ok || { echo "s3: no rclone and no Docker daemon" >&2; exit 1; }
    port=$(free_port)
    c=pairdeck-test-s3-$$
    docker run -d --rm --name "$c" -p "127.0.0.1:$port:9000" \
        -e MINIO_ROOT_USER=AKTEST -e MINIO_ROOT_PASSWORD=SKTEST12345 -e MINIO_DEFAULT_BUCKETS=testbucket \
        bitnami/minio:latest >/dev/null
    CONTAINERS="$CONTAINERS $c"
    wait_port "$port"
    sleep 3
    PAIRDECK_TEST_S3_ENDPOINT="http://127.0.0.1:$port" PAIRDECK_TEST_S3_BUCKET=testbucket \
        PAIRDECK_TEST_S3_KEY=AKTEST PAIRDECK_TEST_S3_SECRET=SKTEST12345 run s3_integration
}

webdav() {
    if ! use_docker && have rclone; then
        run webdav_integration
        return
    fi
    docker_ok || { echo "webdav: no rclone and no Docker daemon" >&2; exit 1; }
    port=$(free_port)
    c=pairdeck-test-dav-$$
    docker run -d --rm --name "$c" -p "127.0.0.1:$port:8080" rclone/rclone:latest \
        serve webdav /data --addr :8080 --user davuser --pass davpass >/dev/null
    CONTAINERS="$CONTAINERS $c"
    wait_port "$port"
    PAIRDECK_TEST_DAV_URL="dav://127.0.0.1:$port/" PAIRDECK_TEST_DAV_USER=davuser PAIRDECK_TEST_DAV_PASS=davpass \
        run webdav_integration
}

[ $# -eq 0 ] && set -- sftp s3 webdav
for t in "$@"; do
    case "$t" in
        sftp) sftp ;;
        s3) s3 ;;
        webdav | dav) webdav ;;
        *) echo "unknown backend: $t (sftp, s3, webdav)" >&2; exit 2 ;;
    esac
done
