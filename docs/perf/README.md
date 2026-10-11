# Performance runs

`clients/go/cmd/loadgen` drives a mixed REST FHIR workload (stdlib Go only) and
reports per-operation throughput and latency, plus the server's
`/healthz?verbose=1` counters before and after the run.

## 1. Start Postgres

```bash
docker run --rm -d --name refhir-pg -p 5432:5432 \
  -e POSTGRES_PASSWORD=refhir -e POSTGRES_DB=refhir postgres:17-alpine
```

## 2. Start the server (release build)

```bash
cargo build --release -p refhir
./target/release/refhir \
  --postgres postgres://postgres:refhir@127.0.0.1:5432/refhir \
  --bind 127.0.0.1:8080
```

- `--db-pool 1` reproduces the old single connection, which is the baseline in `baseline.md`.
  The default is 8.
- Without network access, install the bundled fixture instead of downloading the R4 packages:
  `REFHIR_SKIP_DEFAULT_PACKAGES=1 ./target/release/refhir ... --package crates/fhir/fixtures/r4`.
  (The `minir4` fixture is too small for the Observation writes the load generator makes.)
- For several processes, give each its own port and role: a primary, `--standby`, and
  `--role replica --primary-url http://127.0.0.1:8080` (see `deploy/cluster/README.md`).

## 3. Run the load generator

```bash
cd clients/go
go run ./cmd/loadgen --base http://127.0.0.1:8080 --duration 20s --concurrency 32 \
  --mix read=60,search=25,write=15 --seed-patients 200 --out /tmp/run1.json
```

Several processes: reads and searches go round-robin across `--base`, writes go
to `--write-base` (default: the first base).

```bash
go run ./cmd/loadgen --base http://127.0.0.1:8080,http://127.0.0.1:8081 \
  --write-base http://127.0.0.1:8080 --read-your-writes --verify-ryw --out /tmp/run2.json
```

- `--read-your-writes`: each worker sends the last `X-Refhir-Commit-Ts` it saw as
  `X-Refhir-Min-Ts` on its reads.
- `--verify-ryw`: after each write, GET it at once from the next base and count
  404 or stale answers as violations.
- Both tolerate a server that ignores the headers. See `-h` for the other flags.

## 4. Reading the result

- Latency percentiles (ms) cover successful (2xx) requests only. `ops_per_s`
  counts every completed request; errors come with a status histogram.
- `total` is read + search + write. Verify reads (`ryw_verify_get`) are reported
  apart so they do not inflate throughput. Seeding is outside the measured window.
- `server_stats_before` / `server_stats` are the raw `healthz?verbose=1` JSON per
  base. Counters are cumulative: diff them for pool-wait and commit timing.

## 5. Recording results

Add each run to `docs/perf/baseline.md`: date, git commit, server flags,
hardware, the loadgen command line, the stdout table and the key `server_stats`
deltas.

## 5. Checking the Phase 2 exit criterion

`scripts/perf-criteria.sh` restarts the server for each configuration, confirms from `/healthz`
that the pool is the size it asked for, and compares a pool of 8 with a pool of 1 over alternating
pairs, judging each workload by the median gain. It needs an empty Postgres database and a release
build, and the GitHub Actions `perf-smoke` job (manual `workflow_dispatch`) runs it.

- **Reads** (`read=100`, 32 clients) are judged: PASS needs a median gain of at least
  `MIN_GAIN_PCT` (default 3), and the script's exit code follows it.
- The **Phase 0 mix**, **reads and searches beside writers** and a **search-heavy** workload are
  reported with their own verdicts but do not set the exit code. They are limited by the server's
  own CPU, not by the connection, so a pool does not raise their throughput (`baseline.md` has the
  measurements, including the FAILs, and the CPU figures that show why).
- Every result line also shows the **cores** the server process used. Close to the machine's core
  count means the server is CPU-bound and no pool can help.
- `PAIRS`, `MIXED_PAIRS`, `SPLIT_PAIRS`, `INFO_PAIRS`, `DURATION`, `CLIENTS`, `READERS`, `WRITERS`
  and `--quick` tune the run time.

```bash
PERF_DB_URL=postgres://postgres:refhir@127.0.0.1:5432/emptydb scripts/perf-criteria.sh
```

Pairs on a shared machine are noisy, so judge the median, not one run. The "waited" figure beside
each result is the clients' total wait for a connection: compare it with clients x run length to
see whether the connection limited that run at all.
