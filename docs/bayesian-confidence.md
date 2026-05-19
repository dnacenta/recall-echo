# Bayesian Confidence for Agent Memory Graphs

How `recall-echo` treats every relationship in its knowledge graph as a probability distribution, why that matters for long-running autonomous agents, and how it differs from every other open-source memory system currently in the space.

## TL;DR

Most agent-memory systems treat the relationship between two facts as a boolean with a timestamp. `recall-echo` treats it as a Beta distribution. Each edge in the graph carries a calibrated confidence in `[0, 1]` that is updated with a Beta-Binomial conjugate prior on every new observation, then decayed over time with a 90-day half-life at read time. Multi-hop traversal multiplies confidence along the path, so weak chains attenuate automatically without an explicit pruning rule. Outcome feedback flows back through an EMA on per-entity utility scores. A linear combination of semantic similarity, hotness, and utility produces the final retrieval score.

The implementation is a few hundred lines of Rust in `src/graph/confidence.rs`, `src/graph/utility.rs`, `src/graph/search.rs`, and `src/graph/query.rs`. The novelty is not the math — Beta-Binomial is textbook — it is that, as far as I can tell, no other open-source agent-memory system applies it to graph edges.

## Why edge updates matter

An autonomous agent that runs for weeks or months hits the same recurring problem: yesterday's confidently-stated fact is today's stale lie, and the system has no principled way to know which is which. The two dominant patterns in the open-source agent-memory space both fall short here.

**Deterministic timestamps with bi-temporal invalidation** (Graphiti's approach) capture *when* something was true but not *how much* the system should trust it. A relationship that the agent observed once last Tuesday is treated identically to one it has heard nine times across nine independent conversations. Contradiction is binary — either you invalidate the old edge and write a new one, or you don't.

**ADD-only accumulation with a conflict detector** (mem0's approach) sidesteps merge entirely. New observations get appended; conflicts get flagged. This works for short-horizon agents, but for an entity running for months the accumulated noise grows unbounded, and "is this still true?" becomes a query the system cannot answer.

**Full graph rewrites** (Cognee's approach) regenerate the structure when contradictions accumulate. This loses history.

What all three miss: facts decay at different rates, observations have different trustworthiness, and the right merge of "stated three times, contradicted once" is not a deletion or an append — it is a posterior.

## The model — three primitives

### 1. Beta-Binomial confidence on edges

Every relationship starts with a prior determined by how it was extracted, defined in `src/graph/confidence.rs:13-33`:

```
Authoritative → 1.0
Explicit      → 0.9
Inferred      → 0.6
Speculative   → 0.3
```

The pseudocount is fixed at 10 (`confidence.rs:10`). This is the only knob that shapes how fast the posterior moves; everything else falls out of conjugacy.

The stored confidence `c` is interpreted as the mean of a Beta distribution with concentration `PSEUDOCOUNT = 10`:

```
α = c · 10
β = 10 − α
```

When a new observation arrives, we add one success (corroboration) or one failure (contradiction):

```
corroborate:  c' = (α + 1) / (α + β + 1) = (α + 1) / 11
contradict:   c' =       α / (α + β + 1) =       α / 11
```

The full implementation is six lines (`confidence.rs:57-66`).

**Worked example.** Take a fact extracted as `Inferred`, so the prior is 0.6 (α=6, β=4).

| Step | Event | α | β | Posterior |
|------|-------|---|---|-----------|
| 0 | prior (Inferred) | 6 | 4 | 0.600 |
| 1 | corroborate | 7 | 4 | 0.636 |
| 2 | corroborate | 8 | 4 | 0.667 |
| 3 | corroborate | 9 | 4 | 0.692 |
| 4 | contradict | 9 | 5 | 0.643 |

Three statements followed by one contradiction land at 0.643 — meaningfully above the prior, well below "certain". This is the property we want. A single contradictory mention does not erase three corroborations, but it does register. Tests in `confidence.rs:139-172` lock the math.

### 2. Temporal decay layered on top

The Bayesian posterior is what's stored. Effective confidence at read time also accounts for staleness:

```
effective = stored × 0.5^(days_since_reinforced / 90)
```

implemented in `confidence.rs:85-97`, floored at `DECAY_FLOOR = 0.05` (`confidence.rs:73`). Decay is computed against `last_reinforced` if present, otherwise against `valid_from` (`confidence.rs:102-119`). A corroborating update calls `reinforce_relationship` (see `mod.rs:259-265`), which resets the decay clock — so a fact that keeps getting confirmed never decays.

Why layer decay on top of the posterior rather than baking it into the update? Two reasons:

1. **Decoupling.** The posterior captures *evidence*; decay captures *recency*. A fact stated nine times two years ago and a fact stated three times yesterday are different things, and the system should be able to tell them apart. Mixing the two into one update loses information.
2. **Cheap reads, cheap writes.** Decay is computed at query time from the timestamp. No background job has to walk the graph and re-score edges every night.

The floor at 0.05 is a deliberate choice: edges never disappear through decay alone. They become low-priority. Garbage collection is a separate concern handled in `src/graph/gc.rs`.

### 3. Path confidence as the edge product

Multi-hop traversal compounds confidence multiplicatively (`confidence.rs:127-129`):

```
path_confidence([0.8, 0.7, 0.9]) = 0.504
```

This falls naturally out of treating edges as independent probabilities. The practical effect is that the long tail of weak chains is automatically suppressed. Two hops over 0.9 edges is a stronger signal than four hops over 0.7 edges (`0.81` vs `0.24`) — no special-case pruning needed.

The hybrid query in `src/graph/query.rs:51-97` applies this directly: after the semantic phase identifies the top candidates, graph expansion adds neighbors scored as `parent_score · effective_confidence` (line 83). The effective confidence is computed with temporal decay at read time (`query.rs:158-163`), and edges below 0.1 effective confidence are filtered out (`query.rs:166-168`).

```
  semantic                graph expansion
  ─────────               ──────────────
    [E1: 0.84] ──[0.9]──→ [N1: 0.756]
       │                      │
       └───────[0.6]───────→  [N2: 0.504]   ← scored, not pruned
                                            ← attenuates over hops
    [E2: 0.71] ──[0.4]──→ [N3: 0.284]       ← below filter floor
                                              after second hop
```

## Adaptive utility scoring

The Bayesian model handles *epistemic* uncertainty about whether a relationship holds. A separate layer handles *instrumental* utility: did this entity, when retrieved, actually help the agent succeed?

`src/graph/utility.rs` implements an exponential moving average over per-entity utility scores. Outcomes have rewards (`utility.rs:23-33`):

```
Success → 1.0
Partial → 0.5
Failed  → 0.0
```

When a session completes, retrieved entities receive an EMA update. The alpha depends on whether the entity was actually used in the response (`utility.rs:62-68`):

```
USED_ALPHA   = 0.1     // retrieved AND used → full signal
UNUSED_ALPHA = 0.05    // retrieved but not used → muted
UNUSED_REWARD = 0.3    // "you retrieved me and ignored me" → slight negative
```

The update is `new = (1 − α) · current + α · reward`, applied atomically in a single SurrealDB query to avoid read-modify-write races (`utility.rs:239-263`). The convergence test (`utility.rs:342-354`) confirms that ~50 successive successes push a score from 0.5 to >0.99.

These utility scores feed the final retrieval composition in `src/graph/search.rs:226-235`:

```
final_score = w_semantic · similarity
            + w_hotness  · hotness
            + w_utility  · utility_score
```

Default weights are `0.45 / 0.30 / 0.25` (`search.rs:22-23`), configurable per deployment via `[graph.scoring]` in `.recall-echo.toml`. Hotness itself is a separate signal — `sigmoid(ln(1 + access_count)) · exp(−ln(2)/7 · days)` — capturing recent activity with a 7-day half-life (`search.rs:239-253`).

So the system has two decay clocks operating on different signals at different rates: 7 days for hotness (engagement), 90 days for confidence (epistemic decay). Both are tunable.

## Comparison to the landscape

| System | Edge model | Contradiction handling | Stale-fact handling | Reinforcement |
|---|---|---|---|---|
| Graphiti | Bi-temporal validity intervals | Invalidate + new edge | Explicit `valid_to` | None — observation count not tracked |
| mem0 | ADD-only with conflict tags | Flagged, manual resolve | None | Not represented |
| Cognee | Graph rewrite on conflict | Regenerate region | Implicit via rewrite | Lost on rewrite |
| recall-echo | Beta-Binomial posterior | Single contradiction shifts posterior, multiple erode it | Half-life decay at read time | α += 1 + clock reset |

A few honest notes.

**Graphiti's bi-temporal model gives you something Bayesian confidence does not: point-in-time queries.** "What did this agent believe about X on March 14?" is a clean SQL-like query against `valid_from`/`valid_to`. Against a Beta-Binomial graph, the equivalent question is "what was the posterior at that timestamp?" — answerable only if you log every update, which `recall-echo` does not currently do. If point-in-time auditability matters more than calibrated uncertainty for your use case, bi-temporal is the right design.

**mem0's strength is operational simplicity.** No conjugate priors, no half-lives. For short-horizon agents that won't accumulate enough observations for the math to matter, that simplicity is the correct call.

**Cognee's rewrite approach is the most aggressive about consistency** but trades history for it. For an agent whose memory needs to support "why did you think X?" introspection, that's a hard trade.

The case for probabilistic edges is specifically about long-running autonomous entities — systems whose memory is being shaped by hundreds or thousands of observations over months, where the noise floor is high and "this is still probably true" is a useful concept. That is the design center for `recall-echo`.

## Implementation notes

The whole confidence model is ~130 lines of Rust including tests (`src/graph/confidence.rs`). The utility layer is ~365 lines including the SurrealDB feedback edges (`src/graph/utility.rs`). It runs on:

- **SurrealDB** (embedded SurrealKV by default, WebSocket server optional) as the graph store and HNSW vector index
- **FastEmbed** for local 384-dimension cosine embeddings
- **Tokio** for async, with `futures::future::join_all` for concurrent per-entity feedback updates (`utility.rs:134`)
- **AGPL-3.0**, version 3.11.0 at time of writing

A few design choices worth pulling out:

- `PSEUDOCOUNT = 10` was chosen so that ~10 observations are needed to fully overwhelm the prior. Lower values make the posterior twitchy; higher values make corroboration too slow to register. Ten is the conventional weak-prior choice and it tested well in practice.
- Half-life of 90 days is a default; the constant lives at `confidence.rs:70` and the function signature accepts a per-call value, so per-domain tuning is mechanical.
- The decay floor at 0.05 is deliberate. Edges below it are still queryable but ranked near the bottom. Hard deletion is a GC concern.
- The hybrid query filters effective confidence below 0.1 during graph expansion (`query.rs:166`). Below this, the multiplicative chain attenuates results to noise.

## Reproducibility

Source: [`github.com/dnacenta/recall-echo`](https://github.com/dnacenta/recall-echo), AGPL-3.0.

Read in this order:

- `src/graph/confidence.rs` — the Bayesian update, decay, and path composition (~300 lines including tests)
- `src/graph/utility.rs` — EMA feedback loop and outcome edges
- `src/graph/search.rs` — final scoring composition
- `src/graph/query.rs` — hybrid query with confidence-weighted graph expansion

The test suite in `confidence.rs:131-297` covers the update math, the decay function, the floor, and the path product. The utility tests in `utility.rs:301-365` cover EMA math, convergence behavior, and the used-vs-unused asymmetry. Run `cargo test -p recall-echo graph::confidence` and `cargo test -p recall-echo graph::utility` to reproduce.

## Open questions

A few directions that would extend this naturally:

**Learned per-relation-type priors.** The four extraction-context priors (`Authoritative` / `Explicit` / `Inferred` / `Speculative`) are hand-tuned constants. A more honest approach would track the calibration of each context over time — what fraction of "Speculative" claims actually held up under corroboration — and adjust the prior accordingly. This is straightforward Bayesian model-averaging and is currently future work.

**Integration with bi-temporal validity intervals.** The two approaches are not actually in conflict. A Beta-Binomial posterior could be stored alongside `valid_from`/`valid_to` intervals to get both point-in-time queries *and* calibrated uncertainty. The cost is schema complexity. Whether it pays off depends on whether anyone's agent actually issues point-in-time queries — for `recall-echo`'s current use, they don't.

**Per-observation logs.** Storing every update event would enable replay, calibration analysis, and rollback. It would also bloat the graph considerably. The current design treats the posterior as sufficient.

---

`recall-echo` exists because the entity it serves — `pulse-null`, a long-running autonomous Rust runtime — needed a memory layer that wouldn't degrade into noise after a few months of operation. The Bayesian model is the part that earns its keep. The rest is plumbing.
