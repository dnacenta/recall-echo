# Assembly Fixes — Measured Effect (N=10, same stores, no re-ingest)

**Date**: 2026-08-05 · **Branch**: feat/RE-37-answer-assembly
**Compares**: `baseline-2026-08.md` (the Phase 0 provisional baseline) against the same ten ingested stores answered with the assembly-fixed binary.
**Cost**: zero. Ingest was not re-run; only answer + judge + retrieval scoring. This isolates the two fixes that act at query time (D1 archive channel, D3 episode top-k) from the one that does not (D2 abstract cap, which only affects newly-ingested stores).

## Result

| Metric | Baseline | After fixes | Δ |
|---|---|---|---|
| **Answer accuracy (headline, n=9)** | **0.0%** | **55.6%** | **+55.6** |
| Abstention (n=1) | 100% | 0% | −100 |
| Recall@1 | 0.380 | 0.491 | +0.111 |
| Recall@3 | 0.648 | 0.833 | +0.185 |
| Recall@5 | 0.676 | 0.861 | +0.185 |
| Recall@full | 0.676 | **1.000** | +0.324 |
| MRR | 0.778 | 0.903 | +0.125 |

Per-category answer accuracy after: knowledge-update 100%, single-session-assistant 100%, temporal-reasoning 67%, multi-session 33%, single-session-preference 0%, abstention 0%.

Five of nine answerable questions flipped from wrong to correct: `gpt4_af6db32f`, `9d25d4e0`, `18bc8abd`, `4388e9dd`, `993da5e2`. Assembled context roughly doubled on every question (e.g. 1,424 → 3,077 tokens), which is the intended effect of reviving a channel that was returning nothing and of sampling episode search where its index is stable.

**Recall@full reaching 1.000** is the clearest single signal: every answerable question now retrieves its evidence session. The two questions the context-presence analysis classified as "absent from retrieval" were absent only from the *graph* channel — the archive held them, and the archive channel was dead.

## The regression, stated plainly

`29f2956b_abs` went from **correct to incorrect**. It asks how much time the user spends practising violin; the gold answer is that they never mentioned violin, only guitar. With a starved context the system correctly said it lacked the information. With richer context it answered **"30 minutes"** — the guitar practice duration, attached to the wrong instrument.

This is the predictable cost of more context: additional material creates additional opportunities to construct a plausible but false link. It arrives as a confident false positive rather than a miss, which is the worse failure mode for a memory system, and it should be treated as a first-class problem rather than rounding error at n=1. The likely fix is instruction-level (an explicit licence to answer "you never told me this") rather than retrieval-level, and it needs its own measurement.

## What this does and does not establish

**Establishes**: the Phase 0 conclusion was right. Assembly, not retrieval quality, was the bottleneck — the store already held the answer in 9 of 9 failures, and simply delivering it moved accuracy from nothing to a majority.

**Does not establish**:
- **D2 (1,000-char abstracts) is untested.** These ten stores still hold 200-character abstracts; the fix only applies to new ingests. `18bc8abd` improved *despite* its rank-1 episode still reading `"...my favourite is Ka..."`.
- **The dedup cost fix is untested.** It acts at ingest, which was not re-run. Its counters exist so that a re-ingest reports the answer instead of inferring it.
- **n=9.** One question is 11 percentage points. Treat 55.6% as "clearly better than zero", not as a rank against published figures. For scale only, and not comparable: Zep reports 63.8% on the full 500-question LongMemEval with a different answer stack.
- Single run, no variance estimate; judge is LongMemEval's official per-type prompt via claude-code/sonnet, as in the baseline.

## Reproduce

```
scripts/lme-postfix.sh          # answer + judge + retrieval against runs/lme-postfix
```
Requires the ten ingested stores under `runs/lme-mini/entities/` and a binary built with `--features bench,llm`.
