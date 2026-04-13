//! Integration tests for episode creation, retrieval, and search.

mod common;
use common::TestDb;
use recall_echo::graph::types::NewEpisode;

#[tokio::test]
async fn add_and_retrieve_episode() {
    let db = TestDb::new().await;

    let episode = NewEpisode {
        session_id: "session-001".to_string(),
        abstract_text: "Discussed Rust ownership patterns".to_string(),
        overview: Some("Deep dive into borrow checker, lifetimes, and move semantics.".to_string()),
        content: Some("Full conversation about ownership...".to_string()),
        log_number: Some(1),
    };
    let created = db.graph.add_episode(episode).await.unwrap();
    assert_eq!(created.session_id, "session-001");

    let episodes = db.graph.get_episodes_by_session("session-001").await.unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].abstract_text, "Discussed Rust ownership patterns");
}

#[tokio::test]
async fn get_episode_by_log_number() {
    let db = TestDb::new().await;

    let episode = NewEpisode {
        session_id: "session-042".to_string(),
        abstract_text: "Morning orientation".to_string(),
        overview: None,
        content: None,
        log_number: Some(42),
    };
    db.graph.add_episode(episode).await.unwrap();

    let found = db.graph.get_episode_by_log_number(42).await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().session_id, "session-042");
}

#[tokio::test]
async fn get_episode_nonexistent_log_number() {
    let db = TestDb::new().await;
    let found = db.graph.get_episode_by_log_number(999).await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn multiple_episodes_per_session() {
    let db = TestDb::new().await;

    for i in 0..3 {
        let episode = NewEpisode {
            session_id: "session-multi".to_string(),
            abstract_text: format!("Chunk {i} of conversation"),
            overview: None,
            content: None,
            log_number: Some(100 + i),
        };
        db.graph.add_episode(episode).await.unwrap();
    }

    let episodes = db.graph.get_episodes_by_session("session-multi").await.unwrap();
    assert_eq!(episodes.len(), 3);
}

#[tokio::test]
async fn episodes_count_in_stats() {
    let db = TestDb::new().await;

    let episode = NewEpisode {
        session_id: "s1".to_string(),
        abstract_text: "test".to_string(),
        overview: None,
        content: None,
        log_number: Some(1),
    };
    db.graph.add_episode(episode).await.unwrap();

    let stats = db.graph.stats().await.unwrap();
    assert_eq!(stats.episode_count, 1);
}
