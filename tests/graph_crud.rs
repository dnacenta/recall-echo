//! Integration tests for graph entity and relationship CRUD operations.
//!
//! Each test spins up an ephemeral embedded SurrealDB — no external services needed.

mod common;

use common::fixtures;
use common::TestDb;
use recall_echo::graph::types::EntityType;

#[tokio::test]
async fn add_and_get_entity_by_name() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();

    let created = db.graph.add_entity(entities[0].clone()).await.unwrap();
    assert_eq!(created.name, "Rust");
    assert_eq!(created.entity_type, EntityType::Tool);

    let found = db.graph.get_entity("Rust").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "Rust");
}

#[tokio::test]
async fn get_nonexistent_entity_returns_none() {
    let db = TestDb::new().await;
    let result = db.graph.get_entity("DoesNotExist").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn add_relationship_between_entities() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();
    let rels = fixtures::simple_relationships();

    for entity in &entities {
        db.graph.add_entity(entity.clone()).await.unwrap();
    }

    let rel = db.graph.add_relationship(rels[0].clone()).await.unwrap();
    assert_eq!(rel.rel_type, "WRITTEN_IN");
}

#[tokio::test]
async fn relationship_requires_existing_entities() {
    let db = TestDb::new().await;
    let rels = fixtures::simple_relationships();

    let result = db.graph.add_relationship(rels[0].clone()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn stats_counts_entities_and_relationships() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();
    let rels = fixtures::simple_relationships();

    for entity in &entities {
        db.graph.add_entity(entity.clone()).await.unwrap();
    }
    for rel in &rels {
        db.graph.add_relationship(rel.clone()).await.unwrap();
    }

    let stats = db.graph.stats().await.unwrap();
    assert_eq!(stats.entity_count, 3);
    assert_eq!(stats.relationship_count, 2);
}

#[tokio::test]
async fn duplicate_entity_name_is_handled() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();

    db.graph.add_entity(entities[0].clone()).await.unwrap();
    // Adding same entity again — should either merge or error, not panic
    let result = db.graph.add_entity(entities[0].clone()).await;
    // Either outcome is acceptable — just don't panic
    let _ = result;
}
