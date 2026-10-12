#!/bin/sh
# Build the website playground: sats-playground compiled to WebAssembly plus the
# wasm-bindgen JS glue, written to website/public/playground/. The output
# is committed so the website deploys without a Rust toolchain.
#
# Requires: rustup target wasm32-unknown-unknown, and a wasm-bindgen CLI
# matching the wasm-bindgen version pinned in Cargo.toml.
set -eu

cd "$(dirname "$0")/.."

WBG_VERSION="$(grep -o 'wasm-bindgen = "=[0-9.]*"' Cargo.toml | grep -o '[0-9.]*')"
if ! wasm-bindgen --version 2>/dev/null | grep -q "$WBG_VERSION"; then
    echo "error: need wasm-bindgen CLI $WBG_VERSION on PATH" >&2
    echo "  cargo install wasm-bindgen-cli --version $WBG_VERSION" >&2
    exit 1
fi

cargo build -p sats-playground --release --locked --target wasm32-unknown-unknown

OUT="website/public/playground"
rm -rf "$OUT"
wasm-bindgen target/wasm32-unknown-unknown/release/sats_playground.wasm \
    --target web --no-typescript --out-dir "$OUT"

ls -lh "$OUT"
