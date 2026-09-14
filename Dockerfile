# syntax=docker/dockerfile:1.7
#
# Multi-arch build. The build stage always runs natively on the build host and
# cross-compiles for TARGETPLATFORM (linux/386, linux/amd64, linux/arm64,
# linux/arm/v7), so no emulated rustc.
#
#   docker buildx build --platform linux/386 -t peterpeerdeman/pebble-ingest:1.0.0 --load .

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETPLATFORM
ARG BUILDPLATFORM
WORKDIR /src

COPY docker/rust-target.sh /usr/local/bin/rust-target
RUN rust-target

COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The three arch builds run concurrently and share these mounts. `sharing=locked`
# serialises registry access (else cargo races unpacking a crate: "…/.cargo-ok
# File exists"); the target cache is keyed per-arch so artifacts don't collide.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=target-${TARGETPLATFORM} \
    . /etc/rust-target.env \
    && cargo build --release --locked --target "$RUST_TARGET" \
    && cp "target/$RUST_TARGET/release/pebble-ingest" /pebble-ingest

FROM debian:bookworm-slim
COPY --from=build /pebble-ingest /usr/local/bin/pebble-ingest
ENV INFLUX_URL=http://influxdb:8086 \
    INFLUX_DB=pebble \
    LISTEN_ADDR=0.0.0.0:8088 \
    RUST_LOG=info
USER 65534:65534
EXPOSE 8088
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["pebble-ingest", "healthcheck"]
CMD ["pebble-ingest"]
