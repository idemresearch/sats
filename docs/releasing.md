# Releasing sats

Tag a version, let CI build and publish the archives, then verify them
before announcing.

`.github/workflows/release.yml` builds native binaries for Linux and macOS,
each on x86_64 and ARM64. Binaries link a bundled SQLite. Each archive ships
with its own SHA-256 file, plus an aggregate `SHA256SUMS`. `setup.sh`
installs from the latest release, or from the tag named in `SATS_VERSION`.

## Before tagging

The package version, `sats --version`, the MCP implementation version, and
the playground version all come from `workspace.package.version` in
`Cargo.toml`. Default binaries include MCP and exclude every Alkanes command.

1. Run the full [verification gate](development.md#verification-gate).
2. Regenerate the playground if `sats-core` or `sats-playground` changed.
3. Exercise the request and recovery flows with a local build.
4. Optionally run the workflow manually. It builds each archive and checksum
   without publishing, so you can download and check them first.

Local tests use disposable wallets and mock providers. They don't verify live
provider dialects, a real MCP client, or the packaged archives. Those checks
come after the workflow runs.

## Publish

1. Update `workspace.package.version` in `Cargo.toml` and refresh
   `Cargo.lock` if needed. Date the version's entry in `CHANGELOG.md`.
2. Merge the change to `main` once CI passes.
3. Tag and push:

   ```sh
   git switch main
   git pull --ff-only
   git tag -a v0.0.1 -m "sats v0.0.1"
   git push origin v0.0.1
   ```

The workflow rejects malformed tags and tags that don't match `Cargo.toml`.
It runs the installer and Rust tests before building. Re-running it replaces
existing assets, which recovers a partially failed upload.

## Verify

1. All four target jobs succeeded.
2. Every archive has its `.sha256` file.
3. `SHA256SUMS` lists every archive.
4. `setup.sh` installs the tagged release on at least one target.
5. The installed binary reports the tagged version.

The installer at `https://sats.sh/setup.sh` is the committed
`website/public/setup.sh`, and it must stay byte-for-byte identical to the
root `setup.sh`. `scripts/test-setup.sh` fails when the two drift, so update
both together.
