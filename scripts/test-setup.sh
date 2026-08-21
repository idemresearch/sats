#!/bin/sh
set -eu

repo_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/sats-installer-test.XXXXXX")
cleanup() {
    rm -rf "$tmp_dir"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) printf 'unsupported test architecture\n' >&2; exit 1 ;;
esac
case "$(uname -s)" in
    Linux) target="${arch}-unknown-linux-gnu" ;;
    Darwin) target="${arch}-apple-darwin" ;;
    *) printf 'unsupported test operating system\n' >&2; exit 1 ;;
esac

asset="sats-${target}.tar.gz"
release_root="$tmp_dir/releases"
latest_dir="$release_root/latest/download"
version_dir="$release_root/download/v0.1.0"
broken_dir="$release_root/download/v0.2.0"
payload_dir="$tmp_dir/payload"
broken_payload_dir="$tmp_dir/broken-payload"
mkdir -p "$latest_dir" "$version_dir" "$broken_dir" "$payload_dir" "$broken_payload_dir"

printf '#!/bin/sh\nprintf "sats 0.1.0-test\\n"\n' > "$payload_dir/sats"
chmod 0755 "$payload_dir/sats"
tar -czf "$latest_dir/$asset" -C "$payload_dir" sats

if command -v sha256sum >/dev/null 2>&1; then
    archive_sha=$(sha256sum "$latest_dir/$asset" | awk '{ print $1 }')
else
    archive_sha=$(shasum -a 256 "$latest_dir/$asset" | awk '{ print $1 }')
fi
printf '%s  %s\n' "$archive_sha" "$asset" > "$latest_dir/$asset.sha256"
cp "$latest_dir/$asset" "$version_dir/$asset"
cp "$latest_dir/$asset.sha256" "$version_dir/$asset.sha256"
printf '#!/bin/sh\nexit 1\n' > "$broken_payload_dir/sats"
chmod 0755 "$broken_payload_dir/sats"
tar -czf "$broken_dir/$asset" -C "$broken_payload_dir" sats
if command -v sha256sum >/dev/null 2>&1; then
    broken_sha=$(sha256sum "$broken_dir/$asset" | awk '{ print $1 }')
else
    broken_sha=$(shasum -a 256 "$broken_dir/$asset" | awk '{ print $1 }')
fi
printf '%s  %s\n' "$broken_sha" "$asset" > "$broken_dir/$asset.sha256"

SATS_RELEASE_BASE_URL="file://$release_root" \
SATS_INSTALL_DIR="$tmp_dir/latest-bin" \
SATS_NO_MODIFY_PATH=1 \
    sh "$repo_root/setup.sh" > "$tmp_dir/latest.out"

test -x "$tmp_dir/latest-bin/sats"
test "$("$tmp_dir/latest-bin/sats" --version)" = "sats 0.1.0-test"
grep -F "Installed sats 0.1.0-test" "$tmp_dir/latest.out" >/dev/null

SATS_VERSION=0.1.0 \
SATS_RELEASE_BASE_URL="file://$release_root" \
SATS_INSTALL_DIR="$tmp_dir/version-bin" \
SATS_NO_MODIFY_PATH=1 \
    sh "$repo_root/setup.sh" > "$tmp_dir/version.out"
test -x "$tmp_dir/version-bin/sats"

mkdir -p "$tmp_dir/default-home"
HOME="$tmp_dir/default-home" \
SHELL=/bin/bash \
SATS_RELEASE_BASE_URL="file://$release_root" \
    sh "$repo_root/setup.sh" > "$tmp_dir/default.out"
test -x "$tmp_dir/default-home/.local/bin/sats"
grep -F 'export PATH="$HOME/.local/bin:$PATH"' "$tmp_dir/default-home/.bashrc" >/dev/null
HOME="$tmp_dir/default-home" \
SHELL=/bin/bash \
SATS_RELEASE_BASE_URL="file://$release_root" \
    sh "$repo_root/setup.sh" > "$tmp_dir/default-second.out"
test "$(grep -c 'Added by the sats installer' "$tmp_dir/default-home/.bashrc")" -eq 1

printf 'corrupt' >> "$latest_dir/$asset"
if SATS_RELEASE_BASE_URL="file://$release_root" \
    SATS_INSTALL_DIR="$tmp_dir/corrupt-bin" \
    SATS_NO_MODIFY_PATH=1 \
    sh "$repo_root/setup.sh" > "$tmp_dir/corrupt.out" 2> "$tmp_dir/corrupt.err"; then
    printf 'installer accepted a corrupt archive\n' >&2
    exit 1
fi
test ! -e "$tmp_dir/corrupt-bin/sats"
grep -F "checksum mismatch" "$tmp_dir/corrupt.err" >/dev/null

mkdir -p "$tmp_dir/atomic-bin"
printf '#!/bin/sh\nprintf "existing sats\\n"\n' > "$tmp_dir/atomic-bin/sats"
chmod 0755 "$tmp_dir/atomic-bin/sats"
if SATS_VERSION=0.2.0 \
    SATS_RELEASE_BASE_URL="file://$release_root" \
    SATS_INSTALL_DIR="$tmp_dir/atomic-bin" \
    SATS_NO_MODIFY_PATH=1 \
    sh "$repo_root/setup.sh" > "$tmp_dir/atomic.out" 2> "$tmp_dir/atomic.err"; then
    printf 'installer accepted a binary that does not start\n' >&2
    exit 1
fi
test "$("$tmp_dir/atomic-bin/sats")" = "existing sats"
grep -F "downloaded binary did not start" "$tmp_dir/atomic.err" >/dev/null

printf 'setup.sh tests passed\n'
