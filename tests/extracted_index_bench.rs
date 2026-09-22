// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RE-44 AC10 — measure the schema-version-2 migration and the extraction
//! scan with and without the `episode_extracted` index.
//!
//! The fixture is built to look like a real store rather than a minimal one:
//! every episode carries a 384-float embedding under the HNSW index and a
//! multi-kilobyte `content`, because both change what a table scan costs and
//! what an index write costs. The episode table is defined without
//! `extracted`, so opening it runs the real migration.
//!
//! Four things are timed:
//!
//! 1. the whole version-2 migration (`init_schema`: define, backfill, index),
//! 2. the pre-RE-44 scan, `(extracted ?? false) != true` — un-indexable,
//! 3. the RE-44 scan, `extracted = false`, with the index removed,
//! 4. the RE-44 scan with the index in place,
//!
//! plus the index build on its own, which is *part of* (1) and is reported
//! separately rather than added to it.
//!
//! Steps 2–4 run against the state the daemon actually polls: every episode
//! already extracted, so the scan returns nothing and the whole cost is the
//! looking. The indexed case is measured *before* the others, so ordering
//! favours the pre-RE-44 spelling rather than the change under test.
//!
//! It is `#[ignore]`d *and* gated on `RE44_BENCH`, so neither `cargo test`
//! nor a `cargo test -- --ignored` sweep pays for it:
//!
//! ```text
//! RE44_BENCH=1 cargo test --release --test extracted_index_bench -- --ignored --nocapture
//! ```
//!
//! `RE44_BENCH_EPISODES` overrides the episode count (default 5000, capped at
//! 200_000) and `RE44_BENCH_LOGS` the number of distinct archives they spread
//! over (default 250, clamped to 1..=episodes). The store is a `TempDir`; the
//! real store is never opened.

use std::time::{Duration, Instant};

use recall_echo::graph::crud;
use recall_echo::graph::store::{self, Db};
use surrealdb::Surreal;
use tempfile::TempDir;

const DEFAULT_EPISODES: usize = 5_000;
const MAX_EPISODES: usize = 200_000;
const DEFAULT_LOGS: usize = 250;
const INSERT_BATCH: usize = 250;
const EMBEDDING_DIMENSION: usize = 384;
/// Roughly the size of one archive chunk's text.
const CONTENT_BYTES: usize = 2_048;
/// Discarded before any number is recorded — the first scans pay for caches
/// the daemon's would already have warm.
const WARMUP_RUNS: usize = 3;
const TIMED_RUNS: usize = 20;

/// The scan as it was before RE-44: correct, and impossible to index.
const LEGACY_SCAN: &str = "SELECT log_number FROM episode \
     WHERE (extracted ?? false) != true AND log_number IS NOT NONE \
     GROUP BY log_number ORDER BY log_number";

/// The scan as RE-44 leaves it, spelled out here so the harness measures the
/// statement rather than whatever `crud` happens to run.
const INDEXED_SCAN: &str = "SELECT log_number FROM episode \
     WHERE extracted = false AND log_number IS NOT NONE \
     GROUP BY log_number ORDER BY log_number";

fn enabled() -> bool {
    std::env::var_os("RE44_BENCH").is_some_and(|v| !v.is_empty())
}

fn usize_from_env(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn millis(d: Duration) -> String {
    format!("{:.2}ms", d.as_secs_f64() * 1000.0)
}

/// Mean, p50, p95 and best of a timed series.
#[derive(Debug, Clone, Copy)]
struct Timings {
    mean: Duration,
    p50: Duration,
    p95: Duration,
    best: Duration,
}

impl Timings {
    fn of(mut samples: Vec<Duration>) -> Self {
        assert!(!samples.is_empty(), "no samples");
        let total: Duration = samples.iter().sum();
        let mean = total / samples.len() as u32;
        samples.sort_unstable();
        let at = |q: f64| {
            let index = ((samples.len() as f64 - 1.0) * q).round() as usize;
            samples[index]
        };
        Self {
            mean,
            p50: at(0.50),
            p95: at(0.95),
            best: samples[0],
        }
    }

    fn line(&self, label: &str) -> String {
        format!(
            "  {label:<42} mean {:>9}  p50 {:>9}  p95 {:>9}  best {:>9}",
            millis(self.mean),
            millis(self.p50),
            millis(self.p95),
            millis(self.best)
        )
    }
}

async fn run_query(db: &Surreal<Db>, sql: &str) {
    db.query(sql)
        .await
        .and_then(surrealdb::IndexedResults::check)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{sql}"));
}

/// Time one statement, discarding [`WARMUP_RUNS`] runs first.
async fn time_scan(db: &Surreal<Db>, sql: &str) -> Timings {
    for _ in 0..WARMUP_RUNS {
        let mut response = db.query(sql).await.expect("warm-up scan failed");
        let _: Vec<serde_json::Value> = response.take(0).expect("warm-up scan rows");
    }

    let mut samples = Vec::with_capacity(TIMED_RUNS);
    for _ in 0..TIMED_RUNS {
        let started = Instant::now();
        let mut response = db.query(sql).await.expect("scan failed");
        let rows: Vec<serde_json::Value> = response.take(0).expect("scan rows");
        samples.push(started.elapsed());
        assert!(rows.is_empty(), "steady state: nothing is pending");
    }
    Timings::of(samples)
}

/// The episode table as it was before the `extracted` field existed —
/// embeddings, content and the vector index included, so the fixture writes
/// and reads cost what a real store's do.
async fn define_legacy_episode_table(db: &Surreal<Db>) {
    run_query(
        db,
        r#"
        DEFINE TABLE episode SCHEMAFULL;
        DEFINE FIELD session_id ON episode TYPE string;
        DEFINE FIELD timestamp  ON episode TYPE datetime DEFAULT time::now();
        DEFINE FIELD abstract   ON episode TYPE string;
        DEFINE FIELD content    ON episode TYPE option<string>;
        DEFINE FIELD embedding  ON episode TYPE option<array<float>>;
        DEFINE FIELD log_number ON episode TYPE option<int>;

        DEFINE INDEX episode_session ON episode FIELDS session_id;
        DEFINE INDEX episode_time    ON episode FIELDS timestamp;
        DEFINE INDEX episode_vector  ON episode FIELDS embedding HNSW DIMENSION 384 DIST COSINE;
        "#,
    )
    .await;
}

/// A deterministic unit-ish vector, cheap to build and different per row.
fn embedding_literal(seed: usize) -> String {
    let mut out = String::with_capacity(EMBEDDING_DIMENSION * 8);
    out.push('[');
    for i in 0..EMBEDDING_DIMENSION {
        if i > 0 {
            out.push(',');
        }
        let value = (((seed * 31 + i * 17) % 1000) as f64) / 1000.0;
        out.push_str(&format!("{value:.3}"));
    }
    out.push(']');
    out
}

async fn insert_episodes(db: &Surreal<Db>, count: usize, logs: usize) {
    let content = "x".repeat(CONTENT_BYTES);
    for batch_start in (0..count).step_by(INSERT_BATCH) {
        let batch_end = (batch_start + INSERT_BATCH).min(count);
        let rows: Vec<String> = (batch_start..batch_end)
            .map(|i| {
                format!(
                    "{{ session_id: 'bench', abstract: 'episode {i}', content: '{content}', \
                     embedding: {}, log_number: {} }}",
                    embedding_literal(i),
                    i % logs
                )
            })
            .collect();
        run_query(db, &format!("INSERT INTO episode [{}]", rows.join(", "))).await;
    }
}

#[tokio::test]
#[ignore = "benchmark: set RE44_BENCH=1 to run"]
async fn measure_migration_and_scan() {
    if !enabled() {
        println!("RE44_BENCH is unset — nothing measured.");
        return;
    }

    let episodes = usize_from_env("RE44_BENCH_EPISODES", DEFAULT_EPISODES).clamp(1, MAX_EPISODES);
    let logs = usize_from_env("RE44_BENCH_LOGS", DEFAULT_LOGS).clamp(1, episodes);

    let dir = TempDir::new().expect("temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("graph dir");

    let db = store::open(&graph_path).await.expect("open store");
    define_legacy_episode_table(&db).await;

    let started = Instant::now();
    insert_episodes(&db, episodes, logs).await;
    let insert_time = started.elapsed();

    println!("\n── RE-44 measurement ─────────────────────────────────────────────");
    println!("store:    {}", graph_path.display());
    println!(
        "episodes: {episodes} across {logs} log numbers, {EMBEDDING_DIMENSION}-float \
         embedding under HNSW + {CONTENT_BYTES}B content each"
    );
    println!(
        "insert:   {} (fixture, not part of the result)",
        millis(insert_time)
    );

    // The real migration: define the schema, backfill every absent value,
    // then build the index over finished data.
    let started = Instant::now();
    let migration = store::init_schema(&db).await.expect("init schema");
    let migration_time = started.elapsed();
    assert_eq!(
        migration.episodes_backfilled, episodes as u64,
        "every episode should have been backfilled: {migration:?}"
    );
    println!(
        "migration: {} total for {} episodes — define + backfill + index build",
        millis(migration_time),
        migration.episodes_backfilled
    );

    // Sanity: the scan agrees with the backfill before anything is timed.
    let pending = crud::get_unextracted_log_numbers(&db).await.expect("scan");
    assert_eq!(
        pending.len(),
        logs,
        "every archive is pending after backfill"
    );

    // The state a quiet daemon polls: everything extracted, nothing to do.
    run_query(&db, "UPDATE episode SET extracted = true").await;

    // Indexed first: any warm-cache advantage accrues to the *old* spelling.
    let indexed = time_scan(&db, INDEXED_SCAN).await;
    let legacy = time_scan(&db, LEGACY_SCAN).await;

    run_query(&db, "REMOVE INDEX episode_extracted ON episode").await;
    let unindexed = time_scan(&db, INDEXED_SCAN).await;

    // The index build on its own — a component of the migration above, not an
    // addition to it.
    let started = Instant::now();
    run_query(
        &db,
        "DEFINE INDEX episode_extracted ON episode FIELDS extracted",
    )
    .await;
    let index_build = started.elapsed();

    println!("\nscan, {TIMED_RUNS} timed runs after {WARMUP_RUNS} discarded:");
    println!("{}", legacy.line("(extracted ?? false) != true, pre-RE-44"));
    println!("{}", unindexed.line("extracted = false, index removed"));
    println!("{}", indexed.line("extracted = false, index present"));
    println!(
        "\nratio, pre-RE-44 / indexed (p50): {:.1}×",
        legacy.p50.as_secs_f64() / indexed.p50.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!(
        "index build over {episodes} episodes: {} (already counted inside the migration)",
        millis(index_build)
    );
    println!("──────────────────────────────────────────────────────────────────\n");
}
