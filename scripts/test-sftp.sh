#!/bin/sh
# SFTP integration test against a throwaway sshd (see scripts/test-remote.sh).
exec "$(dirname "$0")/test-remote.sh" sftp
