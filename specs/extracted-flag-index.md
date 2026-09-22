# Spec — Backfill and index the episode `extracted` flag

**Status:** implemented (RE-44, PR #66; revised after the security, quality and
performance audits, 2026-09-21)
**Target version:** 4.4.1 (performance fix, no surface change)
**Drafted:** 2026-09-21
**Baseline:** `main` @ `c8c66a2` (post-RE-63)
**Issue:** #44
**Relation to #42 / PR #43:** follow-up. #43 fixed the *correctness* of the extraction
scan with an un-indexable predicate, deliberately deferring the *performance* half here.

---

## Goal

The background extraction worker's poll — `serve_extract::pending()` →
`GraphMemory::unextracted_log_numbers()` → `crud::get_unextracted_log_numbers()` — must be
served by an index, so a quiet daemon with nothing to extract stops paying for a full
episode-table scan every tick.

## Why this matters

PR #43 changed the scan predicate to

```sql
SELECT log_number FROM episode
WHERE (extracted ?? false) != true AND log_number IS NOT NONE
GROUP BY log_number ORDER BY log_number
```

because episodes written before the `extracted` field existed carry no value at all —
`DEFAULT` applies at creation, not retroactively — and in SurrealDB `NONE ≠ false`, so a
bare `extracted = false` could never see them. That is correct and it is permanently
un-indexable: `??` is a computed expression, and SurrealDB's planner can only serve an
index from a comparison against a plain field.

`serve_extract`'s poll loop runs that scan once per poll interval — fixed per daemon at
`(idle_after_secs / 4)` clamped to 100ms–30s, so 30 s at the default — for as long as the
machine stays quiet, which is exactly when there is least to find. On a 5k-episode store
that is a full table scan every 30 seconds for the life of the daemon, growing linearly
with the archive count: a recurring cost paid by every user, to learn "nothing changed".

The absent-value case that forced the `??` is a *one-time* data defect, not a permanent
shape. Migrating it away is strictly better than routing around it forever.

## Design

### Schema version 2

The store already carries a version marker (`meta:schema.schema_version`, read by
`store::read_schema_version`, written after each successful migration pass; version 0 =
pre-versioning, version 1 = persisted Beta edge evidence). RE-44 adds version 2.

```
 open()  ──▶ init_schema()
              ├── define_meta()          the meta table alone
              ├── read_schema_version()  ──▶ refuse if > SCHEMA_VERSION
              ├── define_schema()        every statement IF NOT EXISTS
              │     DEFINE FIELD  extracted  ON episode TYPE bool DEFAULT false
              │     DEFINE INDEX  episode_log ON episode FIELDS log_number       ← new
              │
              └── migrate_with_retries() ──▶ migrate()
                    ├── v<1: backfill_edge_evidence()        (unchanged)
                    ├── v<2: backfill_episode_extracted()    ← new, batched
                    ├── define_extracted_index()             ← new, after the backfill
                    └── write_schema_version(2)              compare-and-set
```

The migration runs in **whichever process first opens the store embedded** — a CLI
command, or the serve daemon, which most commands auto-start. `graph status` is not
special: it talks to the daemon, so on a daemon-backed store it is the *daemon* that
migrates, and the version notice lands in the daemon log rather than on the terminal.

#### The backfill

```sql
LET $batch = (SELECT VALUE id FROM episode WHERE extracted IS NONE LIMIT 1000);
UPDATE $batch SET extracted = false RETURN NONE;
RETURN count($batch);
```

Batched, because every command's open path runs this and one unbounded whole-table write
transaction is the shape that exhausts SurrealKV's memtable arena on a large store. Each
batch is one statement and therefore one transaction. SurrealDB 3.2.4 has no `LIMIT` on
`UPDATE`, hence the select-then-update pair; `RETURN NONE` plus a server-side `count()`
means one integer crosses the wire per batch, never a list of ids.

Crash-only and re-runnable from any point: `WHERE extracted IS NONE` never selects a row
twice, and the version marker is written only after the whole loop succeeds.

Two guards, because a backfill that reported success while leaving rows behind would
strand them forever — the scan this migration enables cannot see them at all:

- the loop cannot run longer than the row count it started with, so a batch that stops
  making progress cannot spin;
- a **post-condition**: after the loop, `count(extracted IS NONE)` must be 0, or the
  migration returns `Err` and the marker is not written. `Ok(0)` with rows still absent
  is not a reachable outcome.

#### Ordering: the index is built after the backfill

`episode_extracted` is defined by `migrate`, *after* the backfill, not by `define_schema`
before it. Building it first means maintaining it row by row through a whole-table
rewrite: measured at 7,243 ms versus 1,995 ms at 40k episodes, a 3.6× difference. The
backfill therefore runs unindexed — a one-time scan, against a one-time cost it more than
repays. `define_extracted_index` is `IF NOT EXISTS` and is called on the already-migrated
path too, so a store that loses the index still regains it on the next open.

#### Refusing a newer store

The marker is load-bearing now, so `init_schema` reads it *before* applying any other
definition: `define_meta` first, then the version, then everything else. A store at a
version this build does not know is refused by name — opening it would run this build's
read and write paths against a shape it has never seen, with the marker still claiming
the newer version.

#### Concurrency

Embedded stores take a process-exclusive file lock, so a migration race is only reachable
in server mode. There is deliberately **no migration lock**:

- every backfill only touches rows that still lack the value it writes, so doing it twice
  is doing it once;
- the marker write is a compare-and-set (`WHERE schema_version < $version`), so a pass
  that finishes late cannot lower a version another process already claimed;
- `migrate()` re-reads the version on each attempt, so the loser of a race finds the work
  already done rather than redoing or undoing it.

What must not happen is a transient conflict becoming a failed open, so `init_schema`
retries the pass up to four times with exponential backoff when — and only when — the
store answers with an optimistic-transaction conflict.

### Indexable predicate

```sql
SELECT log_number FROM episode
WHERE extracted = false AND log_number IS NOT NONE
GROUP BY log_number ORDER BY log_number
```

Verified on SurrealDB 3.2.4 (the version in `Cargo.lock`) with `EXPLAIN FULL`: the plan is

```
Sort(log_number) ─ Aggregate(by log_number) ─ Filter(log_number != NONE)
                                                └─ IndexScan{index: episode_extracted,
                                                             access: "= false"}
```

The `log_number IS NOT NONE` half stays an un-indexed filter on purpose — it runs over
the *output* of the index scan, not the table.

The index makes this cheaper, **not free, and not O(pending)**: SurrealKV retains
superseded entries in the `= false` range until compaction, so an episode that has been
extracted still costs the scan something until then. Measured ~15× below the un-indexed
predicate at 40k episodes, growing with each extraction cycle and partly reclaimed on
reopen. A scan whose cost really is bounded by the pending set would need a different
shape — a small `pending_extraction` table keyed by log number, rows deleted on
extraction. Out of scope here; recorded as the shape to reach for if the constant ever
stops being good enough.

The same source fragment backs a count (`UNEXTRACTED_COUNT`) so `stats()` can ask the
store how many archives are pending instead of collecting every log number into a `Vec`
to call `.len()` on it. A shared `unextracted_source!` fragment means the two cannot
drift apart.

### The other episode index: `episode_log`

`mark_episodes_extracted` (`WHERE log_number = $ln`) and `get_episode_by_log_number` are
run **once per archive** by both the daemon and `graph extract`, and without an index
each one is a full episode-table scan: 308 ms at 20k episodes, 525 ms at 40k, against
2.6 ms indexed. Draining a 1,500-archive backlog spends about 460 s in those scans alone
— two orders of magnitude more than the poll saving this change was about. `log_number`
is `option<int>` and already concrete-or-NONE on every row, so the index needs no
backfill and no version bump. It is asserted by the same kind of `EXPLAIN` test as the
scan.

### Diagnosing a migration that did not land

`extracted_absent` is kept and re-framed. After a successful migration it is zero on
every store this binary opens, by design — but it is not a leftover: it is the assertion
that the migration landed. With the indexable predicate, an episode that still has no
value is not merely slow to find, it *cannot be found at all*, which is invisible pending
work rather than slow work.

Three changes make it a usable detector:

- it is computed on **every** status, not only in the zero-entity state. Gating it on
  "no entities" made the only detector of an unfinished migration unreachable on any
  store that ever extracted a single entity. It is cheap enough: with the index it is an
  `IndexCountScan`, not a table scan.
- it has its own warning, printed outside `zero_entity_explanation`, so it is no longer
  hidden behind that function's early return when archives are pending.
- the remedy is `recall-echo graph migrate --force`, **not** `graph ingest-all`.
  `ingest-all` skips every archive that already has an episode — precisely this set — so
  it was a no-op dressed as a fix.

### `recall-echo graph migrate [--force]`

Opening the store migrates it, so this normally prints that there is nothing to do. It
exists for the one state opening cannot repair: a marker claiming a migration that did
not actually land, which leaves rows the new read paths cannot see and no migration
willing to run. `--force` clears the marker first, so every migration runs again from
version 0 — safe precisely because each one only touches rows that still lack the value
it writes. It takes the store exclusively through `serve_client::exclusive`, stopping the
daemon for the duration.

## Out of scope

- Indexing `log_number` (a second index maintained on every episode write to speed a
  filter that only ever sees pending rows).
- Changing `serve_extract`'s poll cadence or `pending()` itself — the predicate lives one
  layer down.
- Re-extraction of archives that yielded nothing (#42/#55).
- A `pending_extraction` table that would make the scan cost O(pending) rather than
  O(episodes ever extracted) — recorded above as the next shape, not built here.
- Batching `backfill_edge_evidence`. It got the same non-materialising count as the
  episode backfill, but stays a single statement: edges are far fewer than episodes and
  it has shipped that way since version 1.
- A migration lock. See *Concurrency* — idempotent backfills, a compare-and-set marker
  and a bounded conflict retry cover the reachable races without one.
- Pushing `LIMIT` into the scan: `GROUP BY`/`ORDER BY` materialise before it, so it buys
  nothing (auditor-verified).
- A version bump: 4.4.1 is already unreleased on `main`.

## Acceptance criteria

Schema and migration
- AC1: `SCHEMA_VERSION` is 2. `init_schema` defines `episode_log` in `define_schema` and
  `episode_extracted` in `migrate`, after the backfill, both `IF NOT EXISTS`.
- AC2: Opening a store whose episodes predate the `extracted` field sets every absent
  value to `false`, reports the count in `MigrationReport::episodes_backfilled`, and
  changes no other field on those rows.
- AC3: A second open of the same store is a no-op: `ran()` is false,
  `episodes_backfilled` is 0.
- AC4: A fresh (empty) store migrates once to version 2 with both backfill counts 0 and
  is a no-op on the next open.
- AC5: A version-0 store (no marker, legacy edges *and* legacy episodes) gets both
  backfills in the one pass.
- AC6: A pass interrupted between the `UPDATE` and the marker write completes on the next
  open and counts nothing twice.
- AC11: A backfill spanning more than one batch completes and counts every row exactly
  once, and ends with no episode left without a value.
- AC12: A store whose marker is above `SCHEMA_VERSION` is refused, with an error naming
  both versions.
- AC13: A row that cannot satisfy the SCHEMAFULL definition fails the open, leaves the
  marker unwritten, and the next open tries again.
- AC14: `run_migrations(force = true)` re-runs every migration against a store whose
  marker claims they are done, and backfills the stranded rows.

Scan
- AC7: `crud::get_unextracted_log_numbers` uses `extracted = false`, and `EXPLAIN FULL`
  of that statement — and of the count that shares its source — reads the
  `episode_extracted` index rather than scanning the table.
- AC8: The scan returns exactly the same log numbers before and after the change on a
  store holding all three cases — `extracted = true`, `extracted = false`, and absent →
  backfilled — and marking a log extracted still removes it.
- AC15: `EXPLAIN FULL` of `mark_episodes_extracted`'s statement reads the `episode_log`
  index.
- AC16: `count_unextracted_logs` equals `get_unextracted_log_numbers().len()` on the same
  store.

Diagnostic
- AC9: `stats().extracted_absent` is 0 on a migrated legacy store and is computed
  whatever the entity count. `graph status` prints nothing about the flag at 0; above 0
  it names an unfinished migration, points at `graph migrate --force`, and does not
  suggest `ingest-all`.

Measurement
- AC10: A benchmark over a ≥5,000-episode synthetic store in a temp dir — with
  embeddings under the HNSW index and multi-kilobyte content, like a real one — reports
  the migration duration and the scan duration with and without the index, as
  mean/p50/p95/best after discarded warm-ups, and the numbers are recorded here.

## Measurements

Measured 2026-09-21 on the VPS, release build, SurrealKV embedded in a tempdir: 5,000
episodes across 250 log numbers, each with a 384-float embedding under the HNSW index and
2 KB of `content` — the fixture matters, because a table scan reads whole records and a
skinny one flatters it. Every episode is `extracted = true`, the quiet steady state the
daemon polls, so the scan returns nothing and the whole cost is the looking. 20 timed
runs after 3 discarded warm-ups. The indexed case is timed **first**, so cache warming
favours the spelling being replaced.

| step | time |
|---|---|
| version-2 migration: define + backfill + index build, 5,000 episodes | **4464 ms**, once |
| …of which the index build alone (measured separately, *not* additional) | 303 ms |
| scan, `(extracted ?? false) != true` (pre-RE-44) | **p50 196.4 ms** (mean 195.6, p95 203.9, best 187.1) |
| scan, `extracted = false`, index removed | p50 241.9 ms (mean 248.6, p95 325.5, best 197.0) |
| scan, `extracted = false`, index present | **p50 1.02 ms** (mean 0.96, p95 1.23, best 0.65) |

**192× at p50** on the recurring poll. Reading:

- Without an index both spellings are the same thing — a full table scan — and the
  ordering between them here is noise. The whole win is the index; the earlier claim that
  half of it came from avoiding the per-row `??` was an artifact of a fixture whose rows
  were a few dozen bytes.
- The cost is **not** O(pending) and the steady state is not free. SurrealKV keeps
  superseded entries in the `= false` range until compaction, so the figure above is the
  cost after one `false → true` lifecycle and it grows with further flip cycles, partly
  reclaimed on reopen. It is O(episodes ever extracted) with a constant roughly 15× below
  the table scan (auditor's measurement at 40k). Genuinely bounding it by the pending set
  needs the separate `pending_extraction` table noted under *Out of scope*.
- The migration is a one-time ~4.5 s on a 5,000-episode store that predates the field,
  and 0 ms on every open after: it runs once, before the marker is written, in whichever
  process first opens the store embedded — often the daemon rather than the command you
  typed. A store created by any build since the field existed backfills nothing.
- Building the index after the backfill rather than before is worth 3.6× at 40k episodes
  (7,243 ms → 1,995 ms, auditor's measurement).

Separately measured by the performance audit and recorded here so nobody re-litigates
them:

- `episode_log` takes `mark_episodes_extracted` from 308 ms (20k episodes) and 525 ms
  (40k) to **2.6 ms**. A 1,500-archive backlog drain spends ~460 s in those scans without
  it — far more than this change's poll saving.
- A compound `(extracted, log_number)` index is marginally *worse* than the single-field
  one (7.8 ms vs 6.7 ms at 40k), so the scan keeps one index and a filter.
- Index maintenance costs +18% on a bare insert and +4.6% on a mark — noise beside the
  HNSW write already on that path.
- `LIMIT` on the scan buys nothing: `GROUP BY`/`ORDER BY` materialise before it.

Reproduce with:

```
RE44_BENCH=1 cargo test --release --test extracted_index_bench -- --ignored --nocapture
```

`RE44_BENCH_EPISODES` (capped at 200k) and `RE44_BENCH_LOGS` (clamped to 1..=episodes)
resize the fixture. The harness builds its store in a `TempDir` and never opens a real
one.

Real-store check (coordinator, 2026-09-21): the branch binary against a **copy** of the
VPS store — 2,291 episodes, 5,984 entities — reported `v1 → v2 (0 edges, 0 episodes
backfilled)` in ~1.3 s and was idempotent on re-open. Every episode there already carried
`extracted`, so the coercion path was not exercised on real data.

## Tests

- `tests/migration.rs` — AC2–AC6 and AC11–AC14 against real SurrealKV temp stores: a
  legacy-episode fixture that removes the field and the index before writing rows, a
  round-trip comparison of every field the backfill must not touch, a 1,200-row store
  that forces a second batch, a marker above `SCHEMA_VERSION`, an episode that cannot be
  coerced (asserted twice, to pin that the marker stays unwritten and the next open
  retries), and a `--force` repair of a store whose marker lies.
- `src/graph/crud.rs` unit tests — AC7, AC8, AC15, AC16: the legacy/modern/orphan scan
  test, `EXPLAIN FULL` assertions for both the scan and the mark predicate, and the count
  agreeing with the scan. The plan assertions walk the JSON tree structurally and handle
  both shapes SurrealDB 3.2.4 prints (`SELECT` gives `operator`/`attributes`, `UPDATE`
  gives `operation`/`detail.plan`); a pure test drives the walker over both shapes plus a
  table scan that merely names the index.
- `src/graph/store.rs` unit tests — the report summary, the lock-message classifier and
  the conflict classifier that decides what is worth retrying.
- `src/graph_cli.rs` unit tests — AC9: the migration warning's wording and pluralisation,
  that it offers `graph migrate --force` and not `ingest-all`, and that
  `zero_entity_explanation` no longer mentions the flag at all.
- `tests/extracted_index_bench.rs` — AC10, `#[ignore]`d and gated on `RE44_BENCH` so
  neither a plain `cargo test` nor a `--ignored` sweep pays for it.
