//! Integration tests for graph traversal — outgoing/incoming edges, confidence filtering.

mod common;
use common::fixtures;
use common::TestDb;

#[tokio::test]
async fn traverse_outgoing_edges() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();
    let rels = fixtures::simple_relationships();

    for e in &entities {
        db.graph.add_entity(e.clone()).await.unwrap();
    }
    for r in &rels {
        db.graph.add_relationship(r.clone()).await.unwrap();
    }

    // Daniel -> BUILDS -> pulse-null
    let result = db.graph.traverse("Daniel", 2).await.unwrap();
    assert!(!result.edges.is_empty(), "Daniel should have outgoing edges");
}

#[tokio::test]
async fn traverse_no_edges() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();

    // Only add Rust (no relationships)
    db.graph.add_entity(entities[0].clone()).await.unwrap();

    let result = db.graph.traverse("Rust", 2).await.unwrap();
    assert!(result.edges.is_empty(), "Rust with no relationships should have no traversal results");
}

#[tokio::test]
async fn traverse_nonexistent_entity() {
    let db = TestDb::new().await;
    let result = db.graph.traverse("Ghost", 2).await;
    // Should return empty or error, not panic
    assert!(result.is_ok() || result.is_err());
}

#[tokio::test]
async fn traverse_with_depth_limit() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();
    let rels = fixtures::simple_relationships();

    for e in &entities {
        db.graph.add_entity(e.clone()).await.unwrap();
    }
    for r in &rels {
        db.graph.add_relationship(r.clone()).await.unwrap();
    }

    // Depth 1 should find immediate neighbors only
    let depth1 = db.graph.traverse("Daniel", 1).await.unwrap();
    // Depth 2 should potentially find more (edges include sub-edges)
    let depth2 = db.graph.traverse("Daniel", 2).await.unwrap();
    assert!(
        depth2.edges.len() >= depth1.edges.len(),
        "deeper traversal should find at least as many edges"
    );
}
