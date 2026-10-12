#!/bin/sh
set -eu

REPOSITORY="idemresearch/sats"
say() { printf '%s\n' "$*"; }
fail() { printf 'sats installer: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "install with: curl -fsSL https://sats.sh/setup.sh | sudo sh"
# All executable lookups in this root installer use system tools.
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
umask 077

local_binary=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary)
            [ "$#" -ge 2 ] || fail "--binary requires a path"
            local_binary=$2
            [ -n "$local_binary" ] || fail "--binary requires a non-empty path"
            shift 2
            ;;
        *) fail "unknown argument: $1 (usage: setup.sh [--binary PATH])" ;;
    esac
done

os=$(uname -s)
case "$os" in
    Linux) default_vault=/var/lib/sats ;;
    Darwin) default_vault=/var/db/sats ;;
    *) fail "unsupported operating system: $os" ;;
esac

# Overrides allow the root-only installer tests to use disposable accounts and
# directories. Release binaries still use their compiled-in vault location.
vault_user=${SATS_ACCOUNT:-_sats}
vault_dir=${SATS_VAULT_DIR:-$default_vault}
install_dir=${SATS_INSTALL_DIR:-/usr/local/bin}
case "$vault_user" in
    '' | *[!a-z0-9_]*) fail "invalid vault account name" ;;
esac
[ "$vault_user" != root ] || fail "the vault account must not be root"

stat_owner_mode() {
    if [ "$os" = Darwin ]; then
        stat -f '%u %Lp' "$1"
    else
        stat -c '%u %a' "$1"
    fi
}

# Walk from /, checking each parent BEFORE creating a child. Neither a symlink
# nor a caller-writable ancestor may supply or replace the installed executable.
secure_directory() {
    case "$1" in
        /*) ;;
        *) fail "install paths must be absolute: $1" ;;
    esac
    secure_rest=${1#/}
    secure_path=""
    while [ -n "$secure_rest" ]; do
        secure_part=${secure_rest%%/*}
        case "$secure_rest" in
            */*) secure_rest=${secure_rest#*/} ;;
            *) secure_rest="" ;;
        esac
        case "$secure_part" in
            '') continue ;;
            . | ..) fail "install paths must not contain . or .." ;;
        esac
        secure_path="$secure_path/$secure_part"
        [ ! -L "$secure_path" ] || fail "refusing symlink in install path: $secure_path"
        if [ ! -e "$secure_path" ]; then
            mkdir -m 0755 "$secure_path"
        fi
        [ -d "$secure_path" ] || fail "not a directory: $secure_path"
        set -- $(stat_owner_mode "$secure_path")
        [ "$1" -eq 0 ] && [ "$((0$2 & 0022))" -eq 0 ] ||
            fail "install directory must be root-owned and not group- or world-writable: $secure_path"
    done
}

# /var is an OS-managed symlink on macOS. Use its real system location before
# doing the same strict ancestor checks used for every other path.
if [ "$os" = Darwin ]; then
    case "$vault_dir" in /var/*) vault_dir="/private$vault_dir" ;; esac
fi
[ "$install_dir" != / ] || fail "refusing to install directly into /"
secure_directory "$install_dir"
install_dir=$(CDPATH= cd -P "$install_dir" && pwd)
case "$vault_dir" in
    /*/sats | /*/*) ;;
    *) fail "vault must be an absolute directory below a trusted parent" ;;
esac
vault_dir=${vault_dir%/}
case "${vault_dir##*/}" in . | .. | '') fail "invalid vault directory" ;; esac
[ ! -L "$vault_dir" ] || fail "refusing a symlink vault: $vault_dir"
secure_directory "$(dirname "$vault_dir")"

ensure_account() {
    if [ "$os" = Linux ]; then
        if ! getent passwd "$vault_user" >/dev/null; then
            nologin=$(command -v nologin) || fail "nologin is required"
            useradd --system --user-group --no-create-home --home-dir "$vault_dir" \
                --shell "$nologin" "$vault_user"
        fi
    else
        if ! dscl . -read "/Users/$vault_user" >/dev/null 2>&1; then
            dscl . -read "/Groups/$vault_user" >/dev/null 2>&1 &&
                fail "vault group already exists without its user: $vault_user"
            # UID and GID namespaces can differ: find a number unused in both.
            used_ids=$( { dscl . -list /Users UniqueID; dscl . -list /Groups PrimaryGroupID; } | awk '{print $2}')
            vault_id=499
            while [ "$vault_id" -ge 100 ]; do
                if ! printf '%s\n' "$used_ids" | grep -qx "$vault_id"; then break; fi
                vault_id=$((vault_id - 1))
            done
            [ "$vault_id" -ge 100 ] || fail "no free system UID/GID below 500"
            dscl . -create "/Groups/$vault_user"
            dscl . -create "/Groups/$vault_user" PrimaryGroupID "$vault_id"
            dscl . -create "/Groups/$vault_user" Password '*'
            dscl . -create "/Users/$vault_user"
            dscl . -create "/Users/$vault_user" UniqueID "$vault_id"
            dscl . -create "/Users/$vault_user" PrimaryGroupID "$vault_id"
            dscl . -create "/Users/$vault_user" NFSHomeDirectory "$vault_dir"
            dscl . -create "/Users/$vault_user" UserShell /usr/bin/false
            dscl . -create "/Users/$vault_user" Password '*'
            dscl . -create "/Users/$vault_user" IsHidden 1
        fi
    fi
    vault_uid=$(id -u "$vault_user")
    [ "$vault_uid" -ne 0 ] || fail "the vault account must not be root"
    if [ "$os" = Linux ]; then
        account_shell=$(getent passwd "$vault_user" | cut -d: -f7)
        group_id=$(getent group "$vault_user" | cut -d: -f3)
    else
        account_shell=$(dscl . -read "/Users/$vault_user" UserShell | awk '{print $2}')
        group_id=$(dscl . -read "/Groups/$vault_user" PrimaryGroupID | awk '{print $2}')
    fi
    case "$account_shell" in
        */false | */nologin) ;;
        *) fail "existing vault account has a login shell: $vault_user" ;;
    esac
    [ -n "$group_id" ] && [ "$(id -g "$vault_user")" = "$group_id" ] ||
        fail "vault account must have its own primary group"
}

detect_target() {
    detected_os=$(uname -s 2>/dev/null || true)
    detected_arch=$(uname -m 2>/dev/null || true)

    case "$detected_arch" in
        x86_64 | amd64) detected_arch="x86_64" ;;
        arm64 | aarch64) detected_arch="aarch64" ;;
        *) fail "unsupported architecture: ${detected_arch:-unknown}" ;;
    esac

    case "$detected_os" in
        Linux) printf '%s-unknown-linux-gnu\n' "$detected_arch" ;;
        Darwin) printf '%s-apple-darwin\n' "$detected_arch" ;;
        *) fail "unsupported operating system: ${detected_os:-unknown}" ;;
    esac
}

download() {
    download_url=$1
    download_output=$2

    case "$download_url" in
        https://*) download_scheme="https" ;;
        file://*) download_scheme="file" ;;
        *) fail "refusing non-HTTPS download: $download_url" ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        if [ "$download_scheme" = "https" ]; then
            curl -fsSL --proto '=https' --tlsv1.2 -o "$download_output" "$download_url"
        else
            curl -fsSL --proto '=file' -o "$download_output" "$download_url"
        fi
    elif command -v wget >/dev/null 2>&1; then
        [ "$download_scheme" = "https" ] || fail "curl is required for file:// downloads"
        wget -qO "$download_output" "$download_url"
    else
        fail "curl or wget is required"
    fi
}

sha256_file() {
    sha_path=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$sha_path" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$sha_path" | awk '{ print $1 }'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$sha_path" | awk '{ print $NF }'
    else
        fail "sha256sum, shasum, or openssl is required to verify the download"
    fi
}


ensure_account
# install sets ownership before permissions. Reapplying on rerun repairs both.
install -d -o "$vault_user" -g "$vault_user" -m 0700 "$vault_dir"

tmp_dir=$(mktemp -d /tmp/sats-install.XXXXXX) || fail "cannot create temporary directory"
install_stage=""
cleanup() {
    rm -rf "$tmp_dir"
    if [ -n "$install_stage" ]; then rm -rf "$install_stage"; fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

if [ -n "$local_binary" ]; then
    [ -f "$local_binary" ] && [ ! -L "$local_binary" ] || fail "--binary must name a regular file"
    source_binary=$local_binary
else
    target=$(detect_target)
    asset="sats-${target}.tar.gz"
    version=${SATS_VERSION:-latest}
    if [ "$version" = latest ]; then
        release_path=latest/download
    else
        case "$version" in
            '' | *[!0-9A-Za-z._-]*) fail "invalid SATS_VERSION: $version" ;;
        esac
        case "$version" in v*) release_tag=$version ;; *) release_tag="v$version" ;; esac
        release_path="download/$release_tag"
    fi
    release_base=${SATS_RELEASE_BASE_URL:-"https://github.com/${REPOSITORY}/releases"}
    archive="$tmp_dir/$asset"
    checksum="$archive.sha256"
    say "Downloading sats for $target..."
    download "${release_base%/}/$release_path/$asset" "$archive"
    download "${release_base%/}/$release_path/$asset.sha256" "$checksum"
    expected=$(awk 'NR == 1 { print $1 }' "$checksum" | tr 'A-F' 'a-f')
    actual=$(sha256_file "$archive" | tr 'A-F' 'a-f')
    [ "${#expected}" -eq 64 ] || fail "release checksum is malformed"
    case "$expected" in *[!0-9a-f]*) fail "release checksum is malformed" ;; esac
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset"
    # A release is exactly one regular file, not paths supplied to root tar.
    [ "$(tar -tzf "$archive")" = sats ] || fail "release archive must contain only sats"
    tar -xOzf "$archive" sats > "$tmp_dir/sats"
    [ -f "$tmp_dir/sats" ] && [ ! -L "$tmp_dir/sats" ] || fail "release archive does not contain a regular sats binary"
    source_binary="$tmp_dir/sats"
fi

# A private staging directory on the destination filesystem prevents both
# partial replacements and the vault account modifying the candidate while it
# is being verified. The binary itself is owned by the setuid target, _sats.
install_stage=$(mktemp -d "$install_dir/.sats-install.XXXXXX")
install_candidate="$install_stage/sats"
install -o "$vault_user" -g "$vault_user" -m 4555 "$source_binary" "$install_candidate"
installed_version=$("$install_candidate" --version 2>/dev/null) || fail "downloaded binary did not start"
[ -n "$installed_version" ] || fail "binary did not report a version"
mv -f "$install_candidate" "$install_dir/sats"
say "Installed $installed_version to $install_dir/sats"
say "Run sats as yourself, not with sudo."

if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    if [ "$os" = Linux ]; then
        caller_home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
    else
        caller_home=$(dscl . -read "/Users/$SUDO_USER" NFSHomeDirectory | sed 's/^NFSHomeDirectory: //')
    fi
    if [ -n "$caller_home" ] && [ -e "$caller_home/.local/bin/sats" ]; then
        say "Old executable at $caller_home/.local/bin/sats may shadow this installation. Delete it, then refresh your shell's command cache."
    fi
fi
