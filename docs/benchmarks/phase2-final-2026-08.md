# Phase 2 Final — LongMemEval, N=10

**Date**: 2026-08-05 · **Branch**: feat/RE-37-answer-assembly
**Dataset**: LongMemEval-S, deterministic stratified 10-question subset (seed 42): 9 answerable + 1 abstention.
**Judge**: LongMemEval's official per-type prompts, verbatim (upstream `d6dc8b5`), via claude-code/sonnet.
**Retrieval metrics** computed independently of the judge, against `answer_session_ids` from the oracle file.

## Three measurement points

| Metric | Baseline | + assembly fixes | + re-ingest |
|---|---|---|---|
| **Answer accuracy (n=9)** | **0.0%** | 55.6% | **77.8%** |
| Abstention (n=1) | correct | *incorrect* | correct |
| Recall@1 | 0.380 | 0.491 | 0.491 |
| Recall@3 | 0.648 | 0.833 | **0.972** |
| Recall@full | 0.676 | 1.000 | 1.000 |
| MRR | 0.778 | 0.903 | 0.944 |

- **Baseline** — v3.13.0 behaviour. See `baseline-2026-08.md`.
- **+ assembly fixes** — same ingested stores, re-answered with the archive channel revived and episode top-k decoupled. No re-ingest, so zero API cost. See `assembly-fixes-2026-08.md`.
- **+ re-ingest** — the ten conversations ingested again with the full Phase 2 binary: 1,000-character episode abstracts, similarity-gated dedup, and the corrected retrieval scoring.

Per-category at the final point: temporal-reasoning 100%, knowledge-update 100%, single-session-assistant 100%, abstention 100%, multi-session 67%, single-session-preference 0%.

## What each change contributed

The Phase 0 context-presence analysis classified all nine baseline failures: 2 absent from retrieval, 5 dropped in assembly, 2 present-but-unused — with the store holding the answer in **9 of 9**. Every subsequent change follows from that.

**Assembly (0% → 55.6%).** `ranked_search` required every query token to appear in a file, so natural-language questions eliminated every candidate before scoring: the archive channel contributed nothing to any prompt. Replacing the AND gate with OR-qualification plus a coordination factor, and decoupling `episode_top_k` (20) from `archive_top_k` (5), moved five questions from wrong to right without touching the stores.

**Re-ingest (55.6% → 77.8%).** Episode abstracts were cut at 200 characters, mid-word — one failure severed *Kansas City Masterpiece* into `"...my favourite is Ka..."` and the model answered from the fragment. At 1,000 characters, cut on sentence boundaries, two more questions resolve and Recall@3 rises to 0.972.

**The abstention question is the interesting one.** It regressed at the middle point (correct → incorrect) and recovered at the end. With a starved context the system correctly said it lacked the information; with partial context it confabulated "30 minutes" by attaching the user's *guitar* practice duration to a question about violin; with full-length abstracts it can see enough context to recognise that violin was never mentioned. The confabulation was caused by truncation, not by richness — which inverts the obvious reading and is worth stating, because the obvious reading would have led to a prompt-engineering fix for a data-shape problem.

## Ingest cost

Re-ingest of the ten conversations, measured by counters added for this purpose (`dedup_llm_calls` / `dedup_fast_path`):

- **25,810 dedup decisions; 37.0% required a model call; 16,271 avoided.**
- Under the previous gate — which filtered on the *blended* retrieval score (similarity + hotness + utility), so a hot entity qualified at ~0.33 actual similarity — sampling a real store put the call rate at **94%**.
- Per-conversation ingest time clustered at 2,724–3,238 s, against 600–5,665 s previously. The old spread grew with graph size; the new one does not, because the gate no longer moves with access counts.

Extraction, not dedup, remains the dominant cost (~2,500 tokens per chunk vs ~600 per dedup call). This removed the growth term, not the baseline.

## Honest limitations

- **n=9.** One question is 11 percentage points. 77.8% means "seven of nine", not a rank.
- **Single run per point**, no variance estimate, no cross-validation.
- **Not comparable to published figures.** Zep reports 63.8% on the full 500-question LongMemEval with a different answer stack; this is a 10-question subset. It is a before/after on identical questions, which is what it is designed to be, and nothing more.
- **The 50-question run remains unrun.** Specced, scripted, deterministic subset selection shared with this one; deferred on cost.
- **Full-context and grep baselines not measured** — required before any published comparison against other systems.
- The two remaining failures are **not retrieval failures**: Recall@full is 1.000, so evidence was retrieved for all nine. One is cross-session aggregation (counted 2 of 3 items), one is personalization (gave generic advice instead of using known possessions). Both were classified "present but unused" at baseline and remain so.

## Reproduce

```
scripts/lme-reingest.sh      # ingest (API cost)
scripts/lme-v2-measure.sh    # answer + judge + retrieval (subscription)
```
