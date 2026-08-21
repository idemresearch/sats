#!/bin/sh
set -eu

REPOSITORY="jonatns/sats"

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'sats installer: %s\n' "$*" >&2
    exit 1
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

add_default_dir_to_path() {
    [ "${SATS_NO_MODIFY_PATH:-0}" != "1" ] || return 1
    [ -n "${HOME:-}" ] || return 1

    shell_name=${SHELL:-}
    shell_name=${shell_name##*/}
    case "$shell_name" in
        zsh)
            path_profile="$HOME/.zshrc"
            path_line='export PATH="$HOME/.local/bin:$PATH"'
            ;;
        bash)
            if [ "$(uname -s 2>/dev/null || true)" = "Darwin" ]; then
                path_profile="$HOME/.bash_profile"
            else
                path_profile="$HOME/.bashrc"
            fi
            path_line='export PATH="$HOME/.local/bin:$PATH"'
            ;;
        fish)
            path_profile="$HOME/.config/fish/config.fish"
            path_line='fish_add_path "$HOME/.local/bin"'
            ;;
        *) return 1 ;;
    esac

    if [ -f "$path_profile" ] && grep -F '.local/bin' "$path_profile" >/dev/null 2>&1; then
        return 0
    fi

    mkdir -p "$(dirname "$path_profile")"
    {
        printf '\n# Added by the sats installer\n'
        printf '%s\n' "$path_line"
    } >> "$path_profile"
    say "Added ~/.local/bin to PATH in $path_profile"
    return 0
}

target=$(detect_target)
asset="sats-${target}.tar.gz"
version=${SATS_VERSION:-latest}
command -v tar >/dev/null 2>&1 || fail "tar is required"

if [ "$version" = "latest" ]; then
    release_path="latest/download"
else
    case "$version" in
        "" | *[!0-9A-Za-z._-]*) fail "invalid SATS_VERSION: $version" ;;
    esac
    case "$version" in
        v*) release_tag="$version" ;;
        *) release_tag="v$version" ;;
    esac
    release_path="download/$release_tag"
fi

release_base=${SATS_RELEASE_BASE_URL:-"https://github.com/${REPOSITORY}/releases"}
release_base=${release_base%/}
archive_url="${release_base}/${release_path}/${asset}"
checksum_url="${archive_url}.sha256"

tmp_parent=${TMPDIR:-/tmp}
tmp_dir=$(mktemp -d "${tmp_parent%/}/sats-install.XXXXXX") || fail "cannot create temporary directory"
install_candidate=""
cleanup() {
    rm -rf "$tmp_dir"
    if [ -n "$install_candidate" ]; then
        rm -f "$install_candidate"
    fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

archive="$tmp_dir/$asset"
checksum="$archive.sha256"
say "Downloading sats for $target..."
download "$archive_url" "$archive"
download "$checksum_url" "$checksum"

expected=$(awk 'NR == 1 { print $1 }' "$checksum" | tr 'A-F' 'a-f')
actual=$(sha256_file "$archive" | tr 'A-F' 'a-f')
[ "${#expected}" -eq 64 ] || fail "release checksum is malformed"
case "$expected" in
    *[!0-9a-f]*) fail "release checksum is malformed" ;;
esac
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset"

tar -xzf "$archive" -C "$tmp_dir"
[ -f "$tmp_dir/sats" ] || fail "release archive does not contain sats"

using_default_dir=0
if [ -n "${SATS_INSTALL_DIR:-}" ]; then
    install_dir=${SATS_INSTALL_DIR%/}
else
    [ -n "${HOME:-}" ] || fail "HOME is not set; set SATS_INSTALL_DIR explicitly"
    install_dir="$HOME/.local/bin"
    using_default_dir=1
fi
[ -n "$install_dir" ] || fail "SATS_INSTALL_DIR cannot be empty"
[ "$install_dir" != "/" ] || fail "refusing to install directly into /"

mkdir -p "$install_dir"
destination="$install_dir/sats"
install_candidate="${destination}.tmp.$$"
if command -v install >/dev/null 2>&1; then
    install -m 0755 "$tmp_dir/sats" "$install_candidate"
else
    cp "$tmp_dir/sats" "$install_candidate"
    chmod 0755 "$install_candidate"
fi

installed_version=$("$install_candidate" --version 2>/dev/null) || fail "downloaded binary did not start"
mv -f "$install_candidate" "$destination"
install_candidate=""
say "Installed $installed_version to $destination"

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        if [ "$using_default_dir" -eq 1 ] && add_default_dir_to_path; then
            say "Restart your shell, then run: sats --help"
        else
            say "Add sats to PATH: export PATH=\"$install_dir:\$PATH\""
        fi
        ;;
esac
