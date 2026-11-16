# Revex one-line installer

## Intent

Install the released Revex binary with one terminal command, without requiring Rust or administrator access:

```sh
curl -fsSL https://raw.githubusercontent.com/tkgstrator/revex/master/install.sh | sh
```

The repository's default branch is `master`. The installer lives there; the binary comes from a tagged GitHub release. No release or tag exists yet, so the command must not be advertised as working until the first `v*` release is published.

## Installer behavior

- Write a POSIX `sh` script at repository root, `install.sh`. Map `uname` to the existing release targets: macOS arm64 (`aarch64-apple-darwin`), Linux x86_64 (`x86_64-unknown-linux-gnu`), and Linux arm64 (`aarch64-unknown-linux-gnu`). Reject other platforms with a clear error.
- Download the matching `revex-<target>.tar.gz` and `.sha256` from the latest GitHub release. Allow an optional version setting to select a specific tag. Require `curl`, `tar`, and `shasum -a 256` or `sha256sum`.
- Verify the downloaded archive against its published SHA-256 before extracting `revex`. Stage the executable in a temporary directory, then install it to `${REVEX_INSTALL_DIR:-$HOME/.local/bin}` with executable permissions. Replace an existing binary only after download, verification, and extraction succeed. Do not invoke `sudo` or change shell configuration; print a PATH hint if needed.
- Fail without modifying an existing installation when a release, target asset, checksum utility, checksum match, or writable destination is unavailable. Clean temporary files on success and failure. Do not silently fall back to a source build.

## Alternatives and tradeoffs

- `cargo install --git` works before the first release but requires a Rust toolchain and a source build. Keep it as the documented alternative, not an automatic fallback.
- A Homebrew formula would provide package-manager updates but needs formula/tap maintenance and does not cover the existing Linux binaries by itself.
- A direct release URL avoids a hosted script but needs separate commands for each platform and checksum verification.

## Files and verification

- Add `install.sh`; update `README.md` with the one-line command, supported platforms, installation location, and the existing Cargo option.
- Add `tests/install_smoke.sh` with local fixture archives and checksums. Check supported-target selection, successful installation, checksum failure without replacement, unsupported platform, and destination failure. Run it from `.github/workflows/integration.yaml`.
- The current `.github/workflows/release.yaml` already publishes archives and SHA-256 files under the required names, so it needs no change unless that asset contract changes.
- Publish and verify the first `v*` release and its three asset pairs before presenting the binary one-liner as ready for users.
