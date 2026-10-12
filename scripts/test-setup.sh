#!/bin/sh
set -eu

[ "$(id -u)" -eq 0 ] || { echo 'run: sudo sh scripts/test-setup.sh' >&2; exit 1; }
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
repo_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
cmp -s "$repo_root/setup.sh" "$repo_root/website/public/setup.sh" || {
    echo 'website/public/setup.sh differs from setup.sh' >&2; exit 1;
}

case "$(uname -s)" in
    Linux) os=Linux; parent=/var/lib; suffix=unknown-linux-gnu ;;
    Darwin) os=Darwin; parent=/private/var/db; suffix=apple-darwin ;;
    *) echo 'unsupported test platform' >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) echo 'unsupported test architecture' >&2; exit 1 ;;
esac
# /tmp is world-writable, and must be rejected as an install ancestor. This
# disposable directory has only trusted ancestors, just like a real install.
tmp_dir=$(mktemp -d "$parent/sats-installer-test.XXXXXX")
vault_user="_satstest_$$"
if id "$vault_user" >/dev/null 2>&1; then
    echo 'test account already exists; refusing to reuse it' >&2; exit 1
fi
cleanup() {
    if [ "$os" = Linux ]; then
        userdel "$vault_user" 2>/dev/null || true
        groupdel "$vault_user" 2>/dev/null || true
    else
        dscl . -delete "/Users/$vault_user" 2>/dev/null || true
        dscl . -delete "/Groups/$vault_user" 2>/dev/null || true
    fi
    rm -rf "$tmp_dir"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM
chmod 0755 "$tmp_dir"
cp "$repo_root/setup.sh" "$tmp_dir/setup.sh"
chmod 0755 "$tmp_dir/setup.sh"
export SATS_ACCOUNT="$vault_user"
export SATS_VAULT_DIR="$tmp_dir/vault"
export SATS_INSTALL_DIR="$tmp_dir/bin"
export SATS_RELEASE_BASE_URL="file://$tmp_dir/releases"
unset SATS_VERSION

asset="sats-$arch-$suffix.tar.gz"
latest="$tmp_dir/releases/latest/download"
version="$tmp_dir/releases/download/v0.1.0"
broken="$tmp_dir/releases/download/v0.2.0"
mkdir -p "$latest" "$version" "$broken" "$tmp_dir/payload"
printf '#!/bin/sh\nprintf "sats 0.1.0-test\\n"\n' > "$tmp_dir/payload/sats"
chmod 0755 "$tmp_dir/payload/sats"
tar -czf "$latest/$asset" -C "$tmp_dir/payload" sats
checksum() {
    if [ "$os" = Linux ]; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}
checksum "$latest/$asset" > "$latest/$asset.sha256"
cp "$latest/$asset" "$version/$asset"
cp "$latest/$asset.sha256" "$version/$asset.sha256"
printf '#!/bin/sh\nexit 1\n' > "$tmp_dir/broken-sats"
chmod 0755 "$tmp_dir/broken-sats"

metadata() {
    if [ "$os" = Darwin ]; then stat -f '%u:%g:%Lp' "$1"; else stat -c '%u:%g:%a' "$1"; fi
}
reject() {
    expected=$1
    shift
    if "$@" > "$tmp_dir/reject.out" 2> "$tmp_dir/reject.err"; then
        echo "installer incorrectly accepted: $*" >&2; exit 1
    fi
    grep -F "$expected" "$tmp_dir/reject.err" >/dev/null || {
        cat "$tmp_dir/reject.err" >&2; exit 1;
    }
}

# Fresh install and the actual OS account backend (useradd or dscl).
sh "$tmp_dir/setup.sh" > "$tmp_dir/fresh.out"
vault_uid=$(id -u "$vault_user")
vault_gid=$(id -g "$vault_user")
test "$(metadata "$SATS_INSTALL_DIR/sats")" = "$vault_uid:$vault_gid:4555"
test "$(metadata "$SATS_VAULT_DIR")" = "$vault_uid:$vault_gid:700"
test "$("$SATS_INSTALL_DIR/sats" --version)" = 'sats 0.1.0-test'
if [ "$os" = Darwin ]; then
    test "$vault_uid" -lt 500
    test "$vault_gid" -lt 500
    dscl . -read "/Users/$vault_user" IsHidden | grep -F 'IsHidden: 1' >/dev/null
fi

# Rerun must repair permissions and preserve the account and vault contents.
printf preserved > "$SATS_VAULT_DIR/marker"
chmod 0755 "$SATS_VAULT_DIR"
chown root "$SATS_VAULT_DIR"
sh "$tmp_dir/setup.sh" --binary "$tmp_dir/payload/sats" > "$tmp_dir/rerun.out"
test "$(id -u "$vault_user")" = "$vault_uid"
test "$(metadata "$SATS_VAULT_DIR")" = "$vault_uid:$vault_gid:700"
test "$(metadata "$SATS_INSTALL_DIR/sats")" = "$vault_uid:$vault_gid:4555"
test "$(cat "$SATS_VAULT_DIR/marker")" = preserved
SATS_VERSION=0.1.0 sh "$tmp_dir/setup.sh" > "$tmp_dir/pinned.out"

# Refusal is based on real process identity, not a mocked id command.
if [ "$os" = Linux ]; then
    reject '| sudo sh' runuser -u nobody -- sh "$tmp_dir/setup.sh"
else
    reject '| sudo sh' sudo -u nobody sh "$tmp_dir/setup.sh"
fi

mkdir "$tmp_dir/writable" "$tmp_dir/caller-owned" "$tmp_dir/unsafe-parent"
chmod 0777 "$tmp_dir/writable" "$tmp_dir/unsafe-parent"
chown nobody "$tmp_dir/caller-owned"
reject 'root-owned and not group- or world-writable' env SATS_INSTALL_DIR="$tmp_dir/writable" sh "$tmp_dir/setup.sh"
reject 'root-owned and not group- or world-writable' env SATS_INSTALL_DIR="$tmp_dir/caller-owned" sh "$tmp_dir/setup.sh"
reject 'root-owned and not group- or world-writable' env SATS_INSTALL_DIR="$tmp_dir/unsafe-parent/bin" sh "$tmp_dir/setup.sh"
test ! -e "$tmp_dir/unsafe-parent/bin"
ln -s "$SATS_INSTALL_DIR" "$tmp_dir/link"
reject 'refusing symlink' env SATS_INSTALL_DIR="$tmp_dir/link" sh "$tmp_dir/setup.sh"
ln -s "$SATS_VAULT_DIR" "$tmp_dir/vault-link"
reject 'symlink vault' env SATS_VAULT_DIR="$tmp_dir/vault-link" sh "$tmp_dir/setup.sh"

# Both download failures and broken local builds preserve the old executable.
old_sum=$(checksum "$SATS_INSTALL_DIR/sats" | awk '{print $1}')
printf corrupt >> "$latest/$asset"
reject 'checksum mismatch' sh "$tmp_dir/setup.sh"
reject 'binary did not start' sh "$tmp_dir/setup.sh" --binary "$tmp_dir/broken-sats"
cp "$tmp_dir/broken-sats" "$tmp_dir/payload/sats"
tar -czf "$broken/$asset" -C "$tmp_dir/payload" sats
checksum "$broken/$asset" > "$broken/$asset.sha256"
reject 'binary did not start' env SATS_VERSION=0.2.0 sh "$tmp_dir/setup.sh"
test "$(checksum "$SATS_INSTALL_DIR/sats" | awk '{print $1}')" = "$old_sum"
test "$(metadata "$SATS_INSTALL_DIR/sats")" = "$vault_uid:$vault_gid:4555"
test -z "$(find "$SATS_INSTALL_DIR" -name '.sats-install.*' -print)"
printf 'setup.sh tests passed (%s)\n' "$os"
