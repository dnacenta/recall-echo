// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Grade graph writes against a scripted fact history.
//!
//! A fact script becomes conversation archives in the shape ingestion already
//! reads. Those archives are ingested into a store, and the resulting edges
//! are diffed against the script: each claim should have landed as a create,
//! a corroboration, a supersede, a contradiction, or a no-op. Precision and
//! recall are reported per operation, along with wrong facts, junk edges,
//! provider calls and time per session.
//!
//! The scripted extractor replays the claims, so the run makes no provider
//! call. Loading the embedding model is separate and can download on a fresh
//! store. Callers that pass a live provider exercise the same grader.

mod grade;
mod run;
mod script;

pub use grade::{
    grade, ExtractorLabel, JunkEdge, ObservedEdge, OpScore, OpTable, PatternBucket, PatternScore,
    QuestionScore, SessionObservation, SessionReport, StepLog, WriteOp, WriteReport, WrongFact,
    WrongReason,
};
pub use run::{
    require_scratch, run, run_repeat, Extractor, KnownFactsContext, MetricRange, MetricSet,
    MetricSpan, RepeatReport,
};
pub use script::{
    load_script, parse_script, Claim, ClaimOp, DerivedQuestion, EndProvenance, EndState,
    ExpectedFact, FactPattern, FactScript, NoiseItem, ScriptFact, ScriptSession, ScriptTurn,
    Speaker, SupersededPair,
};
