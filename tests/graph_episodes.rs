// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for episode creation, retrieval, and search.

mod common;
use common::TestDb;
use recall_echo::graph::extract::chunk_conversation;
use recall_echo::graph::types::NewEpisode;
use recall_echo::graph::IngestContext;

const CHUNK_TOKENS: usize = 500;

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

    let episodes = db
        .graph
        .get_episodes_by_session("session-001")
        .await
        .unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        episodes[0].abstract_text,
        "Discussed Rust ownership patterns"
    );
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

    let episodes = db
        .graph
        .get_episodes_by_session("session-multi")
        .await
        .unwrap();
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

fn wide(sentence: &str) -> String {
    format!("### User\n\n{sentence}\n{}", "detail ".repeat(450))
}

fn sorted_contents(episodes: Vec<recall_echo::graph::types::Episode>) -> Vec<String> {
    let mut contents: Vec<String> = episodes
        .into_iter()
        .filter_map(|episode| episode.content)
        .collect();
    contents.sort();
    contents
}

#[tokio::test]
async fn a_first_ingest_writes_one_episode_per_chunk() {
    let db = TestDb::new().await;
    db.graph
        .add_episode(NewEpisode {
            session_id: "other".into(),
            abstract_text: "other log".into(),
            overview: None,
            content: Some("other log".into()),
            log_number: Some(99),
        })
        .await
        .expect("other log");
    let text = format!(
        "{}\n---\n{}",
        wide("The shelf keeps one note."),
        wide("The shelf keeps another note.")
    );
    let chunks = chunk_conversation(&text, CHUNK_TOKENS);
    assert!(chunks.len() >= 2, "{}", chunks.len());
    let context = IngestContext::new("session-fresh", Some(7));
    let report = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("first ingest");
    assert_eq!(report.episodes_created, chunks.len() as u32);
    let stored = sorted_contents(
        db.graph
            .get_episodes_by_session("session-fresh")
            .await
            .expect("episodes"),
    );
    let mut expected = chunks;
    expected.sort();
    assert_eq!(stored, expected);
}

#[tokio::test]
async fn a_second_ingest_without_a_model_writes_no_episodes() {
    let db = TestDb::new().await;
    let text = wide("The shelf keeps one note.");
    let chunks = chunk_conversation(&text, CHUNK_TOKENS);
    assert_eq!(chunks.len(), 1, "{}", chunks.len());
    let context = IngestContext::new("session-again", Some(7));
    let first = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("first ingest");
    assert_eq!(first.episodes_created, 1);
    db.graph.mark_extracted(7).await.expect("mark extracted");
    assert!(!db.graph.log_awaits_extraction(7).await.expect("flag"));
    let second = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("second ingest");
    assert_eq!(second.episodes_created, 0);
    assert!(!db.graph.log_awaits_extraction(7).await.expect("flag"));
    let episodes = db
        .graph
        .get_episodes_by_session("session-again")
        .await
        .expect("episodes");
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].content.as_deref(), Some(chunks[0].as_str()));
}

#[tokio::test]
async fn an_ingest_without_a_log_number_writes_every_chunk_again() {
    let db = TestDb::new().await;
    let text = wide("A document with no log number.");
    let chunks = chunk_conversation(&text, CHUNK_TOKENS);
    assert_eq!(chunks.len(), 1);
    let context = IngestContext::new("document", None);
    let first = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("first");
    let second = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("second");
    assert_eq!(first.episodes_created, chunks.len() as u32);
    assert_eq!(second.episodes_created, chunks.len() as u32);
    let episodes = db
        .graph
        .get_episodes_by_session("document")
        .await
        .expect("episodes");
    assert_eq!(episodes.len(), chunks.len() * 2);
}

#[tokio::test]
async fn two_identical_chunks_stay_two_episodes_after_a_second_ingest() {
    let db = TestDb::new().await;
    let segment = wide("The shelf keeps the same note twice.");
    let text = format!("{segment}\n---\n{segment}");
    let chunks = chunk_conversation(&text, CHUNK_TOKENS);
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert_eq!(chunks[0], chunks[1]);
    let context = IngestContext::new("session-copies", Some(4));
    let first = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("first");
    assert_eq!(first.episodes_created, 2);
    let second = db
        .graph
        .ingest_archive(&text, &context, None)
        .await
        .expect("second");
    assert_eq!(second.episodes_created, 0);
    let episodes = db
        .graph
        .get_episodes_by_session("session-copies")
        .await
        .expect("episodes");
    assert_eq!(episodes.len(), 2);
    assert!(episodes
        .iter()
        .all(|episode| episode.content.as_deref() == Some(chunks[0].as_str())));
}
