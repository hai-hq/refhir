# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS builder
WORKDIR /app
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p refhir

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -r -u 10001 -m refhir
WORKDIR /app
COPY --from=builder /app/target/release/refhir /usr/local/bin/refhir
COPY deploy/docker/refhir.toml /etc/refhir/refhir.toml
COPY refhir.toml.example /etc/refhir/refhir.toml.example
RUN mkdir -p /data /data/export /data/package-cache \
    && chown -R refhir:refhir /data
USER refhir
ENV REFHIR_CONFIG=/etc/refhir/refhir.toml
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["refhir"]
CMD []
