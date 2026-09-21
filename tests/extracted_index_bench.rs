// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RE-44 AC10 — measure the schema-version-2 backfill and the extraction scan
//! with and without the `episode_extracted` index.
//!
//! This builds a synthetic store of several thousand episodes in a temp
//! directory and times four things, so the numbers in
//! `specs/extracted-flag-index.md` can be reproduced rather than asserted:
//!
//! 1. the backfill of every absent `extracted` value,
//! 2. the pre-RE-44 scan, `(extracted ?? false) != true` — un-indexable,
//! 3. the RE-44 scan, `extracted = false`, with the index removed,
//! 4. the RE-44 scan with the index in place.
//!
//! Steps 2–4 run against the state the daemon actually polls: every episode
//! already extracted, so the scan returns nothing and the whole cost is the
//! looking.
//!
//! It is `#[ignore]`d *and* gated on `RE44_BENCH`, so neither `cargo test`
//! nor a `cargo test -- --ignored` sweep pays for it:
//!
//! ```text
//! RE44_BENCH=1 cargo test --test extracted_index_bench -- --ignored --nocapture
//! ```
//!
//! `RE44_BENCH_EPISODES` overrides the episode count (default 5000) and
//! `RE44_BENCH_LOGS` the number of distinct archives they spread over
//! (default 250). The store is a `TempDir`; the real store is never opened.

use std::time::{Duration, Instant};

use recall_echo::graph::crud;
use recall_echo::graph::store::{self, Db};
use surrealdb::Surreal;
use tempfile::TempDir;

const DEFAULT_EPISODES: usize = 5_000;
const DEFAULT_LOGS: usize = 250;
const INSERT_BATCH: usize = 500;
const SCAN_REPEATS: usize = 20;

/// The scan as it was before RE-44: correct, and impossible to index.
const LEGACY_SCAN: &str = "SELECT log_number FROM episode \
     WHERE (extracted ?? false) != true AND log_number IS NOT NONE \
     GROUP BY log_number ORDER BY log_number";

/// The scan as RE-44 leaves it, spelled out here so the harness measures the
/// statement rather than whatever `crud` happens to run.
const INDEXED_SCAN: &str = "SELECT log_number FROM episode \
     WHERE extracted = false AND log_number IS NOT NONE \
     GROUP BY log_number ORDER BY log_number";

fn usize_from_env(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn enabled() -> bool {
    std::env::var_os("RE44_BENCH").is_some_and(|v| !v.is_empty())
}

fn millis(d: Duration) -> String {
    format!("{:.2}ms", d.as_secs_f64() * 1000.0)
}

async fn run_query(db: &Surreal<Db>, sql: &str) {
    db.query(sql)
        .await
        .and_then(surrealdb::IndexedResults::check)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{sql}"));
}

/// Mean and best of `SCAN_REPEATS` runs of one statement.
async fn time_scan(db: &Surreal<Db>, sql: &str) -> (Duration, Duration) {
    let mut total = Duration::ZERO;
    let mut best = Duration::MAX;
    for _ in 0..SCAN_REPEATS {
        let started = Instant::now();
        let mut response = db.query(sql).await.expect("scan failed");
        let rows: Vec<serde_json::Value> = response.take(0).expect("scan rows");
        let elapsed = started.elapsed();
        assert!(rows.is_empty(), "steady state: nothing is pending");
        total += elapsed;
        best = best.min(elapsed);
    }
    (total / SCAN_REPEATS as u32, best)
}

/// The episode table as it was before the `extracted` field existed, so the
/// rows inserted next carry no value for it.
async fn define_legacy_episode_table(db: &Surreal<Db>) {
    run_query(
        db,
        r#"
        DEFINE TABLE episode SCHEMAFULL;
        DEFINE FIELD session_id ON episode TYPE string;
        DEFINE FIELD timestamp  ON episode TYPE datetime DEFAULT time::now();
        DEFINE FIELD abstract   ON episode TYPE string;
        DEFINE FIELD log_number ON episode TYPE option<int>;
        "#,
    )
    .await;
}

async fn insert_episodes(db: &Surreal<Db>, count: usize, logs: usize) {
    for batch_start in (0..count).step_by(INSERT_BATCH) {
        let batch_end = (batch_start + INSERT_BATCH).min(count);
        let rows: Vec<String> = (batch_start..batch_end)
            .map(|i| {
                format!(
                    "{{ session_id: 'bench', abstract: 'episode {i}', log_number: {} }}",
                    i % logs
                )
            })
            .collect();
        run_query(db, &format!("INSERT INTO episode [{}]", rows.join(", "))).await;
    }
}

#[tokio::test]
#[ignore = "benchmark: set RE44_BENCH=1 to run"]
async fn measure_backfill_and_scan() {
    if !enabled() {
        println!("RE44_BENCH is unset — nothing measured.");
        return;
    }

    let episodes = usize_from_env("RE44_BENCH_EPISODES", DEFAULT_EPISODES);
    let logs = usize_from_env("RE44_BENCH_LOGS", DEFAULT_LOGS);

    let dir = TempDir::new().expect("temp dir");
    let graph_path = dir.path().join("graph");
    std::fs::create_dir_all(&graph_path).expect("graph dir");

    let db = store::open(&graph_path).await.expect("open store");
    define_legacy_episode_table(&db).await;

    let started = Instant::now();
    insert_episodes(&db, episodes, logs).await;
    let insert_time = started.elapsed();

    println!("\n── RE-44 measurement ─────────────────────────────────");
    println!("store:    {}", graph_path.display());
    println!("episodes: {episodes} across {logs} log numbers");
    println!(
        "insert:   {} (fixture, not part of the result)",
        millis(insert_time)
    );

    // Schema definition (including the new index) plus the backfill.
    let started = Instant::now();
    let migration = store::init_schema(&db).await.expect("init schema");
    let migrate_time = started.elapsed();
    assert_eq!(
        migration.episodes_backfilled, episodes as u64,
        "every episode should have been backfilled: {migration:?}"
    );
    println!(
        "backfill: {} for {} episodes (init_schema: define + migrate)",
        millis(migrate_time),
        migration.episodes_backfilled
    );

    // Sanity: both spellings of the scan agree before we start timing.
    let pending = crud::get_unextracted_log_numbers(&db).await.expect("scan");
    assert_eq!(
        pending.len(),
        logs,
        "every archive is pending after backfill"
    );

    // The state a quiet daemon polls: everything extracted, nothing to do.
    run_query(&db, "UPDATE episode SET extracted = true").await;

    let (indexed_mean, indexed_best) = time_scan(&db, INDEXED_SCAN).await;
    let (legacy_mean, legacy_best) = time_scan(&db, LEGACY_SCAN).await;

    run_query(&db, "REMOVE INDEX episode_extracted ON episode").await;
    let (unindexed_mean, unindexed_best) = time_scan(&db, INDEXED_SCAN).await;

    let started = Instant::now();
    run_query(
        &db,
        "DEFINE INDEX episode_extracted ON episode FIELDS extracted",
    )
    .await;
    let index_build = started.elapsed();

    println!("\nscan, mean of {SCAN_REPEATS} (best in brackets):");
    println!(
        "  (extracted ?? false) != true, pre-RE-44   {} [{}]",
        millis(legacy_mean),
        millis(legacy_best)
    );
    println!(
        "  extracted = false, index removed          {} [{}]",
        millis(unindexed_mean),
        millis(unindexed_best)
    );
    println!(
        "  extracted = false, index present          {} [{}]",
        millis(indexed_mean),
        millis(indexed_best)
    );
    println!(
        "\nindex build over {episodes} episodes: {}",
        millis(index_build)
    );
    println!("──────────────────────────────────────────────────────\n");

    assert!(
        indexed_mean < legacy_mean,
        "the indexed scan must beat the pre-RE-44 one: {} vs {}",
        millis(indexed_mean),
        millis(legacy_mean)
    );
}
