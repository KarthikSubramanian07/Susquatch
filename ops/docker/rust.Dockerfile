# syntax=docker/dockerfile:1.7
# rust-live: the only process that trades once the bridge is retired. Built natively on aarch64.
#
#   docker build -f ops/docker/rust.Dockerfile -t susquatch/rust-live .

FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY contracts/proto contracts/proto
# Cache mounts keep incremental rebuilds on the 2-vCPU server to minutes, not the full 10+.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p susq-live -p susq-enr --bins \
 && cp target/release/susq-live /susq-live \
 && cp target/release/enr-capture /enr-capture

FROM debian:trixie-slim
ARG GIT_SHA=unknown
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 susq && useradd --system --uid 10001 --gid susq --home /susq susq
COPY --from=build /susq-live /usr/local/bin/susq-live
# enr-capture archives CA SoS test-window feeds (ops/systemd/susq-enr-capture.timer).
COPY --from=build /enr-capture /usr/local/bin/enr-capture
ENV SUSQ_HOME=/susq SUSQ_GIT_SHA=${GIT_SHA} TZ=UTC
WORKDIR /susq
USER susq
ENTRYPOINT ["susq-live"]
