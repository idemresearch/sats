#!/bin/sh
# The reproducible MCP demo: an agent session over stdio and the human's
# CLI, through approval, denial, expiry, revocation, and a failed
# broadcast. Offline and deterministic: an isolated wallet per scene and
# the mock chain. The demo is crates/sats-cli/tests/demo.rs, so every
# outcome it prints is also asserted.
set -eu

cd "$(dirname "$0")/.."

exec cargo test -p sats-cli --locked --test demo -- --nocapture --test-threads=1 --quiet
