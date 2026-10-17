# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS builder
WORKDIR /app
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p refhir
# The release's FHIR packages, read and expanded once here, so that the first start of a container
# stores them instead of downloading and reading them. This needs the network at build time only
# (packages.fhir.org, or the registry REFHIR_PACKAGE_REGISTRY names); the build log records the
# SHA-256 of each tarball, and the file's header keeps them.
RUN ./target/release/refhir package-bundle --out /app/refhir-packages.bundle

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -r -u 10001 -m refhir
WORKDIR /app
COPY --from=builder /app/target/release/refhir /usr/local/bin/refhir
# Found without being named: `../share/refhir/` from the binary.
COPY --from=builder /app/refhir-packages.bundle /usr/local/share/refhir/refhir-packages.bundle
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
