# Spec — Backfill and index the episode `extracted` flag

**Status:** implemented (RE-44, PR pending)
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

`serve_extract`'s poll loop runs that scan on a backoff clamped to 100ms–30s, forever,
including when there is nothing to do. On a 5k-episode store the steady-state cost is a
full table scan every 30 seconds for the life of the daemon, growing linearly with the
archive count — a recurring cost paid by every user, to learn "nothing changed".

The absent-value case that forced the `??` is a *one-time* data defect, not a permanent
shape. Migrating it away is strictly better than routing around it forever.

## Design

### Schema version 2

The store already carries a version marker (`meta:schema.schema_version`, read by
`store::read_schema_version`, written after each successful migration pass; version 0 =
pre-versioning, version 1 = persisted Beta edge evidence). RE-44 adds version 2.

```
 open()  ──▶ init_schema()
              ├── define_schema()   every statement IF NOT EXISTS
              │     DEFINE FIELD  extracted ON episode TYPE bool DEFAULT false
              │     DEFINE INDEX  episode_extracted ON episode FIELDS extracted   ← new
              │
              └── migrate()         read_schema_version() < SCHEMA_VERSION ?
                    ├── v<1: backfill_edge_evidence()      (unchanged)
                    ├── v<2: backfill_episode_extracted()  ← new
                    └── write_schema_version(2)
```

```sql
UPDATE episode SET extracted = false WHERE extracted IS NONE RETURN id
```

Crash-only, exactly like the version-1 backfill: the `UPDATE` runs *before* the version
marker is written and only touches rows that still lack a value, so an interrupted pass
re-opens, finishes the rest, and can never double-count. The statement is idempotent on
its own (second run updates 0 rows), so re-entry from any point is free.

Each backfill is gated on the version it introduced, not on `from_version == N-1`, so a
version-0 store gets both passes in one open and a version-1 store gets only the new one.

`MigrationReport` gains `episodes_backfilled` alongside `edges_backfilled`; the one-line
migration notice `GraphMemory` prints on open reports both.

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
the *output* of the index scan (the pending rows only), which in the steady state is
empty.

### Correctness depends on the backfill, so the backfill fails loudly

With `extracted = false` restored, an episode whose value is still absent is invisible to
the scan: its archive would never be extracted and nothing would say so. Therefore a
failed backfill must not be swallowed. `migrate()` returns the error, `init_schema()`
fails, and the store does not open — the version marker is not written, so the next open
retries. Every read path reaches the predicate only through a `GraphMemory` whose
`init_schema()` succeeded, which is what makes the invariant hold.

Known exposure (not new): SurrealDB coerces the *whole* document against the SCHEMAFULL
definition on `UPDATE`, so a row missing any non-option episode field (`session_id`,
`timestamp`, `abstract`) fails the statement. `crud::mark_episodes_extracted` has carried
the same exposure in production since the field existed.

### The `extracted_absent` diagnostic is kept, and re-worded

After a successful migration the count is zero on every store this binary opens, by
design. It is kept because it is now the *assertion that the migration landed*: a
non-zero value means the backfill did not reach those rows, and with the indexable
predicate that is invisible pending work rather than merely slow work. The status text
therefore prints the line only when the count is non-zero, and names it as a migration
failure with the remedy, instead of listing a permanent zero next to the `log_number`
count. Doc comments on `GraphStats::extracted_absent` and
`crud::episode_absent_field_counts` carry the same explanation.

## Out of scope

- Indexing `log_number` (a second index maintained on every episode write to speed a
  filter that only ever sees pending rows).
- Changing `serve_extract`'s poll cadence or `pending()` itself — the predicate lives one
  layer down.
- Re-extraction of archives that yielded nothing (#42/#55).
- Widening `stats()`'s diagnostic window. `extracted_absent` is still computed only when
  the store has episodes and no entities, so a store that has *both* entities and an
  unfinished migration will not show the count — unchanged from 4.4.0, and deliberate: the
  diagnostic is a full table scan and `stats()` is an agent-hot path. The loud failure of
  the backfill is the primary guard; the count is the backstop.
- A version bump: 4.4.1 is already unreleased on `main`.

## Acceptance criteria

Schema and migration
- AC1: `SCHEMA_VERSION` is 2, and `init_schema` defines
  `DEFINE INDEX IF NOT EXISTS episode_extracted ON episode FIELDS extracted`.
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

Scan
- AC7: `crud::get_unextracted_log_numbers` uses `extracted = false`, and `EXPLAIN FULL`
  of that statement contains an `IndexScan` on `episode_extracted`.
- AC8: The scan returns exactly the same log numbers before and after the change on a
  store holding all three cases — `extracted = true`, `extracted = false`, and absent →
  backfilled — and marking a log extracted still removes it.

Diagnostic
- AC9: `stats().extracted_absent` is 0 on a migrated legacy store, and `graph status`
  prints no "missing the extracted flag" line when it is 0; when it is non-zero the line
  names it as an unfinished migration.

Measurement
- AC10: A benchmark over a ≥5,000-episode synthetic store in a temp dir reports the
  backfill duration and the scan duration with and without the index, and the numbers are
  recorded here.

## Measurements

Measured 2026-09-21 on the VPS (Hostinger, SurrealKV embedded in a tempdir): 5,000
episodes across 250 log numbers, every one `extracted = true` — the quiet steady state the
daemon polls, where the scan returns nothing and the whole cost is the looking. Scans are
the mean of 20 runs, best run in brackets.

Release build (what a user runs):

| step | time |
|---|---|
| backfill of 5,000 absent `extracted` values (`init_schema`: define + migrate) | **1054 ms** |
| build of the `episode_extracted` index over 5,000 episodes | 234 ms |
| scan, `(extracted ?? false) != true` (pre-RE-44) | **11.97 ms** [11.01] |
| scan, `extracted = false`, index removed | 4.37 ms [2.98] |
| scan, `extracted = false`, index present | **0.67 ms** [0.53] |

Debug build, same store, for anyone reproducing it with a plain `cargo test`:

| step | time |
|---|---|
| backfill | 6078 ms |
| index build | 546 ms |
| scan, pre-RE-44 | 71.46 ms [60.89] |
| scan, `extracted = false`, index removed | 30.33 ms [25.97] |
| scan, `extracted = false`, index present | 5.99 ms [4.90] |

Reading:

- The recurring scan drops **~18×** at 5,000 episodes, and the shape of the cost changes:
  the old predicate is O(episodes) forever, the new one is O(rows that actually match),
  which in the steady state is zero. The gap widens with every archive a user accumulates
  — the 12 ms is not the point, the slope is.
- Half of the remaining win is the index and half is the expression: `extracted = false`
  without any index is already 2.7× faster than `(extracted ?? false) != true`, because
  the `??` is evaluated per row.
- The backfill is a **one-time ~1 s** cost on the first open of a 5,000-episode store that
  predates the field, and 0 ms on every open after: it runs once, before the version
  marker is written. A store created by any build since the field existed backfills
  nothing at all. The index build is an additional ~230 ms, once.

Reproduce with:

```
RE44_BENCH=1 cargo test --release --test extracted_index_bench -- --ignored --nocapture
```

`RE44_BENCH_EPISODES` and `RE44_BENCH_LOGS` override the fixture size. The harness builds
its store in a `TempDir` and never opens a real one.

## Tests

- `tests/migration.rs` — AC2–AC6 against real SurrealKV temp stores, reusing the existing
  legacy-store fixtures; a legacy-episode writer defines the pre-`extracted` episode
  schema, inserts, then opens normally.
- `src/graph/crud.rs` unit tests — AC8 (the existing legacy/modern/orphan scan test, now
  asserting the backfill leaves the same answer) and AC9 (`extracted_absent` is 0 after
  migration).
- A plan test asserting the `EXPLAIN FULL` output of the real statement contains
  `IndexScan` and `episode_extracted` (AC7) — the only guard against a future predicate
  edit silently dropping back to a table scan.
- `src/graph_cli.rs` unit tests — the status line is absent at 0 and present, naming the
  migration, above 0 (AC9).
- `tests/extracted_index_bench.rs` — AC10, `#[ignore]`d and gated on `RE44_BENCH=1` so
  neither a plain `cargo test` nor a `--ignored` sweep pays for it.
