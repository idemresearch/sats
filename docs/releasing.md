# Releasing sats

Releases are produced by `.github/workflows/release.yml`. A stable semantic
version tag builds native binaries on matching GitHub-hosted runners for:

- Linux x86_64 and ARM64;
- macOS x86_64 and Apple silicon.

The CLI links a bundled SQLite build, so release binaries do not depend on a
system SQLite installation.

Each archive is published with an individual SHA-256 checksum plus an
aggregate `SHA256SUMS` file. `setup.sh` consumes the stable per-target asset
names from either the latest release or a tag selected with `SATS_VERSION`.

## Publish a release

1. Update `workspace.package.version` in `Cargo.toml` and refresh `Cargo.lock`
   if necessary.
2. Merge the version change to `main` after CI passes.
3. Create and push an annotated `vMAJOR.MINOR.PATCH` tag:

   ```sh
   git switch main
   git pull --ff-only
   git tag -a v0.1.0 -m "sats v0.1.0"
   git push origin v0.1.0
   ```

The workflow rejects malformed tags and tags whose version does not match
`Cargo.toml`. It runs installer tests and the Rust test suite before building
the four archives.

Re-running a release workflow replaces existing assets, allowing a partially
failed upload to recover without creating another release. A manual workflow
dispatch builds and verifies every archive without publishing a GitHub
release.

## Verify assets

Before announcing a release:

1. confirm all four target jobs succeeded;
2. confirm each archive and its `.sha256` file are present;
3. verify `SHA256SUMS` contains every archive;
4. run `setup.sh` against the tagged release on at least one supported target;
5. confirm the installed binary reports the tagged version.

The installer source served from any project domain must remain byte-for-byte
identical to the repository's `setup.sh`, leaving one implementation to audit
and test. The copy served at `https://sats.sh/setup.sh` is the committed
`website/public/setup.sh`; `scripts/test-setup.sh` fails when it drifts from
`setup.sh`, so update both files together.
