// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Human correction, end to end through the daemon.
//!
//! Three properties, in order of how much they matter:
//!
//! - **A correction is evidence.** `--wrong` moves the Beta counts and the
//!   posterior mean, and the move survives the daemon and a reopen.
//! - **Nothing is damaged on a guess.** A name that does not resolve, and an
//!   entity whose claims are ambiguous, both come back with candidates and
//!   leave the store byte-identical.
//! - **Destruction is confirmed.** An unconfirmed forget reports what would go
//!   and removes nothing.
//!
//! Fixtures are raw SurrealQL: correction is graph writes and arithmetic, so no
//! embedding model is involved.

mod common;

use common::daemon::{self, Edge, Fixture};
use recall_echo::graph::correct::{CorrectTarget, Correction, CorrectionReport};
use recall_echo::graph::store::Db;
use recall_echo::graph_cli::{self, CorrectOptions};
use recall_echo::serve::{AddEntityArgs, CorrectArgs, RelateArgs, Request};
use recall_echo::serve_client;
use std::path::Path;
use surrealdb::Surreal;

/// Weight of one user-authored observation at the default provenance weights.
const USER_WEIGHT: f64 = 0.8;

// ── Fixtures ─────────────────────────────────────────────────────────────

/// D uses Vim (strongly held), and Vim is configured by dotfiles.
async fn seed_two_claims(db: &Surreal<Db>) -> (String, String) {
    daemon::create_entity(db, "entity:d", "D", "person").await;
    daemon::create_entity(db, "entity:vim", "Vim", "tool").await;
    daemon::create_entity(db, "entity:dotfiles", "dotfiles", "project").await;

    let uses = daemon::create_edge(
        db,
        &Edge {
            from: "entity:d",
            to: "entity:vim",
            rel_type: "USES",
            alpha: 9.0,
            beta: 1.0,
            self_reinforcements: 0,
        },
    )
    .await;
    let configured = daemon::create_edge(
        db,
        &Edge {
            from: "entity:vim",
            to: "entity:dotfiles",
            rel_type: "CONFIGURED_BY",
            alpha: 6.0,
            beta: 4.0,
            self_reinforcements: 2,
        },
    )
    .await;
    (uses, configured)
}

/// Only the first claim: an entity with exactly one thing said about it.
async fn seed_one_claim(db: &Surreal<Db>) -> String {
    daemon::create_entity(db, "entity:d", "D", "person").await;
    daemon::create_entity(db, "entity:vim", "Vim", "tool").await;
    daemon::create_edge(db, &Edge::default()).await
}

async fn correct(
    memory_dir: &Path,
    target: CorrectTarget,
    correction: Correction,
) -> CorrectionReport {
    let request = Request::Correct(CorrectArgs { target, correction });
    let data = serve_client::execute(memory_dir, &request)
        .await
        .expect("correction request");
    serde_json::from_value(data).expect("decode correction report")
}

fn entity(name: &str) -> CorrectTarget {
    CorrectTarget::Entity {
        name: name.to_string(),
    }
}

fn edge(from: &str, rel_type: &str, to: &str) -> CorrectTarget {
    CorrectTarget::Edge {
        from: from.to_string(),
        rel_type: rel_type.to_string(),
        to: to.to_string(),
    }
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ── Contradiction ────────────────────────────────────────────────────────

#[tokio::test]
async fn contradicting_a_relationship_lowers_confidence_and_persists() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    let uses = seed_one_claim(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::Contradicted { edges } = report else {
        panic!("expected a contradiction, got {report:?}");
    };
    assert_eq!(edges.len(), 1);
    let correction = &edges[0];

    // The user's word is one observation at user weight, not a decree: alpha
    // is untouched, beta grows by the weight, and the mean follows.
    assert!(approx(correction.confidence_before, 0.9));
    assert!(approx(correction.evidence_before, 10.0));
    assert!(
        approx(correction.edge.confidence, 9.0 / (10.0 + USER_WEIGHT)),
        "got {}",
        correction.edge.confidence
    );
    assert!(approx(correction.edge.evidence, 10.0 + USER_WEIGHT));
    assert_eq!(correction.edge.from, "D");
    assert_eq!(correction.edge.to, "Vim");

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    let (confidence, alpha, beta) = daemon::edge_state(&db, &uses)
        .await
        .expect("the edge is still there");
    assert!(approx(alpha, 9.0), "corroboration is not erased: {alpha}");
    assert!(approx(beta, 1.0 + USER_WEIGHT), "got {beta}");
    assert!(approx(confidence, 9.0 / (10.0 + USER_WEIGHT)));
    daemon::close(db).await;
}

#[tokio::test]
async fn saying_it_twice_is_twice_the_evidence() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    let uses = seed_one_claim(&db).await;
    daemon::close(db).await;

    for _ in 0..2 {
        correct(
            &fixture.memory_dir,
            edge("D", "USES", "Vim"),
            Correction::Wrong { all_edges: false },
        )
        .await;
    }

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    let (confidence, _, beta) = daemon::edge_state(&db, &uses).await.expect("edge");
    assert!(approx(beta, 1.0 + 2.0 * USER_WEIGHT), "got {beta}");
    assert!(confidence < 9.0 / (10.0 + USER_WEIGHT));
    daemon::close(db).await;
}

#[tokio::test]
async fn an_entity_with_one_claim_needs_no_disambiguation() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    let uses = seed_one_claim(&db).await;
    daemon::close(db).await;

    // Correcting the entity means correcting what is claimed about it — and
    // with one claim there is nothing to choose between.
    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    let CorrectionReport::Contradicted { edges } = report else {
        panic!("expected a contradiction, got {report:?}");
    };
    assert_eq!(edges[0].edge.id, uses);
}

#[tokio::test]
async fn an_entity_with_several_claims_asks_which_and_changes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    let before = daemon::edges(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::Ambiguous { entity, edges } = report else {
        panic!("expected disambiguation, got {report:?}");
    };
    assert_eq!(entity, "Vim");
    assert_eq!(edges.len(), 2, "both claims are offered: {edges:?}");
    assert!(
        edges.iter().any(|edge| edge.rel_type == "USES")
            && edges.iter().any(|edge| edge.rel_type == "CONFIGURED_BY"),
        "{edges:?}"
    );
    // The one carrying self-authored corroboration says so, unprompted.
    assert_eq!(
        edges
            .iter()
            .find(|edge| edge.rel_type == "CONFIGURED_BY")
            .map(|edge| edge.self_reinforcements),
        Some(2)
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(
        daemon::edges(&db).await,
        before,
        "a refusal to guess must not move a single count"
    );
    daemon::close(db).await;
}

#[tokio::test]
async fn asking_for_all_of_them_contradicts_every_claim() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Wrong { all_edges: true },
    )
    .await;
    let CorrectionReport::Contradicted { edges } = report else {
        panic!("expected a contradiction, got {report:?}");
    };
    assert_eq!(edges.len(), 2);
    assert!(
        edges
            .iter()
            .all(|edge| edge.edge.confidence < edge.confidence_before),
        "{edges:?}"
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    for (id, _, _, beta) in daemon::edges(&db).await {
        assert!(
            beta > 1.0,
            "every claim took a hit — {id} is at beta {beta}"
        );
    }
    daemon::close(db).await;
}

#[tokio::test]
async fn an_entity_nothing_is_claimed_about_has_nothing_to_contradict() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    daemon::create_entity(&db, "entity:lonely", "Lonely", "concept").await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Lonely"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    assert!(
        matches!(report, CorrectionReport::NothingToCorrect { ref entity } if entity == "Lonely"),
        "{report:?}"
    );
}

// ── Refusing to guess ────────────────────────────────────────────────────

#[tokio::test]
async fn a_name_that_does_not_resolve_lists_the_closest_and_changes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    let before = daemon::edges(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("dotfile"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::UnknownEntity { query, candidates } = report else {
        panic!("expected a refusal, got {report:?}");
    };
    assert_eq!(query, "dotfile");
    assert_eq!(
        candidates.first().map(|entity| entity.name.as_str()),
        Some("dotfiles"),
        "the near-miss is offered, not applied: {candidates:?}"
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await, before);
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn a_name_close_to_nothing_offers_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("zzzzqqqq"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    let CorrectionReport::UnknownEntity { candidates, .. } = report else {
        panic!("expected a refusal, got {report:?}");
    };
    assert!(candidates.is_empty(), "{candidates:?}");
}

#[tokio::test]
async fn a_relationship_that_does_not_exist_reports_the_ones_that_do() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "HATES", "Vim"),
        Correction::Wrong { all_edges: false },
    )
    .await;

    let CorrectionReport::NoSuchEdge {
        from,
        to,
        rel_type,
        existing,
    } = report
    else {
        panic!("expected a refusal, got {report:?}");
    };
    assert_eq!(
        (from.as_str(), rel_type.as_str(), to.as_str()),
        ("D", "HATES", "Vim")
    );
    assert_eq!(
        existing
            .iter()
            .map(|edge| edge.rel_type.as_str())
            .collect::<Vec<_>>(),
        vec!["USES"],
        "what does connect them is worth saying"
    );
}

/// The graph stored the claim one way round; the human need not know which.
#[tokio::test]
async fn a_relationship_named_backwards_still_resolves() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_one_claim(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("Vim", "uses", "D"),
        Correction::Wrong { all_edges: false },
    )
    .await;
    assert!(
        matches!(report, CorrectionReport::Contradicted { ref edges } if edges.len() == 1),
        "{report:?}"
    );
}

// ── Forgetting ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unconfirmed_forget_reports_what_would_go_and_removes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: false },
    )
    .await;

    let CorrectionReport::Planned { removal } = report else {
        panic!("expected a plan, got {report:?}");
    };
    assert_eq!(
        removal.entity.as_ref().map(|entity| entity.name.as_str()),
        Some("Vim")
    );
    assert_eq!(
        removal.edges.len(),
        2,
        "the plan names every relationship going with it: {:?}",
        removal.edges
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 2, "nothing was removed");
    assert!(daemon::entity_names(&db).await.contains(&"Vim".to_string()));
    daemon::close(db).await;
}

#[tokio::test]
async fn a_confirmed_forget_removes_the_entity_and_its_relationships() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let CorrectionReport::Removed { removal } = report else {
        panic!("expected a removal, got {report:?}");
    };
    assert_eq!(removal.edges.len(), 2);

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert!(
        daemon::edges(&db).await.is_empty(),
        "an entity's edges go with it"
    );
    assert_eq!(
        daemon::entity_names(&db).await,
        vec!["D".to_string(), "dotfiles".to_string()],
        "its neighbours stay"
    );
    daemon::close(db).await;
}

#[tokio::test]
async fn forgetting_one_relationship_leaves_both_entities() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let CorrectionReport::Removed { removal } = report else {
        panic!("expected a removal, got {report:?}");
    };
    assert!(removal.entity.is_none());
    assert_eq!(removal.edges.len(), 1);

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 1, "only that claim went");
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn forgetting_a_name_that_does_not_resolve_removes_nothing() {
    let fixture = Fixture::new();

    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vimm"),
        Correction::Forget { confirmed: true },
    )
    .await;
    assert!(
        matches!(report, CorrectionReport::UnknownEntity { .. }),
        "{report:?}"
    );

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    assert_eq!(daemon::edges(&db).await.len(), 2);
    daemon::close(db).await;
}

fn enable_lasting(memory_dir: &Path) {
    let path = memory_dir.join(".recall-echo.toml");
    let mut text = std::fs::read_to_string(&path).expect("config");
    text.push_str("\n[graph]\nlasting_forget = true\n");
    std::fs::write(&path, text).expect("write config");
}

fn forget_rows(memory_dir: &Path) -> Vec<serde_json::Value> {
    let path = memory_dir.join("forgotten.jsonl");
    let text = std::fs::read_to_string(&path).expect("forget file");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect()
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

async fn try_correct(
    memory_dir: &Path,
    target: CorrectTarget,
    correction: Correction,
) -> Result<CorrectionReport, recall_echo::error::RecallError> {
    let request = Request::Correct(CorrectArgs { target, correction });
    let data = serve_client::execute(memory_dir, &request).await?;
    Ok(serde_json::from_value(data)?)
}

struct HeldForgetLock {
    file: std::fs::File,
}

impl HeldForgetLock {
    fn on(path: &Path) -> Self {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("lock file");
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "could not hold the forget lock");
        Self { file }
    }
}

impl Drop for HeldForgetLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[tokio::test]
async fn flag_off_forget_deletes_and_writes_no_file() {
    let fixture = Fixture::new();
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    assert!(matches!(report, CorrectionReport::Removed { .. }));
    assert!(!fixture.memory_dir.join("forgotten.jsonl").exists());
    assert!(!fixture.memory_dir.join(".forgotten.lock").exists());
}

#[tokio::test]
async fn an_unconfirmed_forget_with_the_flag_on_writes_no_record() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: false },
    )
    .await;
    assert!(matches!(report, CorrectionReport::Planned { .. }));
    assert!(!fixture.memory_dir.join("forgotten.jsonl").exists());
    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 2);
    daemon::close(db).await;
}

#[tokio::test]
async fn a_confirmed_relationship_forget_records_each_live_edge() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let CorrectionReport::Removed { removal } = report else {
        panic!("expected a removal");
    };
    assert!(removal.entity.is_none());
    assert_eq!(removal.edges.len(), 1);

    let rows = forget_rows(&fixture.memory_dir);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["v"], 1);
    assert_eq!(rows[0]["status"], "active");
    assert_eq!(rows[0]["kind"], "fact");
    assert_eq!(rows[0]["subject"], "D");
    assert_eq!(rows[0]["relation"], "USES");
    assert_eq!(rows[0]["value"], "Vim");
    assert_eq!(rows[0]["provenance"], "user");
    assert_eq!(mode_of(&fixture.memory_dir.join("forgotten.jsonl")), 0o600);
    assert_eq!(mode_of(&fixture.memory_dir.join(".forgotten.lock")), 0o600);

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 1);
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn a_confirmed_entity_forget_records_the_name_and_every_edge() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let CorrectionReport::Removed { removal } = report else {
        panic!("expected a removal");
    };
    assert_eq!(removal.edges.len(), 2);

    let rows = forget_rows(&fixture.memory_dir);
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().any(|row| {
        row["kind"] == "entity" && row["subject"] == "vim" && row["status"] == "active"
    }));
    assert!(rows.iter().any(|row| {
        row["kind"] == "fact" && row["subject"] == "D" && row["relation"] == "USES"
    }));
    assert!(rows.iter().any(|row| {
        row["kind"] == "fact" && row["subject"] == "Vim" && row["relation"] == "CONFIGURED_BY"
    }));
    assert_eq!(mode_of(&fixture.memory_dir.join("forgotten.jsonl")), 0o600);
}

#[tokio::test]
async fn a_held_forget_lock_deletes_nothing() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let lock_path = fixture.memory_dir.join(".forgotten.lock");
    let _held = HeldForgetLock::on(&lock_path);
    let err = try_correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: true },
    )
    .await
    .expect_err("lock");
    let message = err.to_string();
    assert!(
        message.contains(&lock_path.display().to_string()),
        "{message}"
    );
    assert!(message.contains("locked"), "{message}");
    assert!(!fixture.memory_dir.join("forgotten.jsonl").exists());

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 2);
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn unforget_without_yes_writes_nothing_and_yes_deactivates_one_fact() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;
    correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;

    let options = CorrectOptions {
        subject: "D".into(),
        rel_type: Some("uses".into()),
        object: Some("Vim".into()),
        wrong: false,
        forget: false,
        all_edges: false,
        yes: false,
        unforget: true,
    };
    let err = graph_cli::correct(&fixture.memory_dir, &options)
        .await
        .expect_err("needs yes");
    assert!(err.to_string().contains("--yes"), "{err}");
    assert_eq!(forget_rows(&fixture.memory_dir)[0]["status"], "active");

    let mut confirmed = options;
    confirmed.yes = true;
    graph_cli::correct(&fixture.memory_dir, &confirmed)
        .await
        .expect("unforget");
    let rows = forget_rows(&fixture.memory_dir);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "inactive");
    assert!(rows[0]["unforgotten_at"].as_str().is_some());
}

#[tokio::test]
async fn unforget_with_the_flag_off_names_the_flag_and_writes_nothing() {
    let fixture = Fixture::new();
    let path = fixture.memory_dir.join("forgotten.jsonl");
    std::fs::write(&path, b"{not json\n").expect("leftover");
    let err = graph_cli::correct(
        &fixture.memory_dir,
        &CorrectOptions {
            subject: "Vim".into(),
            rel_type: None,
            object: None,
            wrong: false,
            forget: false,
            all_edges: false,
            yes: true,
            unforget: true,
        },
    )
    .await
    .expect_err("flag off");
    assert!(err.to_string().contains("lasting_forget"), "{err}");
    assert_eq!(std::fs::read(&path).expect("bytes"), b"{not json\n");
}

#[tokio::test]
async fn an_entity_unforget_clears_that_subject_only() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;
    correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;

    graph_cli::correct(
        &fixture.memory_dir,
        &CorrectOptions {
            subject: "Vim".into(),
            rel_type: None,
            object: None,
            wrong: false,
            forget: false,
            all_edges: false,
            yes: true,
            unforget: true,
        },
    )
    .await
    .expect("unforget");

    let rows = forget_rows(&fixture.memory_dir);
    assert_eq!(rows.len(), 3);
    let active: Vec<_> = rows
        .iter()
        .filter(|row| row["status"] == "active")
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["subject"], "D");
    assert_eq!(active[0]["relation"], "USES");
}

#[tokio::test]
async fn relate_and_add_entity_name_unforget_when_the_flag_is_on() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let db = fixture.open().await;
    seed_one_claim(&db).await;
    daemon::close(db).await;
    correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;

    let relate = serve_client::execute(
        &fixture.memory_dir,
        &Request::Relate(RelateArgs {
            from: "D".into(),
            rel_type: "USES".into(),
            to: "Vim".into(),
            description: None,
            source: None,
        }),
    )
    .await
    .expect_err("relate");
    assert!(relate.to_string().contains("--unforget"), "{relate}");

    correct(
        &fixture.memory_dir,
        entity("Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    let add = serve_client::execute(
        &fixture.memory_dir,
        &Request::AddEntity(AddEntityArgs {
            name: "Vim".into(),
            entity_type: "tool".into(),
            abstract_text: "editor".into(),
            overview: None,
            source: None,
        }),
    )
    .await
    .expect_err("add");
    assert!(add.to_string().contains("--unforget"), "{add}");

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert!(!daemon::entity_names(&db)
        .await
        .iter()
        .any(|name| name == "Vim"));
    daemon::close(db).await;
}

#[tokio::test]
async fn a_malformed_forget_file_deletes_nothing() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let path = fixture.memory_dir.join("forgotten.jsonl");
    std::fs::write(&path, b"{not json\n").expect("leftover");
    let db = fixture.open().await;
    seed_two_claims(&db).await;
    daemon::close(db).await;

    let err = try_correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await
    .expect_err("malformed forget file");
    let message = err.to_string();
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("invalid JSON"), "{message}");
    assert_eq!(std::fs::read(&path).expect("bytes"), b"{not json\n");

    fixture.stop_daemon().await;
    let db = fixture.open().await;
    assert_eq!(daemon::edges(&db).await.len(), 2);
    assert_eq!(daemon::entity_names(&db).await.len(), 3);
    daemon::close(db).await;
}

#[tokio::test]
async fn flag_off_forget_ignores_a_malformed_leftover() {
    let fixture = Fixture::new();
    let path = fixture.memory_dir.join("forgotten.jsonl");
    std::fs::write(&path, b"{not json\n").expect("leftover");
    let db = fixture.open().await;
    seed_one_claim(&db).await;
    daemon::close(db).await;

    let report = correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    assert!(matches!(report, CorrectionReport::Removed { .. }));
    assert_eq!(std::fs::read(&path).expect("bytes"), b"{not json\n");
    assert!(!fixture.memory_dir.join(".forgotten.lock").exists());
}

#[tokio::test]
async fn a_confirmed_forget_does_not_rewrite_source_files() {
    let fixture = Fixture::new();
    enable_lasting(&fixture.memory_dir);
    let files = [
        "conversations/conversation-001.md",
        "EPHEMERAL.md",
        "MEMORY.md",
        "turns/conversation-001.json",
        "checkpoints/precompact.md",
        "graph.v1-backup-2026/KEEP",
    ];
    let mut before = Vec::new();
    for file in files {
        let path = fixture.memory_dir.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        let bytes = format!("raw value Vim in {file}\n");
        std::fs::write(&path, &bytes).expect("plant");
        before.push(bytes);
    }
    let db = fixture.open().await;
    seed_one_claim(&db).await;
    daemon::close(db).await;
    correct(
        &fixture.memory_dir,
        edge("D", "USES", "Vim"),
        Correction::Forget { confirmed: true },
    )
    .await;
    for (file, bytes) in files.iter().zip(before) {
        let path = fixture.memory_dir.join(file);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            bytes,
            "{file}"
        );
    }
}

#[test]
fn conversation_numbers_ignore_the_forget_lock_and_temps() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(dir.path().join("conversation-003.md"), "x").expect("archive");
    std::fs::write(dir.path().join(".forgotten.lock"), "").expect("lock");
    std::fs::write(dir.path().join(".forgotten.jsonl.9.9.tmp"), "temp").expect("temp");
    assert_eq!(
        recall_echo::archive::highest_conversation_number(dir.path()),
        3
    );
}
