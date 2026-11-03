#!/bin/sh
# Start the relay as the unprivileged `sats` user.
#
# Platforms often mount persistent storage owned by root. Started as root,
# this takes ownership of the data directory, then drops to `sats` before
# the relay runs. Started as any other user, it runs the relay directly.
set -eu
data="${SATS_RELAY_DATA:-/data}"
if [ "$(id -u)" = 0 ]; then
    mkdir -p "$data"
    chown sats:sats "$data"
    exec setpriv --reuid=sats --regid=sats --init-groups --inh-caps=-all -- \
        /usr/local/bin/sats-relay "$@"
fi
exec /usr/local/bin/sats-relay "$@"
