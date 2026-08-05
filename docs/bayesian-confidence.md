# Bayesian Confidence for Agent Memory Graphs

How `recall-echo` treats every relationship in its knowledge graph as a probability distribution, why that matters for long-running autonomous agents, and how it differs from every other open-source memory system currently in the space.

## TL;DR

Most agent-memory systems treat the relationship between two facts as a boolean with a timestamp. `recall-echo` treats it as a Beta distribution whose pseudo-counts are **persisted on the edge**. Each relationship carries `alpha` (corroboration) and `beta` (contradiction) in the store, so evidence genuinely accumulates: the posterior after fifty corroborations is a different distribution from the posterior after five, with the same mean and a smaller variance. The stored `confidence` field is the posterior mean, kept in sync with the counts on every write so read paths can keep scoring on a single scalar.

Two things move those counts. First, observations: a re-extraction that agrees with an edge adds weight to α, one that disagrees adds weight to β. Second — and this is the part nobody else ships — **provenance**. Every observation is stamped at write time with who authored the text it came from (external / user / self), and the weight it contributes depends on that class. The agent restating its own belief adds 0.05, not 1.0, and is separately tallied in `self_reinforcements` so coherence never passes for evidence.

On top of the posterior sits temporal decay with a 90-day half-life, computed at read time. Multi-hop traversal multiplies confidence along the path, so weak chains attenuate without an explicit pruning rule. Outcome feedback flows through an EMA on per-entity utility scores, and a linear combination of semantic similarity, hotness, and utility produces the final retrieval score.

The implementation is `src/graph/confidence.rs` (the model), `src/graph/store.rs` (schema and migration), `src/graph/crud.rs` and `src/graph/ingest.rs` (the write paths), `src/graph/utility.rs` (the feedback loop), `src/graph/search.rs` and `src/graph/query.rs` (the read paths). The novelty is not the math — Beta-Binomial is textbook — it is that, as far as I can tell, no other open-source agent-memory system applies accumulating evidence to graph edges, and none of them weight that evidence by source independence.

## Why edge updates matter

An autonomous agent that runs for weeks or months hits the same recurring problem: yesterday's confidently-stated fact is today's stale lie, and the system has no principled way to know which is which. The two dominant patterns in the open-source agent-memory space both fall short here.

**Deterministic timestamps with bi-temporal invalidation** (Graphiti's approach) capture *when* something was true but not *how much* the system should trust it. A relationship that the agent observed once last Tuesday is treated identically to one it has heard nine times across nine independent conversations. Contradiction is binary — either you invalidate the old edge and write a new one, or you don't.

**ADD-only accumulation with a conflict detector** (mem0's approach) sidesteps merge entirely. New observations get appended; conflicts get flagged. This works for short-horizon agents, but for an entity running for months the accumulated noise grows unbounded, and "is this still true?" becomes a query the system cannot answer.

**Full graph rewrites** (Cognee's approach) regenerate the structure when contradictions accumulate. This loses history.

What all three miss: facts decay at different rates, observations have different trustworthiness, and the right merge of "stated three times, contradicted once" is not a deletion or an append — it is a posterior.

Worth saying plainly, because an earlier version of this document overstated it: `recall-echo` used to miss something too. The confidence field was a mean with no memory. Every update re-derived α and β *from the stored mean* at a fixed concentration of 10, applied one observation, and threw the counts away. The mean moved correctly; the *amount of evidence behind it* was pinned at ten forever. An edge confirmed fifty times and an edge confirmed once were, to the system, the same object. Phase 1 is the fix, and everything below describes the fixed version.

## The model — three primitives

### 1. Beta evidence that accumulates

Every relationship starts with a prior determined by how it was extracted (`confidence.rs:47-58`):

```
Authoritative → 1.0
Explicit      → 0.9
Inferred      → 0.6
Speculative   → 0.3
```

That prior is a *mean*. It is turned into counts by `Evidence::from_prior` (`confidence.rs:218-225`), which splits the prior concentration between them so the mean is preserved exactly:

```
α = mean · C
β = (1 − mean) · C        where C = PRIOR_CONCENTRATION = 10   (confidence.rs:18)
```

From then on the counts are what the edge stores, and observations add to them (`Evidence::corroborate` / `Evidence::contradict`, `confidence.rs:250-258`):

```
corroborate:  α += w
contradict:   β += w
mean         = α / (α + β)                    (confidence.rs:281-291)
concentration = α + β                         (confidence.rs:272-279)
variance      = αβ / ((α+β)²(α+β+1))          (confidence.rs:293-304)
```

`w` is the observation's evidence weight, which depends on provenance — see the next major section. With the provenance-blind weight of 1.0 (`DEFAULT_EVIDENCE_WEIGHT`, `confidence.rs:26`) this reduces exactly to counting observations, which is what makes the provenance mechanism differentially testable.

Counts are non-negative by construction: `sanitize_count` (`confidence.rs:308-314`) clamps a corrupt or non-finite persisted value to zero rather than producing a nonsense mean, and `sanitize_weight` (`confidence.rs:318-324`) makes a negative or NaN weight record *nothing* rather than eroding evidence already accumulated. With no evidence at all the mean is 0.5 — maximal ignorance — and the variance is reported as 0.0, since there is no distribution to be uncertain about.

**Worked example.** Take a fact extracted as `Inferred`, so the prior is 0.6 (α=6, β=4, C=10), corroborated three times by independent sources and then contradicted once:

| Step | Event | α | β | C | Mean | Variance |
|------|-------|---|---|---|------|----------|
| 0 | prior (Inferred) | 6 | 4 | 10 | 0.600 | 0.0218 |
| 1 | corroborate (external) | 7 | 4 | 11 | 0.636 | 0.0193 |
| 2 | corroborate (external) | 8 | 4 | 12 | 0.667 | 0.0171 |
| 3 | corroborate (external) | 9 | 4 | 13 | 0.692 | 0.0152 |
| 4 | contradict (external) | 9 | 5 | 14 | 0.643 | 0.0153 |

The mean column is identical to what the old stateless model produced — that was deliberate, so the change is not a silent re-scoring of anyone's graph. The *concentration* column is the new information: it grows. The variance narrows as it does, except at step 4, where the mean moving back toward 0.5 outweighs the extra observation. That is correct behavior, not a bug: a contradicted edge is genuinely less settled than an uncontradicted one at the same evidence count.

`evidence_accumulates_across_observations` (`confidence.rs:497-516`) locks the mean sequence *and* asserts the final counts (α=9, β=5, concentration=14). `variance_narrows_with_corroboration` (`confidence.rs:518-541`) asserts the monotone narrowing across 1, 5 and 50 observations.

**Convergence is slower than the old artifact suggested, and that is the point.** The stateless model recomputed α from the mean each time, which made every update a step of `c' = (10c + 1)/11` — geometric convergence to 1.0 that erased its own history. From a `Speculative` prior of 0.3 it reached 0.994 after fifty corroborations. The stateful model reaches **0.883** at fifty (α=53, β=7) and crosses 0.95 only at about **130**, reaching 0.967 at two hundred. The difference is entirely the seven units of contradiction mass in the prior, which now *persist* instead of being recomputed away. An edge that started life speculative carries that skepticism until real evidence buries it. Slower is the honest answer.

**Migration.** The store carries a `schema_version` on a singleton `meta:schema` record; this build writes version 1 (`store.rs:29`). `init_schema` defines the tables and then runs `migrate` on every open (`store.rs:132-135`), which is a no-op once the version marker is current. The backfill itself is one re-runnable statement (`backfill_edge_evidence`, `store.rs:277-294`):

```sql
UPDATE relates_to SET
    alpha = confidence * $concentration,
    beta  = (1 - confidence) * $concentration,
    self_reinforcements = 0
WHERE alpha IS NONE
```

Every legacy edge keeps its stored mean exactly and gains the honest low concentration of something that was never actually counted. The pass is crash-only by construction: the backfill runs *before* the version marker is written (`store.rs:249-267`), and `WHERE alpha IS NONE` makes re-entry a no-op for edges already done. Kill it halfway and the next open finishes the rest, counting nothing twice.

Until the migration reaches an edge, `Evidence::from_stored` (`confidence.rs:237-248`) falls back to `from_prior` over the bare mean — so an unmigrated store reads correctly rather than reading as zero evidence. The schema declares `alpha`, `beta` and `self_reinforcements` as `option<float>` / `option<int>` precisely to make that state representable (`store.rs:171-175`).

Two write paths touch the counts, and the distinction between them matters. `reinforce_relationship` (`crud.rs:284-306`) *adds* an observation: the caller loads the edge's evidence, records the observation with its provenance, and the whole state is written back — mean, counts, coherence tally, and `last_reinforced = now`. `update_relationship_confidence` (`crud.rs:256-270`) *asserts a mean*: it resets the edge to the prior concentration around the new value, discarding accumulated evidence, because a hand-set confidence is a claim about the mean and not an observation about the world. Writing the counts whole rather than incrementing them in SurrealQL keeps the stored mean and the stored counts from ever disagreeing.

### 2. Temporal decay layered on top

The Bayesian posterior is what's stored. Effective confidence at read time also accounts for staleness:

```
effective = stored × 0.5^(days_since_reinforced / 90)
```

implemented in `temporal_decay` (`confidence.rs:396-409`), floored at `DECAY_FLOOR = 0.05` (`confidence.rs:385`), with the half-life default at `confidence.rs:382`. Decay is computed against `last_reinforced` if present, otherwise against `valid_from` (`effective_confidence`, `confidence.rs:414-431`). A corroborating update goes through `reinforce_relationship`, which resets the decay clock — so a fact that keeps getting confirmed never decays.

Why layer decay on top of the posterior rather than baking it into the update? Two reasons:

1. **Decoupling.** The posterior captures *evidence*; decay captures *recency*. A fact stated nine times two years ago and a fact stated three times yesterday are different things, and the system should be able to tell them apart. Mixing the two into one update loses information.
2. **Cheap reads, cheap writes.** Decay is computed at query time from the timestamp. No background job has to walk the graph and re-score edges every night.

The floor at 0.05 is a deliberate choice: edges never disappear through decay alone. They become low-priority. Garbage collection is a separate concern handled in `src/graph/gc.rs`.

### 3. Path confidence as the edge product

Multi-hop traversal compounds confidence multiplicatively (`path_confidence`, `confidence.rs:439-441`):

```
path_confidence([0.8, 0.7, 0.9]) = 0.504
```

This falls naturally out of treating edges as independent probabilities. The practical effect is that the long tail of weak chains is automatically suppressed. Two hops over 0.9 edges is a stronger signal than four hops over 0.7 edges (`0.81` vs `0.24`) — no special-case pruning needed.

The hybrid query in `src/graph/query.rs` applies this directly: after the semantic phase identifies the top candidates, graph expansion adds neighbors whose *relevance* is the parent's, discounted by the edge — `similarity_parent · effective_confidence` (`merge_graph_candidates`). Effective confidence is computed with temporal decay at read time, and edges below 0.1 effective confidence are dropped before scoring (`get_neighbor_details`).

Confidence therefore attenuates the same term for both channels rather than scaling a whole score. A graph-sourced candidate is scored by `score_with_utility` exactly as a semantic one is — hotness and utility read off the neighbor itself — so a decayed edge ranks its target lower without the target having to overcome a base that semantic candidates receive for free:

```
  semantic                graph expansion
  ─────────               ──────────────
    [E1: sim 0.84] ──[0.9]──→ [N1: sim 0.756]  + own hotness/utility
       │                          │
       └───────[0.6]──────────→  [N2: sim 0.504]   ← scored, not pruned
                                                   ← attenuates over hops
    [E2: sim 0.71] ──[0.4]──→ [N3: sim 0.284]      ← below filter floor
                                                     after second hop
```

When both channels reach the same entity, the graph corroborates a measurement rather than replacing it: the measured similarity is raised by `1 + corroboration_boost · effective_confidence`, clamped at 1.0. The independence precondition below is why the lift is small, why it is credited once per entity over its strongest path rather than accumulated across the expanded parents — those parents are the top hits of a single query, not independent witnesses — and why a self-edge is skipped rather than counted.

Note the honest asymmetry: the multiplicative rule assumes edge independence, and the provenance machinery below exists precisely because *observations* are frequently not independent. Path independence and observation independence are different assumptions; only the second one is modelled.

## Provenance, or why corroboration requires independence

Persisting evidence makes a second problem sharp rather than solving it. If every corroboration adds to α, then an agent that re-asserts its own beliefs across sessions manufactures confidence out of nothing. The graph grows more certain of what it already believed, for no reason other than that it keeps hearing its own voice.

The provenance analysis below originates in research by **Echo, a `pulse-null` entity**, conducted 2026-08-04 on exactly this question — whether a memory system can tell independent corroboration from itself re-asserting. What follows is that analysis and the mechanism it implies, now shipped.

### The epistemology: independence is a precondition, not a nicety

**Bovens & Hartmann** (*Bayesian Epistemology*, 2003) make confidence in a set of reports a function of *both* source reliability and source **independence** — independence is an explicit node in their witness Bayesian network, not an assumption buried in the derivation. As dependence between sources rises, the confidence boost from their agreement shrinks toward zero. Three independent studies at reliability 0.8 beat one study at 0.8; three *copies* of one study at 0.8 are worth roughly one study.

**Olsson**'s impossibility result (*Against Coherence*, 2005) sharpens this into a precondition. The witness model licenses a coherence→credence boost *only if* the reports are conditionally independent given the fact, and each witness has nonzero individual credibility. Drop conditional independence and the boost is not merely weaker — it is not licensed by the probability calculus at all. "Corroboration raises confidence" is a law with a precise precondition, and the precondition is independence.

The default behavior of any count-based scheme is to violate it. This is the echo-chamber failure and the illusion of consensus: people are as convinced by one source cited many times as by many independent sources unless explicitly cued to the shared origin, and agent-based models of echo chambers inflate confidence purely from a single origin propagating. A naive `α += 1` on every re-extraction is that failure mode expressed in Rust.

**Dong, Berti-Équille & Srivastava** (VLDB 2009, *Truth discovery and copying detection in a dynamic world*) show what the mature engineering answer looks like: model `P(S₂ copied from S₁)` and down-weight agreement among sources judged to be copiers, so "many sources say X" stops counting as strong evidence once copying is inferred. **Rekatsinas et al.** (*Fusing Data with Correlations*, arXiv:1503.00306) generalize the accounting to arbitrary correlations and show it is tractable — polynomial for tree-structured dependency models — with one load-bearing caveat: the framework *assumes the dependency structure is given as input*. It does not discover correlations from data.

### The maximum-dependence limit

`recall-echo` is not a mildly-correlated-sources problem. Its corroborating episodes are, in the common case, authored by one agent re-asserting across timestamps. Conditional independence given the fact is not weakened here; it is **structurally absent**. Confidence gained in that regime is coherence amplification, not evidence.

And it cannot be recovered after the fact. Copy-detection works because it has a *population* of distinct nominal sources to triangulate among; at n=1 it is not expensive, it is undefined. Every statistic computable over the agent's own episode stream — corroboration count, semantic agreement, graph coherence, the posterior itself — is a function of the agent's own prior assertions. "I re-asserted X because X is true" and "I re-asserted X because I believe X" emit identical episode streams.

Which leaves exactly one route, and it is the one taken: **capture the distinction at write time**. Recording lineage is cheap; reconstructing it is impossible. The tag must be applied where content crosses from world into memory, by something that is not the loop re-touching the belief. Echo's phrasing for the conclusion is the right one: the fix is *provenance tagging at write-time, not a smarter scorer*.

### The mechanics, as shipped

**Three classes, stamped on the episode.** `Provenance` (`confidence.rs:84-98`) has exactly three variants: `External` (ingested documents, web content, tool output — sources independent of the agent), `User` (statements authored by the human), and `SelfGenerated` (the agent's own summaries, reflections and re-assertions). Episodes carry it as a string field (`store.rs:199`), and `add_episode_from` (`crud.rs:427-461`) writes it. Provenance is a parameter of the ingestion call rather than a field of `NewEpisode` because it is a property of the *ingestion context*, not of the text: the same chunk is external when read out of a document and self-authored when the agent wrote it.

Three classes rather than two is deliberate. Collapsing three into two at scoring time is always possible; splitting one class back into three after the fact is not.

**Weights.** `ProvenanceWeights` (`confidence.rs:153-198`) maps class to evidence weight, defaulting to:

```
external → 1.0     (DEFAULT_WEIGHT_EXTERNAL, confidence.rs:29)
user     → 0.8     (DEFAULT_WEIGHT_USER,     confidence.rs:32)
self     → 0.05    (DEFAULT_WEIGHT_SELF,     confidence.rs:35)
```

They are configurable per deployment via `[graph.provenance]` in `.recall-echo.toml` (`config.rs:180-186`), settable individually through `config set graph.provenance.weight_self …` (`config.rs:409-419`), and negative values are rejected at parse time. `default_weights_rank_independence_above_repetition` (`confidence.rs:598-615`) asserts the ordering, not just the values.

**Corroboration by class.** `EdgeEvidence` (`confidence.rs:334-378`) is the pair the write path actually manipulates: the Beta counts plus the coherence tally. `EdgeEvidence::corroborate` (`confidence.rs:351-358`) adds the class's weight to α *and*, when the class is `SelfGenerated`, increments `self_reinforcements`. `EdgeEvidence::contradict` (`confidence.rs:360-365`) adds the weight to β and leaves the tally alone — contradicting yourself is not coherence.

The consequence is testable and tested. `external_contradiction_outweighs_accumulated_self_corroboration` (`confidence.rs:697-718`) runs twenty self-corroborations against an `Inferred` edge — which moves it from 0.600 to 0.636 and sets `self_reinforcements = 20` — then applies a *single* external contradiction and asserts the mean lands **below** where it started (7/12 = 0.583). Twenty rounds of the agent agreeing with itself do not survive one independent source disagreeing.

**`self_reinforcements` as a separate tally.** The count is persisted on the edge (`store.rs:175`, `types.rs:180-183`) and never folded into the mean. Keeping it separate is what lets "believed because three independent sources said so" stay distinguishable from "believed because the agent has said it thirty times" — two edges that can otherwise sit at the same confidence. It is available on every `Relationship` read out of the store and is written on every reinforcement (`crud.rs:302`).

**Unknown defaults to self.** `Provenance::from_stored` (`confidence.rs:100-123`) resolves both an *absent* value (an episode written before provenance existed) and an *unrecognised* one (written by a newer build, or by hand) to `SelfGenerated`. `Provenance`'s `#[derive(Default)]` points at the same variant. This is why the episode `provenance` field needed no backfill and no schema version bump (`store.rs:194-199`): the absent case already means the conservative thing. A legacy store never gains confidence from data it cannot vouch for. `stored_provenance_defaults_to_self` (`confidence.rs:648-660`) pins it.

**Where the class comes from.** `ProvenancePolicy` (`ingest.rs:26-45`) has two modes. `FromTurnRoles` — the default — reads authorship off the `### User` / `### Assistant` headings the archive pipeline writes (`infer_from_turn_roles`, `ingest.rs:105-122`). A chunk is credited to the human only when *every* role heading in it is a user turn; mixed turns, assistant turns, and text with no headings at all are the agent's own. `Fixed(class)` stamps the whole run, which is what `graph ingest --external` does for document ingestion (`main.rs:266-276`, `main.rs:688-692`). The heuristic is conservative in both directions by design — `heading_matching_is_exact` (`ingest.rs:503-508`) checks that `### Users of the system` is read as a topic, not a turn.

The class then rides through extraction: a relationship keeps the provenance of the chunk it came out of (`ingest.rs:250-264`), and re-extraction of an existing edge corroborates it at that class's weight (`ingest.rs:307-330`). Evidence is only ever as independent as the text that produced it.

**The escape hatch.** `ProvenanceWeights::uniform(w)` (`confidence.rs:174-187`) weights every class identically. `uniform(DEFAULT_EVIDENCE_WEIGHT)` reproduces provenance-blind behavior *exactly*, which is what makes the whole mechanism differentially measurable: run a benchmark at `(1, 1, 1)` and at the defaults, and the delta is attributable to provenance weighting and nothing else. `uniform_weights_are_provenance_blind` (`confidence.rs:617-631`) asserts the reduction holds.

**One knock-on effect.** Episode garbage collection uses the same signal: an episode is collectable only when it is old, never retrieved, cited by nothing, *and* self-authored (`gc.rs:501-528`). External and user episodes survive any age — they are the only evidence in the store that was not manufactured internally, and throwing them away would be throwing away the independence the whole mechanism depends on.

## Adaptive utility scoring

The Bayesian model handles *epistemic* uncertainty about whether a relationship holds. A separate layer handles *instrumental* utility: did this entity, when retrieved, actually help the agent succeed?

`src/graph/utility.rs` implements an exponential moving average over per-entity utility scores. Outcomes have rewards (`utility.rs:23-33`):

```
Success → 1.0
Partial → 0.5
Failed  → 0.0
```

When a session is adjudicated, the entities it touched receive an EMA update. The alpha depends on whether the entity was actually used (`utility.rs:61-68`):

```
USED_ALPHA    = 0.1     // retrieved AND used → full signal
UNUSED_ALPHA  = 0.05    // retrieved but not used → muted
UNUSED_REWARD = 0.3     // "you retrieved me and ignored me" → slight negative
```

The update is `new = (1 − α) · current + α · reward`, applied atomically in a single SurrealDB query to avoid read-modify-write races (`utility.rs:479-505`). The convergence test (`utility.rs:584-596`) confirms that ~50 successive successes push a score from 0.5 to >0.99.

Until Phase 1, this layer had no caller — it was correct, tested, and dead. It now has two.

**Passive linkage at ingestion.** The last phase of extraction records which entities a session produced or reinforced (`record_session_use`, `utility.rs:206-233`, called from `ingest.rs:361`). Those records are `contributed_to` edges pointing at a per-session outcome entity marked `pending` — a state that carries no reward and moves no utility score, because an unadjudicated session is not evidence of usefulness (`ContributionResult`, `utility.rs:304-322`). Linkage is idempotent per session: re-ingesting rewrites the records rather than accumulating duplicates (`create_contribution_edge`, `utility.rs:447-476`), and one outcome entity is created on first use and reused thereafter whichever order the linkage and the verdict arrive in (`outcome_entity_for_session`, `utility.rs:329-341`).

**Adjudication on demand.** `recall-echo graph feedback <session-id> --outcome success|partial|failed` (`main.rs:335-343`, `graph_cli.rs:1153`) resolves the session's entities from those records and applies the outcome (`GraphMemory::record_session_outcome`, `mod.rs:527`). If a store has no records for the session — ingested before passive linkage existed — it falls back to the entities the session authored (`session_entities`, `utility.rs:240-294`). The command reports every entity that moved and where its score landed. It runs through the daemon like search and ingest, so it can be issued while a session is still using the store.

These utility scores feed the final retrieval composition (`score_with_utility`, `search.rs:231-239`):

```
final_score = w_semantic · similarity
            + w_hotness  · hotness
            + w_utility  · utility_score
```

Default weights are `0.45 / 0.30 / 0.25` (`GraphScoringConfig`, `config.rs:249-255`), configurable per deployment via `[graph.scoring]` in `.recall-echo.toml`. Hotness itself is a separate signal — `sigmoid(ln(1 + access_count)) · exp(−ln(2)/7 · days)` — capturing recent activity with a 7-day half-life (`search.rs:244-258`).

So the system has two decay clocks operating on different signals at different rates: 7 days for hotness (engagement), 90 days for confidence (epistemic decay). Both are tunable.

## Comparison to the landscape

| System | Edge model | Contradiction handling | Stale-fact handling | Reinforcement | Source independence |
|---|---|---|---|---|---|
| Graphiti | Bi-temporal validity intervals | Invalidate + new edge | Explicit `valid_to` | None — observation count not tracked | Not modelled |
| mem0 | ADD-only with conflict tags | Flagged, manual resolve | None | Not represented | Not modelled |
| Cognee | Graph rewrite on conflict | Regenerate region | Implicit via rewrite | Lost on rewrite | Not modelled |
| recall-echo | Beta posterior with **persisted** α/β | Contradiction mass accumulates and persists | Half-life decay at read time | α += provenance weight, clock reset, concentration grows | Three write-time classes, weighted 1.0 / 0.8 / 0.05, self-corroboration separately tallied |

Both differentiating columns are new. Accumulating evidence means the system can distinguish "believed at 0.9" from "believed at 0.9 for good reason" — the variance is the difference, and nothing else in the space computes it. Provenance-aware weighting means it can distinguish corroboration from repetition — and here the gap is wider still: the truth-discovery literature has done this since 2009, and no open-source agent-memory system has picked it up.

A few honest notes.

**Graphiti's bi-temporal model gives you something Bayesian confidence does not: point-in-time queries.** "What did this agent believe about X on March 14?" is a clean SQL-like query against `valid_from`/`valid_to`. Against a Beta graph, the equivalent question is "what were the counts at that timestamp?" — answerable only if you log every update, which `recall-echo` still does not do. Persisting α and β makes the *current* state auditable; it does not make the history replayable. If point-in-time auditability matters more than calibrated uncertainty for your use case, bi-temporal is the right design.

**mem0's strength is operational simplicity.** No conjugate priors, no half-lives, no provenance policy to get wrong. For short-horizon agents that won't accumulate enough observations for the math to matter, that simplicity is the correct call.

**Cognee's rewrite approach is the most aggressive about consistency** but trades history for it. For an agent whose memory needs to support "why did you think X?" introspection, that's a hard trade.

The case for probabilistic edges is specifically about long-running autonomous entities — systems whose memory is being shaped by hundreds or thousands of observations over months, where the noise floor is high, where "this is still probably true" is a useful concept, and where a large fraction of those observations are the agent talking to itself. That is the design center for `recall-echo`.

## Implementation notes

The confidence model is ~440 lines of Rust plus ~410 of tests (`src/graph/confidence.rs`). The utility layer is ~540 lines plus tests including the SurrealDB feedback edges (`src/graph/utility.rs`). It runs on:

- **SurrealDB** (embedded SurrealKV by default, WebSocket server optional) as the graph store and HNSW vector index
- **FastEmbed** for local 384-dimension cosine embeddings
- **Tokio** for async, with `futures::future::join_all` for concurrent per-entity feedback updates (`utility.rs:166`)
- **MPL-2.0** (through v3.13.0: AGPL-3.0)

A few design choices worth pulling out:

- `PRIOR_CONCENTRATION = 10` sets how much evidence a *prior* is worth, and nothing else. It is no longer the fixed concentration of every edge forever — it is the starting point, and the migration's backfill value. Lower makes new edges twitchy; higher makes early corroboration slow to register. Ten is the conventional weak-prior choice.
- Weights are floats, and α/β are weighted sums rather than integer counts. This is what lets a self-corroboration be worth 0.05 of an observation instead of requiring a separate mechanism.
- The `Evidence` counts are private with accessor methods, so nothing outside the model can write a state where the mean and the counts disagree.
- Half-life of 90 days is a default; the constant lives at `confidence.rs:382` and the `temporal_decay` signature accepts a per-call value, so per-domain tuning is mechanical.
- The decay floor at 0.05 is deliberate. Edges below it are still queryable but ranked near the bottom. Hard deletion is a GC concern.
- The hybrid query filters effective confidence below 0.1 during graph expansion (`get_neighbor_details`). Below this, the multiplicative chain attenuates results to noise.

## Reproducibility

Source: [`github.com/dnacenta/recall-echo`](https://github.com/dnacenta/recall-echo), MPL-2.0.

Read in this order:

- `src/graph/confidence.rs` — `Evidence`, `EdgeEvidence`, `Provenance`, `ProvenanceWeights`, decay, path composition
- `src/graph/store.rs` — schema definition and the version-1 evidence migration
- `src/graph/crud.rs` — `reinforce_relationship` (adds evidence) vs `update_relationship_confidence` (asserts a mean)
- `src/graph/ingest.rs` — turn-role provenance inference and the corroboration path
- `src/graph/utility.rs` — EMA feedback loop, passive session linkage, outcome edges
- `src/graph/search.rs` / `src/graph/query.rs` — final scoring composition and confidence-weighted expansion

The test suite in `confidence.rs:443-849` covers the update math, accumulation, variance narrowing, weight and count sanitization, the three provenance classes and their serde wire names, the self-vs-external asymmetry, the decay function, the floor, and the path product. The utility tests in `utility.rs:543-661` cover EMA math, convergence, the used-vs-unused asymmetry, and the pending/resolved contribution states. Run `cargo test -p recall-echo graph::confidence` and `cargo test -p recall-echo graph::utility` to reproduce.

## Open questions and known limits

**Bi-temporal point-in-time queries are still unsupported.** Persisted counts make the current posterior auditable, not the history of it. Replaying "what did the graph believe last March" would need a per-observation log; the current design treats the counts as sufficient. A Beta posterior stored alongside `valid_from`/`valid_to` intervals would give both, at the cost of schema complexity — and it only pays off if anyone's agent actually issues point-in-time queries.

**The priors and the provenance weights are hand-tuned.** The four extraction-context priors and the three class weights (1.0 / 0.8 / 0.05) are constants chosen for plausibility, not fitted to anything. The honest version tracks the calibration of each context and each class over time — what fraction of "Speculative" claims, or of user-authored claims, actually held up under independent corroboration — and adjusts. That is straightforward Bayesian model-averaging and it is future work. Until the benchmark runs at `(1, 1, 1)` versus the defaults, the *direction* of the effect is argued from the epistemology, not measured.

**Retrieval consumes neither variance nor provenance.** `score_with_utility` sees similarity, hotness and utility; graph expansion sees the decayed mean. Variance is computed and tested but read by nothing outside the tests, and `self_reinforcements` is persisted on the edge but rendered by no CLI view. This is deliberate for Phase 1 — the point was to make the state true and the scoring unchanged, so that a benchmark can attribute any movement to the state rather than to a re-ranking. Consuming variance ("prefer the well-evidenced edge among equals") and surfacing the coherence tally are Phase 2.

**The migration report goes nowhere.** `init_schema` returns a `MigrationReport` with the count of backfilled edges (`store.rs:225-241`), and both call sites currently discard it. Nothing tells an operator that their store was migrated, or how many edges moved. That is a gap, not a design.

**The utility feedback loop is newly wired and unproven.** `record_outcome_feedback` was dead code until this phase; it now has a passive caller at ingestion and an explicit one in `graph feedback`. The EMA math is tested, the wiring is tested, but nothing yet demonstrates that the resulting utility scores improve retrieval on a real workload. Treat the loop as live and unvalidated.

**Provenance inference is a heuristic at one boundary.** Turn-role inference is only as good as the archive format, and it is conservative by construction: anything ambiguous is credited to the agent. That is the right failure direction — under-crediting costs confidence that real evidence would have earned, over-crediting manufactures it — but it does mean a store whose archives lack role headings will treat genuinely external material as self-authored until someone passes `--external`. The tag's correctness is a function of where it is applied, and that remains an architectural property, not something the code can check.

---

`recall-echo` exists because the entity it serves — `pulse-null`, a long-running autonomous Rust runtime — needed a memory layer that wouldn't degrade into noise after a few months of operation. The Bayesian model is the part that earns its keep. Making it remember its own evidence, and making it refuse to count its own voice as a second witness, is what Phase 1 was for. The rest is plumbing.
