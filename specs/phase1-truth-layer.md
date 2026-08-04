# Phase 1 — Truth Layer: Stateful, Provenance-Aware Confidence

**Status**: In progress
**Date**: 2026-08-04
**Parent roadmap**: `/opt/pulse-vault/pulse-null/specs/todo/recall-echo-v4-roadmap-spec.md` (Phase 1 of 5)
**Base**: stacked on Phase 0 (`feat/RE-29-phase0-unblock-measurement`) — daemon + runtime backend assumed present.
**Intellectual origin**: Echo's source-correlation research (2026-08-04, Bovens-Hartmann/Olsson: independence is a load-bearing precondition for corroboration; Dong et al.: model source copying, don't count copies) + rev-3 audit finding that the Beta-Binomial update is stateless.

## Problem

The confidence score is not what the essay says it is, in two independent ways:

1. **Stateless update.** `confidence.rs` re-derives α/β from the stored mean with concentration pinned at 10 — evidence counts never accumulate. The posterior after 50 corroborations is indistinguishable from after 5; variance never narrows. The essay describes accumulation that does not happen.
2. **Provenance-blind corroboration.** Every corroborating episode is counted as evidence regardless of who authored it. For an entity whose episodes are all written by one agent (Echo's case exactly), confidence gain is coherence amplification, not evidence — "well-corroborated" and "often-repeated-by-the-agent" are indistinguishable. Undetectable from inside; the only fix is provenance tagged at write-time.

Adjacent debts in the same layer: episodes have **no GC/decay at all** (unbounded accumulation, the exact failure class that killed Echo); the outcome-feedback tier (`record_outcome_feedback`) has **no caller** — utility contributes a constant to every score in every real deployment.

## Work items

### 1. Stateful evidence (schema + math)
- Persist per-edge evidence: `alpha: float`, `beta: float` pseudo-count fields on `relates_to` (schema migration, idempotent `DEFINE FIELD IF NOT EXISTS` + backfill from stored `confidence` at prior concentration — existing edges keep their mean, gain honest low concentration).
- `corroborate`/`contradict` increment the persisted counts (by provenance weight, item 2); `effective_confidence` = decayed posterior mean; posterior variance now available and exposed in `graph status`/entity detail output.
- Keep the two-clock design (7d hotness / 90d confidence half-life) unchanged. Decay applies to the *mean* at read time exactly as today.

### 2. Provenance classes at write-time
- Every episode and every confidence-moving event carries `provenance: external | user | self` (D's decision 2026-08-04: three classes; collapse is possible at scoring time, recovery is not).
  - `external` — ingested documents, web content, tool outputs.
  - `user` — statements authored by the human (D) in conversation.
  - `self` — the agent's own summaries, reflections, re-assertions (includes everything the archive pipeline generates from assistant turns).
- Class inference at ingest: conversation-turn role → user/self; non-conversation ingest sources (pipeline docs authored by the entity) → self; explicit override flag for genuinely external material. Default when unknown: `self` (the conservative choice — never over-credit).
- Evidence weights in config `[graph.provenance]`: `weight_external = 1.0`, `weight_user = 0.8`, `weight_self = 0.05` (tunable; benchmark decides later). Corroboration increments α by the weight; contradiction increments β by the weight.
- Coherence (self-corroboration) remains **visible as a separate signal** — a `self_reinforcements` counter on the edge — never laundered into confidence.

### 3. Episode GC
- Episodes gain the same governance edges have: read-time relevance decay already exists via hotness; add GC (dry-run default, thresholds config-gated) pruning episodes that are old + never-retrieved + not evidence for any surviving edge. Wire into existing `graph gc`.

### 4. Wire the outcome-feedback tier
- `recall-echo graph feedback <session-id> --outcome success|failure` CLI verb calling `record_outcome_feedback`; hook-side: SessionEnd archive records the session→entity `contributed_to` edges it already knows about (was-used signal), so utility learns passively.

## Acceptance criteria

### Happy path
- [ ] AC1: Corroborating an edge N times with `external` provenance yields a posterior whose variance strictly decreases with N (test: var(α,β) after 5 < after 1; after 50 < after 5) and whose α+β grows by the configured weights — persisted across process restarts.
- [ ] AC2: The same N corroborations with `self` provenance move the mean by ≤ the configured self-weight fraction; the `self_reinforcements` counter records them; an external contradiction outweighs any number of accumulated self-corroborations at default weights.
- [ ] AC3: Schema migration is idempotent and non-destructive: opening a pre-Phase-1 store preserves every stored confidence mean; re-opening migrates nothing twice.
- [ ] AC4: `graph gc` (episodes mode) in dry-run reports prune candidates; real run removes only old + never-retrieved + non-evidence episodes; evidence episodes survive regardless of age.
- [ ] AC5: `graph feedback` moves utility scores (visible in entity detail before/after); SessionEnd hook records was-used edges without user action.
- [ ] AC6: Full benchmark suite re-run shows no regression vs the Phase 0 baseline (retrieval metrics within noise; answer metrics not degraded).

### Edge cases
- [ ] AC7: Unknown/missing provenance on legacy episodes defaults to `self` — legacy stores never gain confidence from backfilled data.
- [ ] AC8: Weights set to (1.0, 1.0, 1.0) reproduce provenance-blind behavior exactly (escape hatch + differential-testing lever).
- [ ] AC9: Golden-set query tests (Phase 0) still pass unmodified — retrieval semantics untouched by this phase.

### Failure modes
- [ ] AC10: Migration interrupted mid-backfill (kill -9) leaves a store that re-opens and completes migration — no corruption, no double-count (crash-only discipline extends to migration).

## Out of scope

Retrieval changes (Phase 2 — including the three retrieval bugs from the golden-set findings), MCP (Phase 3), essay publication (Phase 4 — but this phase makes the essay true, and the provenance addendum draws directly on Echo's research, credited).
