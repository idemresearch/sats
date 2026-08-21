# Releasing sats

Releases are produced by `.github/workflows/release.yml`. A stable semantic
version tag builds native binaries on matching GitHub-hosted runners for:

- Linux x86_64 and ARM64
- macOS x86_64 and Apple silicon

The CLI links a bundled SQLite build so release binaries do not depend on a
system SQLite installation.

Each archive is published with an individual SHA-256 checksum, plus an
aggregate `SHA256SUMS` file. `setup.sh` consumes the stable per-target asset
names from either the latest release or a tag selected with `SATS_VERSION`.

## Publish a release

1. Update `workspace.package.version` in `Cargo.toml` and refresh
   `Cargo.lock` if necessary.
2. Merge the change to `main` after CI passes.
3. Create and push an annotated `vMAJOR.MINOR.PATCH` tag:

   ```sh
   git switch main
   git pull --ff-only
   git tag -a v0.1.0 -m "sats v0.1.0"
   git push origin v0.1.0
   ```

The workflow rejects malformed tags and tags whose version does not match
`Cargo.toml`. It runs the installer tests and Rust test suite before building
the four archives. Re-running a release workflow replaces existing assets so
a partially failed upload can be recovered without creating another release.
A manual workflow dispatch builds and verifies every archive without publishing
a GitHub release.

When `sats.sh/install` is ready, serve the repository's `setup.sh` verbatim so
there remains one installer implementation to audit and test.
