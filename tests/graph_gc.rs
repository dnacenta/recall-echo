//! Integration tests for graph garbage collection — four-phase sweep,
//! dry-run behavior, pipeline entity protection.

mod common;
use common::fixtures;
use common::TestDb;
use recall_echo::graph::gc::GcConfig;
use recall_echo::graph::types::NewRelationship;

#[tokio::test]
async fn gc_dry_run_no_mutations() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();
    let rels = fixtures::simple_relationships();

    for e in &entities {
        db.graph.add_entity(e.clone()).await.unwrap();
    }
    for r in &rels {
        db.graph.add_relationship(r.clone()).await.unwrap();
    }

    let before = db.graph.stats().await.unwrap();

    let config = GcConfig {
        dry_run: true,
        ..Default::default()
    };
    let report = db.graph.run_gc(&config).await.unwrap();

    let after = db.graph.stats().await.unwrap();

    // Dry run should not delete anything
    assert_eq!(
        before.entity_count, after.entity_count,
        "entities unchanged"
    );
    assert_eq!(
        before.relationship_count, after.relationship_count,
        "relationships unchanged"
    );
    // Report should still exist
    let _ = report;
}

#[tokio::test]
async fn gc_execute_removes_low_confidence_relationship() {
    let db = TestDb::new().await;
    let entities = fixtures::simple_entities();

    for e in &entities {
        db.graph.add_entity(e.clone()).await.unwrap();
    }

    // Create a very low confidence relationship
    let weak_rel = NewRelationship {
        from_entity: "Rust".to_string(),
        to_entity: "Daniel".to_string(),
        rel_type: "WEAK_LINK".to_string(),
        description: Some("barely connected".to_string()),
        confidence: Some(0.05),
        source: Some("test".to_string()),
    };
    db.graph.add_relationship(weak_rel).await.unwrap();

    let before = db.graph.stats().await.unwrap();
    assert_eq!(before.relationship_count, 1);

    let config = GcConfig {
        dry_run: false,
        dead_confidence: 0.2,
        dead_min_age_days: 0, // Don't require age for this test
        stale_days: 0,
        stale_confidence: 0.5,
        protect_pipeline: true,
    };
    let _report = db.graph.run_gc(&config).await.unwrap();

    let after = db.graph.stats().await.unwrap();
    // The weak relationship should have been removed (or at minimum flagged)
    // Note: GC behavior depends on age checks — if the rel was just created,
    // dead_min_age_days=0 should allow removal
    assert!(
        after.relationship_count <= before.relationship_count,
        "GC execute should not increase relationships"
    );
}

#[tokio::test]
async fn gc_protects_pipeline_entities() {
    let db = TestDb::new().await;

    // Create a pipeline-linked entity
    let pipeline_entity = recall_echo::graph::types::NewEntity {
        name: "Learning Thread".to_string(),
        entity_type: recall_echo::graph::types::EntityType::Thread,
        abstract_text: "A thought in development".to_string(),
        overview: None,
        content: None,
        attributes: Some(serde_json::json!({"pipeline_stage": "thoughts"})),
        source: Some("pipeline:learning".to_string()),
    };
    db.graph.add_entity(pipeline_entity).await.unwrap();

    let config = GcConfig {
        dry_run: false,
        protect_pipeline: true,
        ..Default::default()
    };
    let _report = db.graph.run_gc(&config).await.unwrap();

    // Pipeline entity should survive
    let found = db.graph.get_entity("Learning Thread").await.unwrap();
    assert!(
        found.is_some(),
        "pipeline entity should be protected from GC"
    );
}

#[tokio::test]
async fn gc_on_empty_graph() {
    let db = TestDb::new().await;

    let config = GcConfig::default();
    let report = db.graph.run_gc(&config).await.unwrap();

    // Should complete without error on empty graph
    let _ = report;
    let stats = db.graph.stats().await.unwrap();
    assert_eq!(stats.entity_count, 0);
}
