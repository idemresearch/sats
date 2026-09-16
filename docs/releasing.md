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

## Local release verification

The v0.0.1 package version, CLI `--version`, MCP implementation version, and
playground version all derive from workspace metadata. Default binaries include
MCP and exclude Alkanes execution; development feature builds are not release
artifacts. Run the complete [development gate](development.md), regenerate the
playground after core changes, and exercise the local request/recovery fixtures
before publishing.

Local tests use disposable wallets and mock/localhost providers. They do not
verify live provider dialects, a real Claude installation, or the four packaged
release archives. Archive contents, per-target checksums, `SHA256SUMS`, and a
tagged installation must be checked after the workflow produces artifacts.

## Publish a release

1. Update `workspace.package.version` in `Cargo.toml` and refresh `Cargo.lock`
   if necessary.
2. Merge the version change to `main` after CI passes.
3. Create and push an annotated `vMAJOR.MINOR.PATCH` tag:

   ```sh
   git switch main
   git pull --ff-only
   git tag -a v0.0.1 -m "sats v0.0.1"
   git push origin v0.0.1
   ```

The workflow rejects malformed tags and tags whose version does not match
`Cargo.toml`. It runs installer tests and the Rust test suite before building
the four archives.

Re-running a release workflow replaces existing assets, allowing a partially
failed upload to recover without creating another release. A manual workflow
dispatch builds each archive and its individual checksum without publishing a
GitHub release. Download and verify those artifacts before tagging; aggregate
checksum verification runs in the publishing job.

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
