# LongMemEval Baseline — August 2026 (Provisional, N=10)

**Run date**: 2026-08-04 · **recall-echo**: feat/RE-29 branch (v3.11.1 + daemon + runtime backend; retrieval stack = BGE-Small-EN-v1.5 384d, dense-only HNSW, 1-hop confidence-weighted expansion)
**Dataset**: LongMemEval-S (Wu et al.), deterministic stratified 10-question subset (seed 42): 3 temporal-reasoning, 2 multi-session, 2 knowledge-update, 1 each single-session-user/assistant/preference (one `_abs` abstention row among them).
**Doctrine**: no number, no claim — this is the "before" picture every later phase diffs against. Single run, point estimates, N=10: treat every number as coarse.

## Setup (exact, for reproducibility)

| Stage | Provider | Model |
|---|---|---|
| Ingest (extraction + dedup) | Anthropic API | claude-haiku-4-5-20251001 |
| Answer | claude-code CLI | sonnet |
| Judge | claude-code CLI | sonnet — LongMemEval's official per-type prompts, verbatim (upstream commit d6dc8b5) |

Harness: `/opt/recall-echo-locomo` @ a8c71e7, `scripts/lme-run.sh` + follow-up stages. One entity root per question; ~540 sessions ingested with LLM extraction. Retrieval ground truth: `answer_session_ids` from the oracle file; retrieval metrics computed independently of the judge.

## Results

### Retrieval (evidence-session recall, n=9 answerable)

| Metric | Value |
|---|---|
| MRR | **0.778** |
| Recall@1 | 0.38 |
| Recall@3 | 0.648 |
| Recall@5 / @10 / @full | 0.676 |

### Answer accuracy (official LongMemEval judges, n=9 headline + 1 abstention)

| Category | n | J-score |
|---|---|---|
| **Headline (all answerable)** | 9 | **0.0%** |
| Abstention | 1 | 100% |
| temporal-reasoning | 3 | 0% |
| multi-session | 2 | 0% |
| knowledge-update | 2 | 0% |
| single-session (u/a/p) | 3 | 0% |

## The headline finding: retrieval works, answer assembly doesn't

The gap between 68% evidence recall and 0% answer accuracy is the result. The failures are not hallucinations — they are honest partial assemblies ("2 items: the boots and the blazer" where gold is 3) and honest give-ups ("I don't have enough information") on questions whose evidence *was retrieved*. The answer path (`bench answer`: top-20 graph facts + top-5 archive snippets + MEMORY.md into one prompt) is structurally insufficient for LongMemEval's dominant question classes — cross-session aggregation and temporal reasoning — which require assembling *many* small facts, not finding one good passage.

Implications for Phase 2 (retrieval modernization): reranking and hybrid retrieval will lift Recall@1 (0.38 is weak — the right session is usually present but not first), but the larger lever this baseline exposes is **answer-context assembly** — how much retrieved material reaches the answering model and in what form. The golden-set findings (graph expansion inert on small result sets; graph corroboration discarded; near-binary confidence at retrieval) compound the same direction.

Field context, not comparable directly (different N, different answer stacks): Zep reports 63.8% LongMemEval; full-context GPT-class baselines ~60-80% by type. recall-echo's current answer stage was never tuned for this benchmark; that is precisely what makes this a useful "before."

## Cost findings (unplanned but load-bearing)

Ingest via API consumed ~$30 for ~540 sessions (~$3/question) — ~5× the pre-run estimate. Per-conversation ingest wall-time varied 600s→5,400s (9×): consistent with **dedup escalation** — each new entity triggers LLM dedup against a growing candidate set, so cost scales super-linearly with graph size. This is empirical confirmation of the audit's dedup-gating concern and a Phase 2 cost target in its own right: fixing dedup cheapens every future benchmark run ~5×.

## Known gaps in this run (honest list)

- **N=10.** The 50-question stratified run is specced and scripted; deferred until dedup costs drop (or credits allow). Same subset-selection code guarantees comparability.
- **Full-context and grep baselines not yet run** (would cost more than the system run itself at 115k tokens/question); required before any *published* comparison.
- Answer/judge ran via CLI (subscription), ingest via API — model families documented above; the judged system is unaffected by judge transport.
- Single run, no variance estimate. Provider defaults for temperature.
- Question dates injected as a `[Current date: …]` prefix (bench answer has no native date parameter); judges see the bare question.
