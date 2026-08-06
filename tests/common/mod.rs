// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared test utilities for recall-echo integration tests.

use recall_echo::graph::GraphMemory;
use tempfile::TempDir;

/// An ephemeral graph database for testing.
/// The temp directory (and all DB data) is cleaned up when this is dropped.
///
/// Not every test binary opens a store this way — the ones that seed with raw
/// SurrealQL go through [`daemon::Fixture`] instead — so unused-code lints do
/// not apply.
#[allow(dead_code)]
pub struct TestDb {
    pub graph: GraphMemory,
    _dir: TempDir,
}

#[allow(dead_code)]
impl TestDb {
    /// Create a fresh, empty graph database.
    pub async fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let graph_path = dir.path().join("graph");
        std::fs::create_dir_all(&graph_path).expect("failed to create graph dir");

        let graph = GraphMemory::open(&graph_path)
            .await
            .expect("failed to open graph");

        Self { graph, _dir: dir }
    }
}

/// A memory directory with its own graph store and daemon socket, seeded
/// through raw SurrealQL.
///
/// Nothing here embeds anything: entities are written with their vectors left
/// absent, so a test that only exercises graph writes, evidence arithmetic or
/// rendering never loads the ONNX model.
#[allow(dead_code)]
pub mod daemon {
    use std::path::PathBuf;
    use std::sync::Once;

    use recall_echo::graph::store::{self, Db};
    use recall_echo::serve_client;
    use surrealdb::Surreal;
    use tempfile::TempDir;

    static BIN_ENV: Once = Once::new();

    /// Point the daemon client at the binary cargo just built.
    pub fn use_test_binary() {
        BIN_ENV.call_once(|| {
            std::env::set_var(
                serve_client::DAEMON_BIN_ENV,
                env!("CARGO_BIN_EXE_recall-echo"),
            );
        });
    }

    pub struct Fixture {
        _dir: TempDir,
        pub memory_dir: PathBuf,
        pub graph_dir: PathBuf,
    }

    impl Fixture {
        pub fn new() -> Self {
            use_test_binary();

            let dir = TempDir::new().expect("temp dir");
            let memory_dir = dir.path().join("e").join("memory");
            let graph_dir = memory_dir.join("graph");
            std::fs::create_dir_all(&graph_dir).expect("memory dir");

            std::fs::write(
                memory_dir.join(".recall-echo.toml"),
                format!(
                    "[serve]\nsocket_path = \"{}\"\nidle_timeout_secs = 120\n",
                    dir.path().join("g.sock").display()
                ),
            )
            .expect("write config");

            Self {
                _dir: dir,
                memory_dir,
                graph_dir,
            }
        }

        /// Open the store directly. Only valid while no daemon holds it.
        pub async fn open(&self) -> Surreal<Db> {
            let db = store::open(&self.graph_dir).await.expect("open store");
            store::init_schema(&db).await.expect("init schema");
            db
        }

        /// Stop the daemon so the store can be opened directly again.
        pub async fn stop_daemon(&self) {
            serve_client::stop_daemon(&self.memory_dir)
                .await
                .expect("stop daemon");
        }
    }

    /// Release the embedded store's process lock.
    pub async fn close(db: Surreal<Db>) {
        drop(db);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    pub async fn create_entity(db: &Surreal<Db>, id: &str, name: &str, entity_type: &str) {
        db.query(
            r#"CREATE type::record($id) SET
                   name = $name,
                   entity_type = $entity_type,
                   abstract = $abstract,
                   overview = '',
                   mutable = true,
                   access_count = 0,
                   utility_score = 0.5,
                   utility_updates = 0,
                   created_at = time::now(),
                   updated_at = time::now(),
                   source = 'test'"#,
        )
        .bind(("id", id.to_string()))
        .bind(("name", name.to_string()))
        .bind(("entity_type", entity_type.to_string()))
        .bind(("abstract", format!("{name} exists.")))
        .await
        .and_then(surrealdb::IndexedResults::check)
        .unwrap_or_else(|e| panic!("failed to create entity {id}: {e}"));
    }

    /// One seeded relationship, with its evidence stated outright.
    pub struct Edge {
        pub from: &'static str,
        pub to: &'static str,
        pub rel_type: &'static str,
        pub alpha: f64,
        pub beta: f64,
        pub self_reinforcements: i64,
    }

    impl Default for Edge {
        fn default() -> Self {
            Self {
                from: "entity:d",
                to: "entity:vim",
                rel_type: "USES",
                alpha: 9.0,
                beta: 1.0,
                self_reinforcements: 0,
            }
        }
    }

    impl Edge {
        /// The posterior mean the seeded counts imply.
        pub fn confidence(&self) -> f64 {
            self.alpha / (self.alpha + self.beta)
        }
    }

    /// Create the edge and return its record id.
    pub async fn create_edge(db: &Surreal<Db>, edge: &Edge) -> String {
        let mut response = db
            .query(
                r#"LET $from = type::record($from_id);
                   LET $to = type::record($to_id);
                   RELATE $from -> relates_to -> $to SET
                       rel_type = $rel_type,
                       description = NONE,
                       valid_from = time::now(),
                       valid_until = NONE,
                       confidence = $confidence,
                       alpha = $alpha,
                       beta = $beta,
                       self_reinforcements = $self_reinforcements,
                       last_reinforced = time::now(),
                       source = 'test'"#,
            )
            .bind(("from_id", edge.from.to_string()))
            .bind(("to_id", edge.to.to_string()))
            .bind(("rel_type", edge.rel_type.to_string()))
            .bind(("confidence", edge.confidence()))
            .bind(("alpha", edge.alpha))
            .bind(("beta", edge.beta))
            .bind(("self_reinforcements", edge.self_reinforcements))
            .await
            .expect("relate");

        let created: Vec<serde_json::Value> = response.take(2).expect("read created edge");
        created
            .first()
            .and_then(|row| row.get("id"))
            .and_then(|id| id.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| panic!("RELATE returned no edge: {created:?}"))
    }

    /// Every stored relationship as (id, confidence, alpha, beta).
    pub async fn edges(db: &Surreal<Db>) -> Vec<(String, f64, f64, f64)> {
        let mut response = db
            .query(
                "SELECT type::string(id) AS id, confidence, alpha, beta \
                 FROM relates_to ORDER BY id",
            )
            .await
            .expect("list edges");
        let rows: Vec<serde_json::Value> = response.take(0).expect("read edges");
        rows.iter()
            .map(|row| {
                (
                    row["id"].as_str().unwrap_or_default().to_string(),
                    row["confidence"].as_f64().unwrap_or_default(),
                    row["alpha"].as_f64().unwrap_or_default(),
                    row["beta"].as_f64().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// One stored relationship as (confidence, alpha, beta), by record id.
    pub async fn edge_state(db: &Surreal<Db>, edge_id: &str) -> Option<(f64, f64, f64)> {
        edges(db)
            .await
            .into_iter()
            .find(|(id, ..)| id == edge_id)
            .map(|(_, confidence, alpha, beta)| (confidence, alpha, beta))
    }

    /// Names of every stored entity, sorted.
    pub async fn entity_names(db: &Surreal<Db>) -> Vec<String> {
        let mut response = db
            .query("SELECT VALUE name FROM entity ORDER BY name")
            .await
            .expect("list entities");
        response.take(0).expect("read entity names")
    }
}

// Re-export fixture builders.
// Each integration-test binary compiles this module independently; not every
// binary uses every fixture, so unused-code lints don't apply here.
#[allow(dead_code)]
pub mod fixtures {
    use recall_echo::graph::types::{EntityType, NewEntity, NewRelationship};

    pub fn simple_entities() -> Vec<NewEntity> {
        vec![
            NewEntity {
                name: "Rust".to_string(),
                entity_type: EntityType::Tool,
                abstract_text: "Systems programming language".to_string(),
                overview: Some("Rust is a systems programming language focused on safety and performance.".to_string()),
                content: None,
                attributes: None,
                source: Some("test".to_string()),
            },
            NewEntity {
                name: "pulse-null".to_string(),
                entity_type: EntityType::Project,
                abstract_text: "Entity runtime framework".to_string(),
                overview: Some("A runtime for persistent AI entities with memory, growth, and self-monitoring.".to_string()),
                content: None,
                attributes: None,
                source: Some("test".to_string()),
            },
            NewEntity {
                name: "Daniel".to_string(),
                entity_type: EntityType::Person,
                abstract_text: "Developer and creator of pulse-null".to_string(),
                overview: Some("Freelance React Native developer learning Rust and cybersecurity.".to_string()),
                content: None,
                attributes: None,
                source: Some("test".to_string()),
            },
        ]
    }

    pub fn simple_relationships() -> Vec<NewRelationship> {
        vec![
            NewRelationship {
                from_entity: "pulse-null".to_string(),
                to_entity: "Rust".to_string(),
                rel_type: "WRITTEN_IN".to_string(),
                description: Some("pulse-null is written in Rust".to_string()),
                confidence: Some(1.0),
                source: Some("test".to_string()),
            },
            NewRelationship {
                from_entity: "Daniel".to_string(),
                to_entity: "pulse-null".to_string(),
                rel_type: "BUILDS".to_string(),
                description: Some("Daniel builds and maintains pulse-null".to_string()),
                confidence: Some(0.9),
                source: Some("test".to_string()),
            },
        ]
    }
}
