#!/bin/sh
set -eu

repo_dir=$(cd "$(dirname "$0")/.." && pwd)
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/revex-install-test.XXXXXX")
trap 'rm -rf "$test_dir"' 0
trap 'exit 1' 1 2 3 15

fail() {
    printf 'installer smoke test: %s\n' "$*" >&2
    exit 1
}

assets_dir=$test_dir/assets
source_dir=$test_dir/source
stub_dir=$test_dir/stubs
curl_log=$test_dir/curl.log
mkdir -p "$assets_dir" "$source_dir" "$stub_dir"
: > "$curl_log"

for target in aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    printf '#!/bin/sh\necho %s\n' "$target" > "$source_dir/revex"
    chmod 755 "$source_dir/revex"
    archive=revex-$target.tar.gz
    tar -C "$source_dir" -czf "$assets_dir/$archive" revex
    if command -v shasum >/dev/null 2>&1; then
        (cd "$assets_dir" && shasum -a 256 "$archive" > "$archive.sha256")
    else
        (cd "$assets_dir" && sha256sum "$archive" > "$archive.sha256")
    fi
done

cat > "$stub_dir/uname" <<'SH'
#!/bin/sh
case $1 in
    -s) printf '%s\n' "$REVEX_TEST_OS" ;;
    -m) printf '%s\n' "$REVEX_TEST_ARCH" ;;
    *) exit 2 ;;
esac
SH
cat > "$stub_dir/curl" <<'SH'
#!/bin/sh
set -eu
destination=
url=
while [ "$#" -gt 0 ]; do
    case $1 in
        -fsSL) shift ;;
        -o) destination=$2; shift 2 ;;
        -*) exit 2 ;;
        *) url=$1; shift ;;
    esac
done
[ -n "$destination" ] && [ -n "$url" ] || exit 2
printf '%s\n' "$url" >> "$REVEX_TEST_CURL_LOG"
asset=${url##*/}
[ -f "$REVEX_TEST_ASSETS/$asset" ] || exit 22
cp "$REVEX_TEST_ASSETS/$asset" "$destination"
SH
chmod 755 "$stub_dir/uname" "$stub_dir/curl"

run_install() {
    REVEX_TEST_OS=$1 REVEX_TEST_ARCH=$2 REVEX_INSTALL_DIR=$3 REVEX_VERSION=$4 \
        REVEX_TEST_ASSETS=$assets_dir REVEX_TEST_CURL_LOG=$curl_log \
        PATH="$stub_dir:$PATH" sh "$repo_dir/install.sh"
}

for platform in Darwin:arm64:aarch64-apple-darwin \
                Linux:x86_64:x86_64-unknown-linux-gnu \
                Linux:aarch64:aarch64-unknown-linux-gnu; do
    os=${platform%%:*}
    rest=${platform#*:}
    arch=${rest%%:*}
    target=${rest#*:}
    destination=$test_dir/install-$target
    run_install "$os" "$arch" "$destination" "" > "$test_dir/output"
    [ -x "$destination/revex" ] || fail "$target was not installed as executable"
    [ "$("$destination/revex")" = "$target" ] || fail "$target archive was not selected"
done
grep -F '/releases/latest/download/revex-aarch64-apple-darwin.tar.gz' "$curl_log" >/dev/null ||
    fail 'latest release URL was not used'

pinned_dir=$test_dir/pinned
run_install Linux x86_64 "$pinned_dir" v0.1.0 > "$test_dir/output"
grep -F '/releases/download/v0.1.0/revex-x86_64-unknown-linux-gnu.tar.gz' "$curl_log" >/dev/null ||
    fail 'pinned release URL was not used'

archive=revex-x86_64-unknown-linux-gnu.tar.gz
printf '%064d  %s\n' 0 "$archive" > "$assets_dir/$archive.sha256"
if run_install Linux x86_64 "$pinned_dir" "" > "$test_dir/output" 2> "$test_dir/error"; then
    fail 'bad checksum was accepted'
fi
[ "$("$pinned_dir/revex")" = x86_64-unknown-linux-gnu ] ||
    fail 'bad checksum replaced the existing binary'

darwin_archive=$assets_dir/revex-aarch64-apple-darwin.tar.gz
mv "$darwin_archive" "$test_dir/held-archive"
if run_install Darwin arm64 "$test_dir/install-aarch64-apple-darwin" "" > "$test_dir/output" 2> "$test_dir/error"; then
    fail 'missing release asset was accepted'
fi
[ "$("$test_dir/install-aarch64-apple-darwin/revex")" = aarch64-apple-darwin ] ||
    fail 'missing release asset replaced the existing binary'
mv "$test_dir/held-archive" "$darwin_archive"

before=$(wc -l < "$curl_log")
if run_install Darwin x86_64 "$test_dir/unsupported" "" > "$test_dir/output" 2> "$test_dir/error"; then
    fail 'unsupported platform was accepted'
fi
after=$(wc -l < "$curl_log")
[ "$before" -eq "$after" ] || fail 'unsupported platform attempted a download'
if run_install Linux x86_64 "$test_dir/invalid-version" '../bad' > "$test_dir/output" 2> "$test_dir/error"; then
    fail 'unsafe release tag was accepted'
fi
after=$(wc -l < "$curl_log")
[ "$before" -eq "$after" ] || fail 'unsafe release tag attempted a download'

printf 'occupied\n' > "$test_dir/not-a-directory"
if run_install Darwin arm64 "$test_dir/not-a-directory" "" > "$test_dir/output" 2> "$test_dir/error"; then
    fail 'unwritable destination was accepted'
fi
[ "$(cat "$test_dir/not-a-directory")" = occupied ] ||
    fail 'destination failure modified the existing file'

printf 'installer smoke tests passed\n'
