// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! E9 — provenance-weighting ablation, store round-trip (Variant B).
//!
//! Pre-registration: <https://github.com/dnacenta/recall-echo/issues/79>.
//!
//! Replays the false claim through the write paths the shipped code uses,
//! against a throwaway embedded store, and checks that what is persisted after
//! every write equals an in-memory replica of the arithmetic:
//!
//! - the edge is created by `add_relationship` at the `Inferred` prior, the
//!   way extraction creates one;
//! - each self-assertion is folded in the way `record_reextraction` does it:
//!   read the edge, `corroborate(SelfGenerated, gm.provenance_weights())`,
//!   `reinforce_relationship`;
//! - the contradiction goes through `GraphMemory::correct` (`--wrong`), the only
//!   contradiction path there is, at `Provenance::User`.
//!
//! `weight_self` reaches the store through a `.recall-echo.toml` written next
//! to it, so the config path is exercised too. The store lives in a fresh
//! `tempfile` directory and is deleted on exit; this never opens an existing
//! store. The embedding model `add_entity` needs is copied in from a local
//! FastEmbed cache so nothing is downloaded.
//!
//! ```text
//! cargo run --example e9_store_roundtrip -- <fastembed-models-dir>
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use recall_echo::graph::confidence::{
    EdgeEvidence, Evidence, ExtractionContext, Provenance, ProvenanceWeights,
};
use recall_echo::graph::correct::{CorrectTarget, Correction, CorrectionReport};
use recall_echo::graph::types::{Direction, EntityType, NewEntity, NewRelationship, Relationship};
use recall_echo::graph::GraphMemory;

const WEIGHTS_SELF: [f64; 2] = [0.05, 1.0];
const CLAIM_FROM: &str = "recall-echo";
const CLAIM_TO: &str = "PostgreSQL";
const CLAIM_TYPE: &str = "uses";
const EMBEDDING_MODEL_DIR: &str = "models--Xenova--bge-small-en-v1.5";
const TOLERANCE: f64 = 1e-9;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    SelfAssertion,
    UserCorrection,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::SelfAssertion => "self_assertion",
            Self::UserCorrection => "user_correction",
        }
    }
}

/// S^10 → C_user → S^30, as pre-registered.
fn sequence() -> Vec<Step> {
    let restate = |count| std::iter::repeat_n(Step::SelfAssertion, count);
    restate(10)
        .chain([Step::UserCorrection])
        .chain(restate(30))
        .collect()
}

/// A throwaway memory dir: config beside the graph, models copied in.
struct TempStore {
    _root: tempfile::TempDir,
    graph_path: PathBuf,
}

impl TempStore {
    fn create(weight_self: f64, models_source: &Path) -> Result<Self> {
        let root = tempfile::Builder::new().prefix("e9-store-").tempdir()?;
        std::fs::write(
            root.path().join(".recall-echo.toml"),
            format!("[graph]\n\n[graph.provenance]\nweight_self = {weight_self}\n"),
        )?;
        let graph_path = root.path().join("graph");
        copy_tree(models_source, &graph_path.join("models"))?;
        Ok(Self {
            _root: root,
            graph_path,
        })
    }
}

/// Copy a FastEmbed (Hugging Face layout) cache, keeping its relative
/// symlinks as symlinks so blobs are not duplicated.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            copy_symlink(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(link: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(link)?, target)?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(link: &Path, target: &Path) -> Result<()> {
    std::fs::copy(link, target)?;
    Ok(())
}

fn entity(name: &str, entity_type: EntityType, abstract_text: &str) -> NewEntity {
    NewEntity {
        name: name.to_string(),
        entity_type,
        abstract_text: abstract_text.to_string(),
        overview: None,
        content: None,
        attributes: None,
        source: Some("e9".to_string()),
    }
}

/// The one live edge carrying the false claim.
async fn claim_edge(gm: &GraphMemory) -> Result<Relationship> {
    let mut live: Vec<Relationship> = gm
        .get_relationships(CLAIM_FROM, Direction::Outgoing)
        .await?
        .into_iter()
        .filter(|edge| edge.rel_type.eq_ignore_ascii_case(CLAIM_TYPE) && edge.valid_until.is_none())
        .collect();
    match (live.pop(), live.is_empty()) {
        (Some(edge), true) => Ok(edge),
        _ => Err("expected exactly one live claim edge".into()),
    }
}

async fn self_assert(gm: &GraphMemory) -> Result<()> {
    let stored = claim_edge(gm).await?;
    let mut evidence = stored.edge_evidence();
    evidence.corroborate(Provenance::SelfGenerated, gm.provenance_weights());
    gm.reinforce_relationship(&stored.id_string(), evidence)
        .await?;
    Ok(())
}

async fn user_correction(gm: &GraphMemory) -> Result<()> {
    let target = CorrectTarget::Edge {
        from: CLAIM_FROM.to_string(),
        rel_type: CLAIM_TYPE.to_string(),
        to: CLAIM_TO.to_string(),
    };
    match gm
        .correct(&target, Correction::Wrong { all_edges: false })
        .await?
    {
        CorrectionReport::Contradicted { edges } if edges.len() == 1 => Ok(()),
        other => Err(format!("correction was not applied to one edge: {other:?}").into()),
    }
}

/// Largest disagreement between the persisted edge and the replica.
fn round_trip_error(stored: &Relationship, replica: EdgeEvidence) -> f64 {
    let counts = replica.evidence();
    let tally_error = (stored.self_reinforcements.unwrap_or(-1) - replica.self_reinforcements())
        .unsigned_abs() as f64;
    [
        (stored.confidence - counts.mean()).abs(),
        (stored.alpha.unwrap_or(f64::NAN) - counts.alpha()).abs(),
        (stored.beta.unwrap_or(f64::NAN) - counts.beta()).abs(),
        tally_error,
    ]
    .into_iter()
    .fold(0.0, |worst, error| {
        if error.is_nan() {
            f64::INFINITY
        } else {
            worst.max(error)
        }
    })
}

fn print_row(
    weight_self: f64,
    step: usize,
    observation: &str,
    stored: &Relationship,
    replica: EdgeEvidence,
) -> f64 {
    let error = round_trip_error(stored, replica);
    println!(
        "{weight_self},{step},{observation},{:.10},{:.10},{:.6},{:.6},{},{},{error:e}",
        stored.confidence,
        replica.evidence().mean(),
        stored.alpha.unwrap_or(f64::NAN),
        stored.beta.unwrap_or(f64::NAN),
        stored.self_reinforcements.unwrap_or(-1),
        replica.self_reinforcements(),
    );
    error
}

/// Run the sequence at one `weight_self`; returns the worst round-trip error.
async fn run(weight_self: f64, models_source: &Path) -> Result<f64> {
    let store = TempStore::create(weight_self, models_source)?;
    let gm = GraphMemory::open_embedded(&store.graph_path).await?;

    let requested = ProvenanceWeights {
        weight_self,
        ..ProvenanceWeights::default()
    };
    if *gm.provenance_weights() != requested {
        return Err(format!(
            "store loaded {:?}, config asked for {requested:?}",
            gm.provenance_weights()
        )
        .into());
    }

    gm.add_entity(entity(
        CLAIM_FROM,
        EntityType::Project,
        "Persistent memory for LLM agents",
    ))
    .await?;
    gm.add_entity(entity(CLAIM_TO, EntityType::Tool, "A relational database"))
        .await?;
    let prior = ExtractionContext::Inferred.prior() as f32;
    gm.add_relationship(NewRelationship {
        from_entity: CLAIM_FROM.to_string(),
        to_entity: CLAIM_TO.to_string(),
        rel_type: CLAIM_TYPE.to_string(),
        description: Some("false claim restated by the agent (E9)".to_string()),
        confidence: Some(prior),
        source: Some("e9".to_string()),
    })
    .await?;

    let mut replica = EdgeEvidence::new(Evidence::from_prior(f64::from(prior)), 0);
    let mut worst = print_row(weight_self, 0, "created", &claim_edge(&gm).await?, replica);

    for (index, step) in sequence().into_iter().enumerate() {
        match step {
            Step::SelfAssertion => {
                self_assert(&gm).await?;
                replica.corroborate(Provenance::SelfGenerated, &requested);
            }
            Step::UserCorrection => {
                user_correction(&gm).await?;
                replica.contradict(Provenance::User, &requested);
            }
        }
        let stored = claim_edge(&gm).await?;
        worst = worst.max(print_row(
            weight_self,
            index + 1,
            step.label(),
            &stored,
            replica,
        ));
    }
    Ok(worst)
}

fn models_source() -> Result<PathBuf> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: e9_store_roundtrip <fastembed-models-dir>")?;
    if !dir.join(EMBEDDING_MODEL_DIR).is_dir() {
        return Err(format!(
            "{} has no {EMBEDDING_MODEL_DIR}; refusing to download a model",
            dir.display()
        )
        .into());
    }
    Ok(dir)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let models = match models_source() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "weight_self,step,observation,stored_confidence,replica_mean,stored_alpha,stored_beta,\
         stored_self_reinforcements,replica_self_reinforcements,max_abs_error"
    );
    let mut passed = true;
    for weight_self in WEIGHTS_SELF {
        match run(weight_self, &models).await {
            Ok(worst) => {
                let pass = worst <= TOLERANCE;
                eprintln!(
                    "H5 weight_self={weight_self}: max round-trip error {worst:e}, pass={pass}"
                );
                passed &= pass;
            }
            Err(error) => {
                eprintln!("H5 weight_self={weight_self}: {error}");
                passed = false;
            }
        }
    }
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
