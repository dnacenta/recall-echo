# Spec: Bayesian Confidence for Knowledge Graph

**Status**: Draft
**Scope**: recall-echo v3.4.0
**Author**: Echo + Synth

## Problem

The knowledge graph stores a `confidence` field on every relationship but never uses it. All relationships default to 1.0 regardless of how they were established. Graph traversal treats every edge equally. Graph expansion uses a hardcoded `parent_score * 0.5` decay factor. The system cannot distinguish high-certainty knowledge from speculation.

## Goal

Make confidence meaningful. Relationships get an evidence-based confidence score that updates over time through Bayesian inference. Traversal, expansion, and display all use confidence to surface trustworthy connections and suppress noisy ones.

## Non-Goals

- Entity-level confidence (Phase 2, separate spec)
- Probabilistic dedup (Phase 3, separate spec)
- Adaptive retrieval scoring weights (Phase 3)
- Adaptive per-type decay rates (Phase 3)

## Current State

| Component | File | Status |
|-----------|------|--------|
| `Relationship.confidence: f64` | `types.rs:155` | Exists, always 1.0, never read |
| `NewRelationship.confidence: Option<f32>` | `types.rs:139` | Exists, always None |
| Schema: `confidence ON relates_to TYPE float DEFAULT 1.0` | `store.rs:57` | Exists |
| `crud.rs` bind: `rel.confidence.unwrap_or(1.0)` | `crud.rs:201` | Defaults to 1.0 |
| Ingestion: `confidence: None` | `ingest.rs:173` | Always passes None |
| Traversal SQL | `traverse.rs:72-84,102-114` | Does not SELECT confidence |
| `TraversalEdge` | `types.rs:305-312` | No confidence field |
| `EdgeRow` | `types.rs:315-321` | No confidence field |
| Graph expansion scoring | `query.rs:79` | `parent_score * 0.5` (hardcoded) |
| `get_neighbor_details` return | `query.rs:118` | `Vec<(EntityDetail, String)>` — no confidence |
| `RelTarget` | `query.rs:164-168` | No confidence field |

## Design

### 1. Bayesian Update Model

Use Beta-Binomial conjugate prior for binary evidence (corroborate/contradict).

**Parameters:**
- Pseudocount total: 10 (moderately informative prior; ~10 observations to overwhelm it)
- Corroboration: alpha += 1
- Contradiction: beta += 1
- Posterior: alpha / (alpha + beta)

**Behavior at pseudocount = 10:**

| Current | Corroborate | Contradict |
|---------|-------------|------------|
| 0.9 | 0.909 | 0.818 |
| 0.6 | 0.636 | 0.545 |
| 0.3 | 0.364 | 0.273 |

Confidence moves slowly per observation but accumulates with repeated evidence. This is intentional — knowledge graphs should be stable.

**Path confidence:** Product of edge confidences along a path. A 2-hop path through 0.8 and 0.7 edges gives 0.56.

### 2. Extraction Context

Classify how a relationship was established to set the initial prior:

| Context | Prior | When |
|---------|-------|------|
| `Authoritative` | 1.0 | Pipeline sync, manual creation, direct user input |
| `Explicit` | 0.9 | Direct statement in conversation ("I use Rust") |
| `Inferred` | 0.6 | Implied by context (entity co-occurrence, discussed together) |
| `Speculative` | 0.3 | Possible connection based on domain knowledge |

The LLM extraction prompt will be updated to classify each relationship. Default when absent or unparseable: `Inferred` (0.6).

### 3. Re-extraction as Evidence

Currently, when a relationship is re-extracted from a new conversation, it is silently skipped (`ingest.rs:163-165`). Instead:

- Find the existing relationship
- Apply `bayesian_update(existing.confidence, true)` (corroboration)
- Write the updated confidence back

This means relationships that appear across multiple conversations gain confidence over time.

### 4. Confidence in Traversal

Traversal SQL queries add `confidence` to SELECT. Edges below 0.1 confidence are filtered out (suppresses garbage, not selective).

`TraversalEdge` gains a `confidence: f64` field. `format_traversal` displays `[85%]` on edges where confidence < 1.0. Authoritative edges (1.0) show clean.

### 5. Confidence in Graph Expansion

Replace `parent_score * 0.5` with `parent_score * confidence`.

A high-confidence edge (0.9) propagates nearly the full parent score. A speculative edge (0.3) deeply discounts the neighbor. This is the highest-impact single change.

`RelTarget` gains `confidence: f64`. `get_neighbor_details` returns `Vec<(EntityDetail, String, f64)>`.

## API Changes

### New Public Types (types.rs)

```rust
/// How a relationship was established — determines initial confidence prior.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionContext {
    Explicit,       // 0.9
    Inferred,       // 0.6
    Speculative,    // 0.3
    Authoritative,  // 1.0
}
```

### New Public Functions (confidence.rs)

```rust
/// Bayesian update using Beta-Binomial conjugate.
pub fn bayesian_update(current_confidence: f64, corroborate: bool) -> f64

/// Compound confidence along a multi-hop path.
pub fn path_confidence(edge_confidences: &[f64]) -> f64
```

### New GraphMemory Method (mod.rs)

```rust
/// Update relationship confidence (Bayesian posterior).
pub async fn update_relationship_confidence(
    &self,
    rel_id: &str,
    confidence: f64,
) -> Result<(), GraphError>
```

### Modified Types

```rust
// types.rs — TraversalEdge gains:
pub confidence: f64,

// types.rs — EdgeRow gains:
pub confidence: f64,

// types.rs — ExtractedRelationship gains:
#[serde(default)]
pub confidence: Option<String>,  // "explicit" | "inferred" | "speculative"
```

### Unchanged (Backwards Compatible)

- `NewRelationship.confidence: Option<f32>` — unchanged. Callers passing `None` still get schema default 1.0.
- `Relationship.confidence: f64` — already exists, unchanged.
- All existing `GraphMemory` methods — signatures unchanged.

## Extraction Prompt Change

Add to the relationship schema in the extraction prompt:

```json
{
  "source": "Source Entity Name",
  "target": "Target Entity Name",
  "rel_type": "USES|BUILDS|...",
  "description": "Why this relationship exists",
  "confidence": "explicit|inferred|speculative"
}
```

Add to extraction rules:

```
- Classify relationship confidence:
  - explicit: Directly stated ("I use Rust", "this depends on X")
  - inferred: Implied by context (discussed together, co-occurring)
  - speculative: Possible connection based on domain knowledge
  - When unsure, use "inferred"
```

## Files Changed

| File | Change |
|------|--------|
| `confidence.rs` | **New** — `bayesian_update()`, `path_confidence()`, unit tests |
| `types.rs` | `ExtractionContext` enum, `confidence` on `EdgeRow`/`TraversalEdge`/`ExtractedRelationship` |
| `crud.rs` | `update_relationship_confidence()` function |
| `mod.rs` | Register `confidence` module, expose method on `GraphMemory` |
| `traverse.rs` | SQL adds `confidence` to SELECT + `>= 0.1` filter, `collect_edges` passes it, `format_traversal` displays it |
| `query.rs` | `RelTarget` adds `confidence`, `get_neighbor_details` returns it, expansion uses `parent_score * confidence` |
| `extract.rs` | Prompt adds `confidence` field to relationship schema |
| `ingest.rs` | New relationships get `ExtractionContext` prior, re-extracted relationships get Bayesian update |

## Versioning

Bump to **3.4.0**. Adding `confidence` to `TraversalEdge` is a minor breaking change for callers that construct the struct directly (not typical usage).

## Testing

1. Unit tests in `confidence.rs`:
   - `bayesian_update(0.6, true)` ≈ 0.636
   - `bayesian_update(0.6, false)` ≈ 0.545
   - `bayesian_update(0.9, true)` ≈ 0.909
   - `path_confidence(&[0.8, 0.7])` ≈ 0.56
   - `path_confidence(&[])` = 1.0
   - `ExtractionContext::prior()` values correct

2. Existing tests pass (pipeline_sync, normalize_key, entity_needs_update)

3. Build with `cargo build --features graph`

4. Manual verification:
   - `recall-echo graph traverse <entity>` shows confidence percentages
   - `recall-echo graph status` still works
   - Graph expansion favors high-confidence edges
