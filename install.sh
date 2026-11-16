#!/bin/sh
set -eu

die() {
    printf 'revex installer: %s\n' "$*" >&2
    exit 1
}

for command_name in uname curl tar mktemp mkdir cp chmod mv rm; do
    command -v "$command_name" >/dev/null 2>&1 ||
        die "$command_name is required"
done
if command -v shasum >/dev/null 2>&1; then
    hash_command=shasum
elif command -v sha256sum >/dev/null 2>&1; then
    hash_command=sha256sum
else
    die 'shasum or sha256sum is required to verify the release'
fi

system=$(uname -s)
architecture=$(uname -m)
case "$system:$architecture" in
    Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
    *) die "unsupported platform: $system $architecture" ;;
esac

version=${REVEX_VERSION:-}
if [ -n "$version" ]; then
    case $version in
        v[0-9]*) ;;
        *) die 'REVEX_VERSION must be a v-prefixed release tag' ;;
    esac
    case $version in
        *[!A-Za-z0-9._-]*) die 'REVEX_VERSION contains invalid characters' ;;
    esac
    release_url=https://github.com/tkgstrator/revex/releases/download/$version
else
    release_url=https://github.com/tkgstrator/revex/releases/latest/download
fi

if [ -n "${REVEX_INSTALL_DIR:-}" ]; then
    install_dir=$REVEX_INSTALL_DIR
else
    [ -n "${HOME:-}" ] || die 'HOME or REVEX_INSTALL_DIR is required'
    install_dir=$HOME/.local/bin
fi
case $install_dir in
    /*) ;;
    *) die 'REVEX_INSTALL_DIR must be an absolute path' ;;
esac
mkdir -p "$install_dir" || die "cannot create $install_dir"
[ -d "$install_dir" ] && [ -w "$install_dir" ] ||
    die "installation directory is not writable: $install_dir"
[ ! -d "$install_dir/revex" ] ||
    die "installation destination is a directory: $install_dir/revex"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/revex-install.XXXXXX") ||
    die 'cannot create a temporary directory'
install_tmp=
cleanup() {
    if [ -n "$install_tmp" ]; then
        rm -f "$install_tmp"
    fi
    rm -rf "$work_dir"
}
trap cleanup 0
trap 'exit 1' 1 2 3 15

archive=revex-$target.tar.gz
archive_path=$work_dir/$archive
checksum_path=$archive_path.sha256
curl -fsSL -o "$archive_path" "$release_url/$archive" ||
    die "cannot download $archive (a published release is required)"
curl -fsSL -o "$checksum_path" "$release_url/$archive.sha256" ||
    die "cannot download checksum for $archive"

IFS=' ' read -r expected_hash checksum_name < "$checksum_path" ||
    die 'invalid checksum file'
[ "${#expected_hash}" -eq 64 ] || die 'invalid SHA-256 digest length'
case $expected_hash in
    *[!0-9a-fA-F]*) die 'invalid SHA-256 digest' ;;
esac
[ "$checksum_name" = "$archive" ] ||
    die 'checksum file names a different archive'
if [ "$hash_command" = shasum ]; then
    actual_line=$(shasum -a 256 "$archive_path") || die 'cannot hash archive'
else
    actual_line=$(sha256sum "$archive_path") || die 'cannot hash archive'
fi
actual_hash=${actual_line%% *}
[ "$actual_hash" = "$expected_hash" ] || die 'SHA-256 checksum mismatch'

entries=$(tar -tzf "$archive_path") || die 'cannot inspect release archive'
[ "$entries" = revex ] || die 'release archive must contain only revex'
mkdir "$work_dir/unpack" || die 'cannot stage release binary'
tar -xzf "$archive_path" -C "$work_dir/unpack" revex ||
    die 'cannot extract revex'
[ -f "$work_dir/unpack/revex" ] && [ ! -L "$work_dir/unpack/revex" ] ||
    die 'release binary is not a regular file'

install_tmp=$(mktemp "$install_dir/.revex.XXXXXX") ||
    die "cannot stage binary in $install_dir"
cp "$work_dir/unpack/revex" "$install_tmp" ||
    die "cannot copy binary to $install_dir"
chmod 755 "$install_tmp" || die 'cannot make binary executable'
mv -f "$install_tmp" "$install_dir/revex" ||
    die "cannot install binary in $install_dir"
install_tmp=

printf 'Installed revex to %s/revex\n' "$install_dir"
case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) printf 'Add %s to PATH to run revex from any directory.\n' "$install_dir" ;;
esac
