# Phase 2 — Answer Assembly and Retrieval Quality

**Status**: Todo (re-plotted 2026-08-05 after the Phase 0 baseline)
**Supersedes**: the Phase 2 section of `/opt/pulse-vault/pulse-null/specs/todo/recall-echo-v4-roadmap-spec.md`, which adopted Echo's retrieval-modernization spec as written. That sequencing was correct given what was known; the baseline changed what is known.

## Why this is re-plotted

The roadmap assumed retrieval quality was the lever. The N=10 LongMemEval baseline (`docs/benchmarks/baseline-2026-08.md`) says otherwise:

```
retrieval:  MRR 0.778   Recall@3 0.648   Recall@full 0.676     ← the memory finds the evidence
answers:    0 / 9 headline (abstention 1/1)                    ← and then fails to use it
```

The failures are honest partial assemblies ("2 items: the boots and the blazer" where gold is 3) and honest give-ups on questions whose evidence *was retrieved*. A reranker cannot fix that. Two further cost/behaviour findings landed alongside it:

- **Dedup escalation**: per-conversation ingest time ranged 600s → 5,400s (9×) as graphs grew — each new entity triggers LLM dedup against an ever-larger candidate set. Ingest cost ran ~5× the estimate (~$3/question), which is what makes the 50-question benchmark expensive enough to defer.
- **Retrieval is near-semantic-only in practice** (from `tests/query_golden.rs`): graph expansion is inert when the graph is smaller than 2×limit; a candidate found *both* semantically and over a high-confidence edge keeps its semantic score (graph corroboration is discarded on collision); graph-sourced scores lack the hotness/utility floor that semantic scores get, so effective confidence must exceed ~0.8 to compete — confidence is near-binary at retrieval time; and only semantic hits increment access counts, so graph-tail entities can never warm up.

## Work items, in priority order

### 1. Answer-context assembly (the bottleneck)
`bench answer` builds one prompt from top-20 graph facts + top-5 archive snippets + MEMORY.md. LongMemEval's dominant classes (temporal-reasoning 133/500, multi-session 133/500) need *many* small facts assembled, not one good passage.
- Measure first: for the 9 answerable baseline questions, determine whether the gold-supporting facts were present in the assembled context at all. That single number splits "retrieval didn't surface it" from "assembly dropped it" from "the model had it and still missed."
- Then: budgeted multi-fact assembly (raise fact count, group by session/date, preserve temporal ordering), evidence-ordered rather than score-ordered presentation for temporal questions, and an explicit "what do you know about X across sessions" shape for aggregation questions.
- Exit: measured answer-accuracy delta on the same 10 questions.

### 2. Dedup cost (gates everything downstream)
- Add an embedding-similarity fast path before any LLM dedup call: identical/near-identical names resolve without a model call; only genuinely ambiguous candidates escalate.
- Gate on cosine similarity, not the blended hotness/utility score (a "hot" unrelated entity currently triggers dedup comparisons).
- Cap candidate-set growth (top-k by similarity, not everything above a blended threshold).
- Exit: per-conversation ingest time flat rather than growing with graph size; measured on a re-ingest of the same 10 conversations.

### 3. The three retrieval bugs (cheap, and they compound with #4)
- Graph corroboration discarded on collision → boost, don't skip, when an entity is found both semantically and via a high-confidence edge.
- Graph-score handicap → give graph-sourced candidates a comparable base so confidence stops being effectively binary.
- Access counts → increment for graph-expanded results too, so the graph tail can warm.

### 4. Echo's retrieval modernization (unchanged, now correctly sequenced last)
1. Reranker (BGE-reranker-v2) over the fused shortlist.
2. Hybrid dense+sparse with RRF fusion — restores exact-token precision for codes, versions, names.
3. BGE-M3 swap (384→1024 re-embed, index rebuild) — verify FastEmbed sparse support and ONNX RAM on the VPS first; the model is ~10× BGE-Small.
4. Spanish cases in the harness.

## Acceptance criteria

- [ ] AC1: Context-presence analysis published for the baseline's 9 answerable questions — gold-supporting facts classified as absent-from-retrieval / dropped-in-assembly / present-but-unused.
- [ ] AC2: Answer accuracy on the same 10 questions improves measurably over 0/9, with the run recorded in `docs/benchmarks/`.
- [ ] AC3: Re-ingesting the 10 baseline conversations shows per-conversation time no longer growing with graph size (compare against the 600s→5,400s spread).
- [ ] AC4: Golden-set tests updated to pin the corrected behaviours (corroboration boost, comparable graph scoring, access-count symmetry) — the current tests pin today's bugs deliberately, so they must change *deliberately*.
- [ ] AC5: Each modernization stage (reranker, hybrid, BGE-M3) recorded with a before/after on the same subset; any stage that does not pay for itself is reverted, and the revert is recorded too.
- [ ] AC6: No regression in retrieval metrics at any stage (MRR/Recall@k vs the Phase 0 baseline).

## Out of scope

MCP server and licensing (Phase 3); publication (Phase 4); the 50-question run — it becomes affordable *because* of work item 2, so it is the natural first beneficiary rather than a prerequisite.
