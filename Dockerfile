# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.88
FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --workspace --bins --release --all-features --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 scion \
    && useradd --system --uid 10001 --gid scion --home-dir /data scion \
    && mkdir -p /data /config \
    && chown scion:scion /data /config
COPY --from=builder /src/target/release/scn /usr/local/bin/scn
COPY --from=builder /src/target/release/scion-server /usr/local/bin/scion-server
COPY --from=builder /src/target/release/scion-cache /usr/local/bin/scion-cache
COPY --from=builder /src/target/release/scion-daemon /usr/local/bin/scion-daemon
COPY scripts/container-init.sh /usr/local/bin/scion-init-policy
USER 10001:10001
WORKDIR /data
EXPOSE 7420 7421 7422
CMD ["scion-server", "--listen", "0.0.0.0:7420", "--data", "/data"]
