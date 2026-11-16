#!/bin/sh
set -eu

repo_dir=$(cd "$(dirname "$0")/../.." && pwd)
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/revex-feature-test.XXXXXX")
trap 'rm -rf "$test_dir"' 0
trap 'exit 1' 1 2 3 15

fail() {
    printf 'revex feature test: %s\n' "$*" >&2
    exit 1
}

mkdir "$test_dir/stubs"
cat > "$test_dir/stubs/curl" <<'SH'
#!/bin/sh
set -eu
[ "$1" = -fsSL ] && [ "$2" = -o ] || exit 2
destination=$3
url=$4
printf '%s\n' "$url" > "$FEATURE_TEST_URL"
cp "$FEATURE_TEST_INSTALLER" "$destination"
SH
cat > "$test_dir/stubs/uname" <<'SH'
#!/bin/sh
case $1 in
    -s) printf '%s\n' "$FEATURE_TEST_OS" ;;
    -m) printf '%s\n' "$FEATURE_TEST_ARCH" ;;
    *) exit 2 ;;
esac
SH
cat > "$test_dir/stubs/getconf" <<'SH'
#!/bin/sh
[ "$1" = GNU_LIBC_VERSION ] || exit 2
printf '%s\n' "$FEATURE_TEST_GLIBC"
SH
chmod 755 "$test_dir/stubs/curl" "$test_dir/stubs/uname" "$test_dir/stubs/getconf"
cat > "$test_dir/installer" <<'SH'
#!/bin/sh
printf '%s\n%s\n' "$REVEX_INSTALL_DIR" "$REVEX_VERSION" > "$FEATURE_TEST_RESULT"
SH

run_feature() {
    FEATURE_TEST_OS=$1 FEATURE_TEST_ARCH=$2 VERSION=$3 \
        FEATURE_TEST_GLIBC=${4:-glibc 2.39} \
        FEATURE_TEST_URL="$test_dir/url" \
        FEATURE_TEST_INSTALLER="$test_dir/installer" \
        FEATURE_TEST_RESULT="$test_dir/result" \
        PATH="$test_dir/stubs:$PATH" sh "$repo_dir/src/revex/install.sh"
}

run_feature Linux x86_64 latest
[ "$(sed -n '1p' "$test_dir/result")" = /usr/local/bin ] || fail 'wrong install path'
[ -z "$(sed -n '2p' "$test_dir/result")" ] || fail 'latest was not selected'
grep -Fx 'https://raw.githubusercontent.com/tkgstrator/revex/v0.1.0/install.sh' "$test_dir/url" >/dev/null ||
    fail 'wrong installer source'

run_feature Linux aarch64 v0.1.0
[ "$(sed -n '2p' "$test_dir/result")" = v0.1.0 ] || fail 'pinned version was not forwarded'

rm "$test_dir/result"
rm "$test_dir/url"
if run_feature Linux x86_64 latest 'glibc 2.38' > "$test_dir/out" 2>&1; then
    fail 'glibc 2.38 was accepted'
fi
grep -F 'glibc 2.39 or newer is required' "$test_dir/out" >/dev/null ||
    fail 'old glibc failure did not explain minimum version'
[ ! -e "$test_dir/result" ] && [ ! -e "$test_dir/url" ] ||
    fail 'old glibc downloaded or installed Revex'

if run_feature Darwin arm64 latest > "$test_dir/out" 2>&1; then
    fail 'macOS was accepted'
fi
[ ! -e "$test_dir/result" ] || fail 'unsupported OS downloaded installer'
if run_feature Linux x86_64 'v0.1.0;false' > "$test_dir/out" 2>&1; then
    fail 'malformed version was accepted'
fi
[ ! -e "$test_dir/result" ] || fail 'malformed version downloaded installer'

printf 'revex feature fixture tests passed\n'
