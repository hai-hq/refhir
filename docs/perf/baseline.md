# Performance baseline and exit criteria

Measured with `clients/go/cmd/loadgen` against `refhir --postgres` (how to run: `README.md` in this
directory). The numbers below come from three stages of the project's performance work (Phases 0, 2
and 3, each with an exit criterion that is restated where it is used), plus two decisions that were
measured before being made.

## Setup

- One 12-core Mac. The server, the load generator and Postgres 17 (in Docker, `fsync=off`) share
  the cores, so these numbers compare configurations on one machine; they are not capacity.
- Release build, FHIR R4 core fixture (`crates/fhir/fixtures/r4`, 3,157 search parameters), 32
  concurrent clients. "Pool of 1" is the baseline: one connection, which is what the single
  `Mutex<Client>` was.
- Every run restarts the server on the same database, so the data grows slightly between runs.
  The final-code runs below were taken in one sitting, in the order shown.

## Phase 0: the baseline, and where the time goes

These figures were taken before the search fix described under Phase 2 (`Model::known_child` scanned
the whole FHIR model for every step of every row of a search), so searches in them are about as slow
as they will ever be; the same workload on the current code gives much higher read+search
throughput. They are kept as the record of where the time went at the start, not as a target.

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

**Yes, for reads and for searches, once a search-code bug was fixed.** The plan's criterion
("read/search throughput rises with pool size versus Phase 0") is met in every workload, 12 of 12
pairs, by +77 % to +359 %. It was *not* met before the fix, and the reason is worth keeping on record.

`scripts/perf-criteria.sh` (run by hand, or from the manual `perf-smoke` job in CI) restarts the
server for every configuration, **checks from `/healthz` that it really has the pool size it asked
for**, warms it up, and compares a pool of 8 with the baseline pool of 1 over alternating pairs. A
verdict is the median of the pairs, because a single comparison on this machine swings by tens of
percent (the server, Postgres and the load generator share 12 cores, and other programs were using
them too: the load average was 15 to 100 during these runs). Each result line also shows the
total time the clients waited for a connection and how many cores the server used.

### After the fix (3 alternating pairs per workload, pool 1 then pool 8)

| Workload | pool 1 (ops/s) | pool 8 (ops/s) | median gain | pairs |
|---|---|---|---|---|
| Reads (`read=100`, 32 clients) | 587, 348, 534 | 1,099, 1,122, 1,657 | **+210 %** | +87, +223, +210 |
| Phase 0 mix, read+search (32 clients) | 210, 144, 196 | 420, 492, 429 | **+119 %** | +100, +242, +119 |
| Reads+searches beside 8 writers (24 clients) | 120, 124, 263 | 552, 422, 819 | **+240 %** | +359, +240, +211 |
| Search-heavy (`read=20,search=80`, 12 clients) | 447, 356, 252 | 790, 800, 800 | **+125 %** | +77, +125, +217 |

With one connection the clients waited 330 to 920 s per run for it (most of their time) and the
server used 0.3 to 0.5 cores: the connection was the limit. With eight, the wait fell to 57 to 680 s
and throughput rose with it. (Absolute figures for reads are lower than in the runs before the fix;
that code is untouched, and the machine was busier. Compare within a table, not across them.)

### Across a network (5 ms added delay)

Everything above ran against a Postgres on the same machine. To see a database that is not next
door, the same script ran against a Postgres container with `tc netem delay 5ms` on its interface
(a connection plus one request took 1.0 ms to the normal container and 15.5 ms to this one; the
delay is on the database's replies only, so it is a rough stand-in for a database in another zone,
not a measurement of one). 2 pairs per workload, pool 1 then pool 8:

| Workload | pool 1 (ops/s) | pool 8 (ops/s) | median gain |
|---|---|---|---|
| Reads | 67, 62 | 378, 468 | **+562 %** |
| Phase 0 mix, read+search | 17, 12 | 86, 92 | **+520 %** |
| Reads+searches beside writers | 36, 35 | 265, 253 | **+635 %** |
| Search-heavy | 54, 54 | 435, 442 | **+713 %** |

8 of 8 pairs positive. With a delay between the server and its database a pool matters far more,
because every query holds its connection for the whole round trip: one connection is limited to
about 1 / (round trip) queries per second, and eight give about eight times that. The first
start's install of the R4 definitions into this database took more than 4 minutes (the script's
old fixed wait for the server, which it now takes from `START_TIMEOUT`).

The cluster integration tests (lease, takeover, a killed primary, replicas, catalog reload, `/sync`
on a replica) also pass through this delay, 9 of 9 (15 s, against 8 s without it), so none of the
lease keepalive, takeover or refresh timers is too tight for a few milliseconds of latency.

### Before the fix: searches did not gain, and that was a bug

The same comparison before the fix, over 14 pairs of search-heavy and 10 of the Phase 0 mix, gave
medians of -6.2 % and +1.6 %, while reads gained +132 %. Dedicated readers running beside writers
(to rule out the Phase 0 mix's closed-loop artifact, where a client that draws a four second write
is not reading meanwhile) were flat too: median -0.4 % over 5 pairs.

The server's own counters said why: during a search-heavy run the refhir process used **9.9 of the
machine's 12 cores** while Postgres used 22 % of one. A profile (`sample` on the running server)
put 95 % of the on-CPU time in one function, reached as
`search` -> `Extractor::matches_op` -> `path_match` -> `fhirpath::evaluate` -> `Model::known_child`.
Every search re-checks each candidate row by evaluating the search parameter's FHIRPath
expression, and `known_child` answered "is this an element of this path?" by **scanning every
element path in the FHIR model** (tens of thousands for R4) and building a `format!` string for
each one. That is one allocation per element, for every step of every row of every search, about
145 to 190 CPU-ms per operation on the search-heavy mix. With the server saturated by that, no
number of connections could raise throughput.

The cost was already in the original commit (CPU per operation on SQLite: 144.0 ms there, 147.6 ms
on this branch, a 2.5 % difference inside the run-to-run spread; the original's `--postgres`
panics at startup, which this work fixed, so there is no Postgres baseline from it).

### The fix

`known_child` is now two lookups in the model's sorted map, `is_choice` one, and `has_descendants`
a range query that looks at the first key not below the prefix (keys that share a prefix are
contiguous). Tests check each against the scan it replaced over every key of a model.

On SQLite, search only, 12 clients, same data and workload:

| | before | after |
|---|---|---|
| Throughput | 286.8 ops/s | **1,688 ops/s** (5.9x) |
| `search_patient_family` p50 | 62 ms | 5 ms |
| Server CPU per operation | not recorded for this run | 1.6 ms |
| On-CPU profile samples (25 s) | 48,568, one function 95 % | 10,314, flat (syscalls, allocator, SQLite) |

(On the Postgres search-heavy mix, before the fix, the server averaged 181 to 198 CPU-ms per
operation; the SQLite run above was not timed in CPU terms before the fix.) A deeper saving remains available, because a search
still fetches every row in the index interval, re-verifies each one and builds the whole result
before applying `_count`; it was not needed to meet the criterion and the profile is flat now.

### Notes

- The default pool stays at 8. `--db-pool` (or `REFHIR_DB_POOL`) tunes it.
- Not tested: Postgres on a real network (the 5 ms run above only simulates one on a single
  machine), where each query holds its connection longer and a pool matters more, and a machine
  with a very different core count.
- An earlier version of the script started its servers inside a command substitution, lost track
  of the first one, and measured it over and over (its "pool of 8" runs reported `max: 1`); those
  results (+31 %, +13 %, -9 %, and a -66 % pair) were noise between identical servers and are
  discarded. The script now refuses to run if something is already listening, and fails if the
  server does not report the requested pool size.

## Search: the cost no longer grows with the matches

After the `known_child` fix `search()` still had three costs that grew with the number of
*matches*, not with `_count`: it read every row in the index range with its document (decoded), it
evaluated the parameter against each of those documents again even though the index had already
selected them, and it copied, sorted and counted every match before cutting the result to the page.

What changed (`crates/fhir/src/search.rs`, `crates/database/src/transaction.rs`,
`PersistenceReader::index_scan_ids`):

- **Rows.** When the index range decides every clause, only the keys and document ids of the range
  are read (no join to `documents`, no JSON), and only the page's documents are fetched. The scan
  reads the index table straight in key order, so with `_total=none` and a `_count` it stops one
  row after the page instead of listing the range. The read recorded for conflict detection is
  still the whole range.
- **Re-check.** A string, token, reference or uri parameter read from a path is "proven" by its
  range (the keys are written from the very values the check compares), so the clause the range
  was chosen for is not evaluated again. Every other clause, the date and number kinds, any
  modifier, `_sort`, `_compartment`, chains and `_has` are still checked, and so is any expression
  that asks the world something when it is read (`resolve()`, the clock, a profile, a value set).
- **Whole result.** Rows are decoded one at a time, only the page is kept, and without `_sort`
  nothing is copied or sorted. Without a wanted total the scan ends once the page and one more match
  are found.

Measured on a SQLite database of 22,000 patients, 2,000 per family name, one query at a time, p50
latency in ms, two alternating rounds (`scripts/perf-search.sh`; the machine was busy, so the second
round is slower throughout):

| Query | before, round 1 / 2 | after, round 1 / 2 |
|---|---|---|
| `family=X&_count=10` | 107.5 / 94.1 | 30.3 / 22.0 |
| `family=X&_count=10&_total=none` | 105.3 / 97.4 | **3.5 / 2.7** |
| `family=X&_count=10&_offset=1000` | 108.3 / 97.1 | 11.8 / 18.6 |
| the same, `_total=none` | 101.7 / 107.4 | **7.1 / 11.1** |
| `family=X&_summary=count` | 111.9 / 101.5 | 21.1 / 37.5 |
| `_count=10`, no filter | 515.5 / 503.2 | 109.1 / 174.7 |
| `_count=10&_total=none`, no filter | 485.7 / 1731.6 | **1.4 / 3.8** |
| `family=X&name=<prefix>&_count=10&_total=none` (second clause checked, all match) | 158.6 / 164.5 | 29.2 / 50.0 |
| `family=X&name=<prefix>&_count=10` (exact total) | 169.6 / 148.7 | 80.1 / 156.5 |
| `family=X&birthdate=ge1900-01-01&_count=10` (second clause checked, none match) | 95.2 / 86.5 | 79.1 / 148.2 |

What improved: a search the index decides is about 3 to 9x cheaper with an exact total and 10 to
450x cheaper without one (the larger the range, the larger the saving). What did **not**: a search with a clause the index cannot decide, with an exact
total, still has to evaluate that clause on every row of the range, and there is no saving there
(the last two rows, within the noise of the machine). With `_total=none` and a second clause the
scan stops early but still reads its whole candidate range with the documents in one query, so
it is 3 to 5x cheaper, not 50x. An exact total still counts the range: 5 to 8 microseconds a row
on SQLite, from the no-filter rows above (22,000 rows in 109 to 175 ms).

The assumption this adds: the keys are trusted. They are written by the same extraction code that
reads them, and a write tombstones every key that stops applying, so a live row means a matching
document. Across a release that changes how a value becomes keys, rows saved by the older release
keep their old keys until they are written again, and a search will no longer set aside the ones
that would not match now (its second check used to). Re-index after such a release.

How it is held to the old behaviour: `search_reference` keeps the previous implementation for
tests, and 817 Patient and 72 Observation queries (every combination of the parameters, paging,
`_total`, `_summary`, `_sort`, `_include`/`_revinclude`, modifiers and boundary dates, over data
with updated, deleted and multi-key documents) must return the same bundle and record the same
reads; an oracle that does not use either implementation checks the matches; counters assert that
a page of 10 among 300 matches decodes 10 documents and that `_total=none` reads 11 rows; and the
storage conformance test (SQLite and Postgres) checks the id-only scan against the full scan across
timestamps, intervals, both orders and limits, with moved keys, deleted and re-created documents,
and a key rewritten thirty times that a chunked read has to resume in the middle of.

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

Through the same 5 ms delay (three runs): one writer 13.6 %, 14.1 %, 6.0 % (median 13.6 %), eight
writers 13.2 %, 6.0 %, 10.1 % (median 10.1 %), with 8 to 12 ms of watermark time per commit. The
share is about the same as on a local database, because a commit already takes several round trips
and the watermark is one more.

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
