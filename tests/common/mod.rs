//! Shared test utilities for recall-echo integration tests.

use recall_echo::graph::GraphMemory;
use tempfile::TempDir;

/// An ephemeral graph database for testing.
/// The temp directory (and all DB data) is cleaned up when this is dropped.
pub struct TestDb {
    pub graph: GraphMemory,
    _dir: TempDir,
}

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

// Re-export fixture builders
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
