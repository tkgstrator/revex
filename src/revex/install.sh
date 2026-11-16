#!/bin/sh
set -eu

fail() {
    printf 'revex feature: %s\n' "$*" >&2
    exit 1
}

[ "$(uname -s)" = Linux ] || fail 'only Linux containers are supported'
case $(uname -m) in
    x86_64|aarch64|arm64) ;;
    *) fail 'only x86_64 and arm64 containers are supported' ;;
esac

# The published Linux v0.1.0 binaries require GLIBC_2.39. Fail before any
# download so an unsupported image does not end up with an unusable binary.
libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) ||
    fail 'glibc 2.39 or newer is required (Ubuntu 24.04 or newer)'
case $libc in
    'glibc '*) libc_version=${libc#glibc } ;;
    *) fail 'glibc 2.39 or newer is required (musl is unsupported)' ;;
esac
libc_major=${libc_version%%.*}
libc_minor=${libc_version#*.}
libc_minor=${libc_minor%%.*}
case "$libc_major:$libc_minor" in
    *[!0-9:]*|:*|*:) fail 'cannot determine glibc version' ;;
esac
if [ "$libc_major" -lt 2 ] ||
    { [ "$libc_major" -eq 2 ] && [ "$libc_minor" -lt 39 ]; }; then
    fail 'glibc 2.39 or newer is required (Ubuntu 24.04 or newer)'
fi

version=${VERSION:-latest}
case $version in
    latest) release_version= ;;
    v[0-9]*)
        case $version in
            *[!A-Za-z0-9._-]*) fail 'version contains invalid characters' ;;
        esac
        release_version=$version
        ;;
    *) fail 'version must be latest or a v-prefixed release tag' ;;
esac

missing_tools=
for command_name in curl tar mktemp sha256sum; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        missing_tools=1
    fi
done
if [ -n "$missing_tools" ] && command -v apt-get >/dev/null 2>&1; then
    DEBIAN_FRONTEND=noninteractive apt-get update -qq || fail 'apt-get update failed'
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        ca-certificates curl tar coreutils || fail 'cannot install prerequisites'
fi
for command_name in curl tar mktemp sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

installer=$(mktemp "${TMPDIR:-/tmp}/revex-feature.XXXXXX") || fail 'cannot create temporary file'
trap 'rm -f "$installer"' 0
trap 'exit 1' 1 2 3 15

# Pin installer behavior to the first supported release. The installer itself
# verifies the selected release archive against its published SHA-256 file.
installer_url=https://raw.githubusercontent.com/tkgstrator/revex/v0.1.0/install.sh
curl -fsSL -o "$installer" "$installer_url" || fail 'cannot download installer'
REVEX_INSTALL_DIR=/usr/local/bin REVEX_VERSION=$release_version sh "$installer"
