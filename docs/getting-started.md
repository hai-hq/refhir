# Getting started

Pick one path. **Docker** is for running a server (your machine, VM, or cluster) without installing Rust. **Local development** is for hacking on ReFHIR or using repo fixtures and tests.

| | Docker | Local development |
|---|--------|-------------------|
| **You need** | Docker (and optionally Docker Compose) | Rust 1.88 ([rust-toolchain.toml](../rust-toolchain.toml)), Git |
| **You get** | A `refhir` process in a container | `cargo run -p refhir` from a clone |
| **Config** | `refhir.toml` on the host (recommended) | `refhir.toml` in the repo root and/or CLI flags |
| **Data** | Docker volume (e.g. `/data`) | `refhir.sqlite` in the clone, or `--memory` |

Configuration is a `refhir.toml` file plus command-line flags; see [Configuration](configuration.md) for the file search order, the packages installed on first start, and every flag. For a production deployment start from [deploy/production.md](../deploy/production.md).

## Docker

Use this when you only want to **run** ReFHIR, not build it from source.

**1. Get an image**

- **Release image**: `ghcr.io/hai-hq/refhir:latest`, or pin a version such as `ghcr.io/hai-hq/refhir:0.0.1`
- **Build from the repo** (no registry needed):

  ```bash
  git clone https://github.com/hai-hq/refhir.git
  cd refhir
  docker build -t refhir:local .
  ```

**2. Create configuration (recommended)**

```bash
cp refhir.toml.example refhir.toml
```

Edit `refhir.toml` for bind address, database path under `/data`, optional `packages` (IGs), SMART, and terminology. The image ships a minimal default at `/etc/refhir/refhir.toml`; mounting your own file replaces it.

**3. Run**

Compose (builds the image from the clone and mounts the example config):

```bash
docker compose up --build
```

Or run the release image with persistent data and your config:

```bash
docker run --rm -p 8080:8080 \
  -v refhir-data:/data \
  -v "$PWD/refhir.toml:/etc/refhir/refhir.toml:ro" \
  -e REFHIR_PACKAGE_CACHE=/data/package-cache \
  ghcr.io/hai-hq/refhir:latest
```

Use `refhir:local` instead of `ghcr.io/hai-hq/refhir:latest` if you built locally.

**Kubernetes:** [deploy/helm/refhir](../deploy/helm/refhir/README.md).

## Local development

Use this when you work **in the repository** (server code, `cargo test`, seed scripts, clients).

**1. Clone and toolchain**

```bash
git clone https://github.com/hai-hq/refhir.git
cd refhir
# rustup installs 1.88 from rust-toolchain.toml
```

**2. Configuration (optional)**

```bash
cp refhir.toml.example refhir.toml
```

For a quick try you can skip the file and pass flags only (next step).

**3. Run the server**

Persistent SQLite file in the repo:

```bash
cargo run -p refhir -- --sqlite refhir.sqlite
```

Ephemeral (no file, good for tests):

```bash
cargo run -p refhir -- --memory
```

Equivalent with [just](https://github.com/casey/just): `just run` or `just memory`.

**4. Load demo data (optional)**

The demo files name their own ids, so they are loaded with `PUT [type]/[id]`. A server accepts that only
when it is started with `--update-as-create` (or `update_as_create = true` under `[conformance]`), so
start it that way:

```bash
cargo run -p refhir -- --memory --update-as-create
```

Then, with Go 1.23+ installed:

```bash
cd clients/go && go run ./cmd/seed -url http://127.0.0.1:8080
```

See [fixtures/mock-fhir/README.md](../fixtures/mock-fhir/README.md) and [clients/go/README.md](../clients/go/README.md#demo-data-seed-script).

**Packages at runtime:** `[conformance].packages` or repeat `--package` to add IGs after the `[fhir].version` defaults. After startup, `POST /ImplementationGuide/$install` can install `name@version` from the registry when SMART is enabled (`system/*.write`). Override the package source with `REFHIR_PACKAGE_REGISTRY`: a registry or mirror URL, or `s3://bucket/prefix` for a bundle you host yourself, built with `scripts/package-bundle.py` (see [deploy/production.md](../deploy/production.md#package-source)). ReFHIR ships no packages, so a first start needs that source to be reachable.

Regenerate the R4 fixtures used by the test suites (the server itself ships no packages): `python3 crates/fhir/generate-r4.py`.

## Verify the server

Base URL defaults to **http://127.0.0.1:8080** (or the host/port in `refhir.toml` / `--bind`).

```bash
curl -s http://127.0.0.1:8080/metadata
```

## HTTP examples

```bash
# The server assigns the id of a resource it creates (see fhir-api.md#resource-ids).
ID=$(curl -s -X POST http://127.0.0.1:8080/Patient \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Patient","active":true,"name":[{"family":"Ann"}]}' | jq -r .id)

curl -s "http://127.0.0.1:8080/Patient/$ID"
curl -s 'http://127.0.0.1:8080/Patient?name=ann'
curl -s "http://127.0.0.1:8080/Patient/$ID/_history"

curl -s -X POST http://127.0.0.1:8080/Observation \
  -H 'Content-Type: application/fhir+json' \
  -d "{\"resourceType\":\"Observation\",\"subject\":{\"reference\":\"Patient/$ID\"},\"code\":{\"coding\":[{\"code\":\"8480-6\"}]},\"effectiveDateTime\":\"2020-01-02\"}"
```
