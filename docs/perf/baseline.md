# Performance baseline and exit criteria

Measured with `clients/go/cmd/loadgen` against `refhir --postgres` (how to run: `README.md` in this
directory). The numbers below are the plan's measured exit criteria for Phases 0, 2 and 3, plus two
decisions that were measured before being made.

## Setup

- One 12-core Mac. The server, the load generator and Postgres 17 (in Docker, `fsync=off`) share
  the cores, so these numbers compare configurations on one machine; they are not capacity.
- Release build, FHIR R4 core fixture (`crates/fhir/fixtures/r4`, 3,157 search parameters), 32
  concurrent clients. "Pool of 1" is the baseline: one connection, which is what the single
  `Mutex<Client>` was.
- Every run restarts the server on the same database, so the data grows slightly between runs.
  The final-code runs below were taken in one sitting, in the order shown.

## Phase 0: the baseline, and where the time goes

Mixed workload `read=60,search=25,write=15`, 20 s, a pool of 1 connection:

| | |
|---|---|
| Throughput | **46.7 ops/s** (read 28.6, search 11.8, write 6.2) |
| Latency p50 / p95 / p99 | **12.1 ms / 5.50 s / 6.13 s** |
| Read p50 / p99 | 5.0 ms / 155 ms |
| Search p50 / p99 | 116 ms / 877 ms |
| Write p50 / p99 | 3.61 s / 6.21 s |

Where the time goes (server counters from `/healthz?verbose=1`, over the run):

| | pool of 1 | pool of 8 |
|---|---|---|
| commits in the run | 141 | 134 |
| `persistence.write` per commit (includes waiting for a connection) | 62.2 ms | 44.0 ms |
| waiting for earlier commits to publish, per commit | 0.6 ms | 1.8 ms |
| writing the published watermark, per commit | 15.1 ms | 1.35 ms |
| checkouts that had to wait for a connection | 927 of 1,722 | 76 of 1,617 |
| total time spent waiting for a connection | 25.2 s | 1.3 s |

Re-run on the final code, on a machine that was at load 25 to 100 on 12 cores (pool 1, twice;
pool 8, twice): the same mix gave 23.8 and 18.0 ops/s with one connection and 29.4 and 27.2 with
eight, read p50 20 and 64 ms against 4 ms. The absolute figures are half of the 46.7 above because
of the other load; compare configurations in one sitting, never numbers across days.

Reading it: with one connection more than half of all checkouts queue, and that queueing is the
largest single cost in the server's own accounting. A write spends most of its time before it
commits (extracting index keys for the full R4 catalog), which is CPU, so write latency barely
moves with the pool.

## Phase 2: does read and search throughput rise with the pool?

**Reads: yes, by a lot. Searches, and the Phase 0 mix: no gain, and a pool cannot give one.** The
plan's criterion ("read/search throughput rises with pool size versus Phase 0") is therefore met
for reads and **not met for searches**. The cause is not the pool or the measurement: the server
itself is CPU-bound in its search code (see "Why searches do not scale" below). Raising search
throughput means making searches cheaper, which is work on the search engine, not on the pool.

`scripts/perf-criteria.sh` (run by hand, or from the manual `perf-smoke` job in CI) restarts the
server for every configuration, **checks from `/healthz` that it really has the pool size it asked
for**, warms it up, and compares a pool of 8 with the baseline pool of 1 over alternating pairs.
A verdict is the median of the pairs, because a single comparison on this machine swings by tens of
percent (the server, Postgres and the load generator share 12 cores, and other programs were using
them too: the load average was 25 to 100 during these runs).

### Reads only (`read=100`, 32 clients): the pool is what limits throughput

| Run | pool 1 (ops/s) | pool 8 (ops/s) | gain |
|---|---|---|---|
| script, pair 1 | 1,186 | 2,555 | +115 % |
| script, pair 2 | 1,130 | 2,622 | +132 % |
| script, pair 3 | 1,107 | 2,676 | +142 % |
| by hand, 5 pairs | 935 to 1,004 | 2,344 to 2,409 | +137 % to +154 % |

All 8 pairs are positive, median **+132 %** in the script run. Read p50 fell from about 31 ms to
12.6 ms and p99 from 55 to 110 ms down to 28 to 35 ms. The reason is in the counters: with one
connection the 32 clients together spent 612 to 920 s of a 640 to 960 s window waiting for it (more
than 90 % of their time); with eight, 440 to 670 s.

### The Phase 0 mix (`read=60,search=25,write=15`, 32 clients): flat

Read + search operations per second, 10 alternating pairs from the script:

`+5.6, -8.9, -0.7, +11.3, +15.4, +1.1, -8.2, +4.6, +2.0, -6.9` (%), median **+1.6 %**, 6 of 10
positive. The script's own verdict is FAIL against the +3 % bar (seven-pair run: +1.1 %; three-pair
run: +2.0 %). Throughput is about 27 to 30 ops/s either way. What does change is the wait for a
connection: 17 to 28 s per run with one connection, 1.3 to 1.9 s with eight. That wait is only 2 to
3 % of the clients' time, so removing it cannot move throughput much, and the work that is left
(index extraction on writes, a few multi-second searches) is CPU. Latency moves: read p50 20 to 64 ms
with one connection, 4 ms with eight.

### Search-heavy (`read=20,search=80`, 12 clients): no gain

14 pairs from four script runs: `+23.4, -15.8, +8.1, -15.5, +7.9, -5.9, +24.1, -24.7, -15.2, -6.5,
-45.8, +10.4, +3.4, -9.9` (%), median **-6.2 %**, 6 of 14 positive: no gain, just noise. A pool of 1
waited 1 to 2 s for a connection in a 360 s client window (0.5 %), so the connection was never the
limit; a few very slow searches use the cores however many connections there are.

### Reads and searches beside writers: flat

The Phase 0 mix is a closed loop: every client draws every operation type in turn, so a client
that draws a write (about 4 s here) is not reading or searching meanwhile, which could cap read +
search throughput whatever the pool does. To rule that out, 24 dedicated clients ran
`read=70,search=30` while 8 other clients wrote at the same time (the contention Phase 0 was
about, without the artifact). Read + search ops/s, 5 alternating pairs, pool 1 then pool 8:

`142.9 / 138.3, 145.6 / 150.2, 140.3 / 139.7, 142.6 / 138.9, 140.0 / 176.3`, gains
`-3.2, +3.2, -0.4, -2.6, +25.9` (%), median **-0.4 %**. Read p50 did improve (6 to 7 ms with one
connection, 3 ms with eight) and the wait for a connection fell from 72 to 93 s to 1 to 2 s per run
(a tenth of the readers' time), but throughput did not move. The artifact was real (throughput is
5x higher without it: about 140 against 27 ops/s) and it was not what hid a pool effect.

### Why searches do not scale

During a search-heavy run with a pool of 8, the refhir process used **9.9 of the machine's 12
cores** while Postgres used 22 % of one core. The database is nearly idle and the server is
saturated by its own work, so a larger pool has nothing to unblock. The expensive operation is
`search_patient_family` (p50 about 316 ms; the other operations are 3 to 5 ms), and on average a
search-heavy operation costs about 145 to 190 CPU-ms of server time.

That cost is not something this work added. The same workload on SQLite, which the original
commit can run (its `--postgres` panics at startup, a defect fixed here, so there is no Postgres
baseline from the original), costs 148.7, 136.8 and 146.4 CPU-ms per operation on the original
commit and 145.8, 150.2 and 146.8 on this branch (means 144.0 and 147.6, a 2.5 % difference, inside
the run-to-run spread). CPU time per operation is used instead of throughput because it barely
changes when other programs load the machine.

### What this means

- The pool removes queueing on the connection, which matters when requests are cheap and many
  (reads: 2.4x), and it cuts latency. It cannot raise throughput that the CPU limits (searches, and
  the write path's index extraction), and on a machine this busy that is most of the mix.
- Earlier figures that suggested a search gain (+11 % to +16 % on a warmed-up run, +45 % on a larger
  database, and one script run with a +7.9 % median) were single comparisons or a lucky five pairs;
  14 pairs say there is no gain. Treat them as noise.
- The default stays at 8: reads gain 2.4x, latency falls, and searches lose nothing measurable.
  `--db-pool` (or `REFHIR_DB_POOL`) tunes it.
- Not tested: Postgres across a network (a round trip of a millisecond or more makes each query hold
  its connection far longer, which is where a pool of 1 would hurt searches too), and a machine
  whose cores are otherwise idle. Neither changes the finding above, that on this hardware the
  server's own CPU is the limit for searches.
- To make search throughput rise, make a search cheaper. The first place to look is
  `search_patient_family`, a string search on `Patient.name.family` (p50 about 316 ms where the
  other operations take 3 to 5 ms; the whole search-heavy mix averages 145 to 190 CPU-ms per
  operation). That is a change to the search engine, not to the pool.

A note on method: an earlier version of the script started its servers inside a command
substitution, lost track of the first one, and measured it over and over; its "pool of 8" runs
reported `max: 1`. Those results (+31 %, +13 %, -9 %, and a -66 % pair) were noise between
identical servers and are discarded. The script now refuses to run if something is already
listening, and fails if the server does not report the requested pool size.

## Phase 3: what the published-up-to watermark costs

The same commits through Postgres with and without the watermark write (`bench_watermark`,
`cargo test -p refhir --test bench_watermark -- --ignored --nocapture`; debug build, a fresh
database each run). Four runs of the final code, taken while the machine was under heavy load
from other programs (load average above 100 on 12 cores), so read the range, not one figure:

| Workload | cost per run | median | watermark time per commit |
|---|---|---|---|
| 1 writer, 300 commits | 13.4 %, 9.1 %, 7.5 %, 13.4 % | **11.3 %** | 440 to 509 us |
| 8 writers, 8 x 60 commits | 28.0 %, 9.6 %, 16.4 %, 26.7 % | **21.5 %** | 934 to 1,190 us |

An earlier set of three runs on a quieter machine measured 13.1 % and 4.5 % (medians), so the
cost of the 8-writer case is somewhere between a few percent and about a quarter of commit
throughput, and the spread is the machine's, not the code's.

What the cost is: the watermark is one extra database round trip on the commit path, after the
in-order publish. Concurrent publishers share it (whichever holds the flush lock writes the newest
timestamp and the others find theirs covered), so it is amortised across a group, but a group still
waits for it before its writers are answered. A single writer pays all of it. It buys what the
replicas need: a timestamp that is never ahead of a missing commit.

The cost could be taken off the commit path by writing the watermark from a background thread and
answering the writer first. That is safe for replicas (a late watermark only delays them, and a
new primary sets it to the highest committed timestamp when it starts), but it makes the watermark
lag the acknowledgement, so it was not done here. It is the obvious next step if commit throughput
with many concurrent writers matters more than the simplicity of "acknowledged means published".

## Decision measured: `FOR SHARE` rather than `FOR UPDATE` on the epoch row

The plan reads the writer epoch `FOR UPDATE` inside every persist. Every concurrent persist would
then wait for the previous one to commit. `epoch_lock_modes_compared` (8 concurrent writers, pool
of 8, median of 3 alternating rounds):

| Epoch check | persists/s |
|---|---|
| `FOR SHARE` (used) | **788** |
| `FOR UPDATE` (the plan's wording) | 292 |

`FOR SHARE` is 2.7x faster. It fences just as well: a successor's epoch bump is an update, which
waits for every in-flight `FOR SHARE` holder and makes every later check fail.

## Notifications

With a 30 s refresh timer, a Postgres replica caught up **16 ms** after the commit, because the
writer notifies and the replica listens. A reader on SQLite, which cannot announce a publish, does
not catch up in 600 ms (it waits for its timer).

## Raw files

The JSON written by `--out` for each run includes the server counters from
`/healthz?verbose=1` (`time_ns`, `pool`, `commits`). `--db-pool` (or `REFHIR_DB_POOL`) sets the size.
