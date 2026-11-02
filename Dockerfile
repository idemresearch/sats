# sats-relay: the keyless relay between sats agents and the human's device.
#
#   docker build -t sats-relay .
#   docker run -p 8080:8080 -v sats-relay:/data \
#     -e SATS_RELAY_PUBLIC_HOST=relay.example.com sats-relay
#
# Terminate TLS in front of it (a reverse proxy or your platform's HTTPS).
# The image contains only the relay binary: no wallet, signer, or seed code.

FROM rust:1.91-bookworm AS build
WORKDIR /src
# Only what the workspace needs to resolve and build; rust-toolchain.toml is
# left out so the image's pinned toolchain builds without a download.
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY vendor vendor
COPY crates crates
RUN cargo build --release --locked -p sats-relay

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin sats \
    && mkdir /data && chown sats /data
COPY --from=build /src/target/release/sats-relay /usr/local/bin/sats-relay
COPY docker/relay-entrypoint.sh /usr/local/bin/relay-entrypoint
# Mount persistent storage at /data (docker run -v, or your platform's
# volumes). There is deliberately no volume instruction: some platforms
# refuse it and mount their own, often owned by root. The entrypoint takes
# ownership of /data and drops to the `sats` user before the relay starts.
ENV SATS_RELAY_LISTEN=0.0.0.0:8080 \
    SATS_RELAY_DATA=/data
EXPOSE 8080
ENTRYPOINT ["relay-entrypoint"]
