# Dev Container Feature

Add Revex to a Linux development container with this `devcontainer.json` entry:

```json
{
  "features": {
    "ghcr.io/tkgstrator/revex:1": {
      "version": "latest"
    }
  }
}
```

For a repeatable build, set `"version": "v0.1.0"` instead. The `:1` suffix
selects the Feature's major version; the `version` option selects the Revex
release. The Feature installs `/usr/local/bin/revex` for Linux x86_64 and arm64
containers with **glibc 2.39 or newer**. The published v0.1.0 binaries require
that minimum; Ubuntu 24.04 is tested, while Ubuntu 22.04, Debian 12, Alpine,
and other musl-based images are unsupported. The Feature checks glibc before
downloading and verifies the release archive against its published SHA-256 file.
On Debian or Ubuntu, missing build prerequisites are installed with `apt-get`;
other compatible images must provide
`curl`, `tar`, `mktemp`, and `sha256sum`.

The GHCR reference becomes usable after the Feature workflow publishes its
first package from `master` and the package is made public in GitHub's package
settings. Revex v0.1.0 release assets already exist; publishing the Feature
does not require a new Revex application release.

Maintainers can run the wrapper fixture test with `sh test/revex/smoke.sh` and
the official container test with:

```sh
devcontainer features test -f revex --base-image ubuntu:24.04 .
```

The publication workflow runs both tests on Linux x86_64 and arm64 before
publishing to `ghcr.io/tkgstrator/revex:1`. Bump
`src/revex/devcontainer-feature.json`'s semantic `version` when changing the
Feature; the registry will not republish an unchanged Feature version.
