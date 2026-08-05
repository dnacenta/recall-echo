# Context-Presence Analysis — August 2026 (AC1, Phase 2)

**Analysis date**: 2026-08-05 · **Analyses**: `/opt/recall-echo-locomo/runs/lme-mini/` (the N=10 LongMemEval baseline of 2026-08-04, see `baseline-2026-08.md`)
**Question**: the baseline retrieved the right evidence session 68% of the time (Recall@full 0.676, MRR 0.778) and answered 0 of 9 answerable questions correctly. Where did the gold-supporting facts go?
**Answer, in one line**: they were retrieved and then thrown away by fixed top-k limits and a 200-character episode cap before the model ever saw them.

## Classification scheme

Each of the 9 answerable questions is assigned exactly one class, by the *earliest blocking cause* in the pipeline:

| Class | Meaning | Operational test |
|---|---|---|
| **A** — absent from retrieval | No gold-supporting fact came back from the graph at any depth | Not present in a `graph query --limit 200 --depth 2 --episodes` result for the same question |
| **B** — dropped in assembly | Retrieved, but never reached the prompt: top-k cutoff, truncation, or ordering | Present in the limit-200 result, absent from the assembled prompt |
| **C** — present but unused | It was in the assembled prompt and the model still answered wrong | Present in the assembled prompt |

## Result

| Class | n | Share |
|---|---|---|
| **A — absent from retrieval** | **2** | 22% |
| **B — dropped in assembly** | **5** | 56% |
| **C — present but unused** | **2** | 22% |

Even the two A cases are A only with respect to the *graph* channel. Both facts sit verbatim in the entity's own `conversations/` archive — a channel that is structurally unreachable today (see "The archive channel is dead", below). Counting the archive as a channel that exists but is broken, **9 of 9 failures are cases where the evidence was in the store and the assembly failed to deliver it.**

## Per-question table

| # | id | type | gold | predicted | class | evidence for the call |
|---|---|---|---|---|---|---|
| 1 | `9d25d4e0` | multi-session | 3 | *"I don't have enough information to answer."* | **B** | Needs 3 acquisitions. Emerald earrings (fact #11) and engagement ring (fact #9, episode #4 — with its "got it a month ago" anchor) **were** in the prompt. The third, a silver necklace, was not — but the entity `Silver Necklace` sits at **rank 34** of the limit-200 query, i.e. just below the `graph_limit 20` cutoff. |
| 2 | `0a995998` | multi-session | 3 | "2 items: the exchanged boots from Zara … and the navy blue blazer at the dry cleaner" | **C** | Every gold evidence turn's content was in the prompt: Zara boots (fact #5, episode #1) and navy blazer (fact #13). The model read them correctly and returned 2. See the gold-ambiguity caveat below — the oracle's three evidence turns describe only two distinct items. |
| 3 | `2ebe6c92` | temporal-reasoning | *'The Nightingale'* by Kristin Hannah | *"I don't have enough information to answer."* | **B** | "Nightingale" appears **nowhere** in the prompt; the 20 facts are dominated by an unrelated European-road-trip cluster and the 5 episodes are all "Song of Achilles" chatter. Yet the entity `The Nightingale` is at **rank 29** (>20) and an episode stating *"'The Nightingale' is an amazing book!"* is at **episode rank 7** (>`archive_top_k` 5). Both gold sessions were "retrieved" per the metric (Recall@full 1.00). |
| 4 | `88432d0a` | multi-session | 4 | "**3 times**: Pizza … Sourdough … Convection cookies" | **B** | 3 of 4 bakes present (chocolate cake #1, sourdough, convection cookies #7/#12). The whole-wheat baguette is absent from the prompt but present at **episode rank 138** (*"Congratulations on your successful whole wheat baguette!"*). Compounding: **4 of the 20 fact slots** (#2, #8, #9, #18) are near-duplicate "sourdough came out dense on Tuesday" cases — dedup failure spending a fifth of the budget on one fact. The model also substituted a non-gold bake (pizza) for the cake that *was* present. |
| 5 | `0a34ad58` | single-session-preference | Personalise using the user's *existing* Suica card and TripIt app | Generic Tokyo transit tips: *"**Get a Suica Card** — Load it with at least ¥10,000"*, *"Use TripIt or Google Maps"* | **C** | Suica (facts #2, #3, #12, #19) and TripIt (#5, #19) were both in the prompt, and the model used them — but as generic recommendations ("Get a Suica Card") rather than acknowledging prior preparation, which is exactly what the gold marks as *not* preferred. Retrieval and assembly both did their job. |
| 6 | `gpt4_af6db32f` | temporal-reasoning | 17 days ago | *"I don't have enough information to answer."* | **B** | No Super Bowl content in the prompt: all 5 assembled episodes are **raw YAML frontmatter** (~1 KB of `log:`/`date:`/`session_id:` metadata, zero content). The decisive episodes are session-41 (*"since I won $20 from my colleague after the Super Bowl"*). **They are the true top-3** — but only at k≥20. See the k-sensitivity bug below. Entity `Super Bowl Bet` also exists, at rank 41. |
| 7 | `18bc8abd` | knowledge-update | Kansas City Masterpiece | Sweet Baby Ray's *(the stale, superseded value)* | **B** | The single most decisive case. The **rank-1** episode (score 0.79) reads verbatim: `"… Currently, my favourite is Ka..."` — the 200-char episode cap severs the gold answer **mid-word**. The rank-2 episode carries the stale value, also truncated: `"… my favorite BBQ sauce, Sweet..."`. The string "sweet baby" is not in the prompt at all; the model completed the brand from the 5-character fragment "Sweet" and world knowledge. A third episode stating *"While Kansas City Masterpiece BBQ sauce is a great choice"* sits at **episode rank 15**, cut by `archive_top_k 5`. |
| 8 | `993da5e2` | temporal-reasoning | One week (7–10 days acceptable) | *"I don't have enough information to answer."* | **A** | Needs two anchors: rug acquired "a month ago" and furniture rearranged "three weeks ago". At limit 200: `rearrang*` returns **0 hits**, and no "rug … a month ago" co-occurrence exists. The rug is mentioned (episodes #1, #5) but only as decor context, never with an acquisition date. Both anchors are in the archive (2 and 7 files) — extraction simply never captured them. Retrieval metric says Recall@full **1.00** for this question. |
| 9 | `4388e9dd` | single-session-assistant | "an untidy, stained white shirt" | *"I don't have enough information to answer."* | **A** | At limit 200: `untidy` **0 hits**, `wears` 0 hits; `shirt` hits only unrelated sessions (an undershirt brand, a diaper-change scene). Extraction condensed the long script-writing turn into Cases about Andy's *behaviour* (exaggerated stories, disabled toilet) and dropped the costume detail entirely. Present in `conversation-003.md`. |

*(The 10th row, `29f2956b_abs`, is the abstention control: correctly answered, excluded from the 9 per the harness convention.)*

## The three mechanisms behind the 5 B's

### 1. The archive channel is dead — 0 hits, all 10 questions

`bench answer` is documented as "top-20 graph facts + **top-5 archive snippets** + MEMORY.md". In this run the archive contributed **zero** snippets to **all ten** prompts, despite 44–55 archive files per entity.

Cause: `src/search.rs::ranked_search` requires **every** whitespace-token of the query to appear in a file (`all_words_present`), and the harness prefixes each question with `[Current date: 2023-05-30T15:43:00Z]`. The tokens `[current` and the ISO timestamp appear in **zero** archive files, so the AND-filter eliminates every candidate before scoring. Replicated exactly in Python across all 10 questions: 0 files survive for every question. Punctuation-attached tokens (`bowl?`, `furniture?`, `wrote`) would kill most of the remainder even without the date prefix.

Consequence: the assembled prompt is graph-only. Both A-class questions, and the full text severed in question 7, are recoverable from the archive.

### 2. Episode abstracts are capped at 200 characters

`src/graph/ingest.rs:424` — `chunk.chars().take(200)`. Every episode in every prompt is ≤203 chars; the 5 episodes together contribute ~1 KB regardless of question. This is a *storage* cap, so the full text is not available to the answer path at any k.

This is what makes session-level retrieval metrics misleading. Recall@full 0.676 measures whether the right **session** appeared in the ranked list — but the assembly carries only a 200-character slice of that session. Questions 3, 7 and 8 all scored **Recall@full 1.00** while the answer-bearing sentence never reached the prompt. **Session-level recall says almost nothing about whether the answer got through.**

### 3. Episode search loses recall at small k

Not a cutoff — a correctness bug. For `gpt4_af6db32f`, the same query returns strictly worse results at k=5 than at k=20:

```
graph query --limit 5    top-5 episodes:  0.666  0.660  0.660  0.660  0.659   (all frontmatter, session 27/28/34/29/35)
graph query --limit 20   top-5 episodes:  0.701  0.700  0.690  0.666  0.660   (session-41 x3 — the Super Bowl evidence)
graph query --limit 200  top-5 episodes:  0.701  0.700  0.690  0.666  0.660   (identical to k=20)
```

The k=5 call misses three higher-scoring items that k=20 finds, consistent with an HNSW `ef_search` tied to k. Since `answer.rs` calls `search_episodes(question, archive_top_k)` with `archive_top_k = 5`, the answer path runs the episode index at exactly the k where its recall is worst. Note this did not reproduce for `2ebe6c92` (k=5, 20 and 200 gave identical top-5), so it is graph-dependent, not universal.

### Also observed

- **MEMORY.md is empty (0 bytes) in all 10 entity roots** — the "Curated memory" section never appeared in any prompt. Expected for a benchmark ingest that never runs distillation, but it means one of the three documented context sources contributed nothing and was not measured.
- **Fact-slot redundancy**: question 4 spent 4 of 20 slots on near-duplicate sourdough cases. Raising `graph_limit` without deduplicating will spend the increase on more duplicates.

## What this means for Phase 2

**The spec's ordering assumption is confirmed, but its diagnosis is not.** Phase 2 opens by asserting that the failures are "honest partial assemblies … a reranker cannot fix that", and prioritises **budgeted multi-fact assembly** — raising fact counts, grouping by session, preserving temporal order. The measurement supports keeping work item 1 first, and refutes work item 4 (reranking) as the lever. But the dominant defect is not that the *fact budget* is too small in the abstract. It is that **three plumbing faults silently discard already-retrieved evidence**, and they are all cheap to fix:

1. **Fix `ranked_search`'s all-words AND filter** (and strip the `[Current date: …]` prefix, or match on content tokens only). This restores an entire retrieval channel that is currently contributing zero. It is the only fix that can reach questions 8 and 9 — the two A-class failures — because their evidence exists nowhere else.
2. **Stop truncating episodes to 200 chars in the answer path.** Question 7 is a correct-rank-1 retrieval that returned the wrong answer purely because the gold string was cut mid-word. This is a one-question-in-nine effect on its own.
3. **Raise `archive_top_k` above 5 for episodes, and audit the HNSW `ef_search`/k coupling.** Questions 3, 6 and 7 each have decisive evidence at episode rank 7, 1–3 (only visible at k≥20), and 15 respectively.

Only after that does "budgeted multi-fact assembly, grouped and temporally ordered" become the binding constraint — and question 1 (`Silver Necklace` at rank 34) plus question 4 (baguette at rank 138) show it will still bind. **Raise `graph_limit` together with entity dedup**, not alone: a fifth of question 4's budget already went to duplicates.

The 2 C-class failures are the honest floor. One (question 5) is a genuine prompt/instruction problem — the assembly delivered the right facts and the model used them impersonally; it argues for the spec's "what do you know about X across sessions" prompt shape, not for more retrieval. The other (question 2) may not be a system failure at all.

**Expected value of the cheap fixes**: they address the blocking cause in 5 of 9 questions (all B) and are prerequisites for the 2 A's. They do not guarantee 5 correct answers — questions 1 and 4 also require the model to *count* correctly across many small facts, which it did not do even with 3 of 4 facts present. A realistic AC2 target is a measurable improvement over 0/9, not 7/9.

## Caveats

- **n = 9.** Every share in this document is one question ≈ 11 percentage points. Treat A=2 / B=5 / C=2 as a direction, not a rate. Reclassifying a single question moves the headline by 11 points.
- **No replication divergence in the assembly step.** `predictions.jsonl` stores the serialised `BenchAnswer` including `retrieved_facts` and `retrieved_episodes` verbatim, and `build_user_message` is deterministic given (memory_md, facts, episodes, question). The prompts were reconstructed and verified **byte-exact for all 10 questions** by recomputing `answer.rs`'s `estimate_tokens` (`len().div_ceil(4)` over bytes, system + user) and matching the recorded `tokens_in` exactly. This part of the analysis carries no approximation.
- **The retrieval-probe step does carry a caveat.** `graph query` (`graph_cli::hybrid_query`) and `bench answer` (`answer.rs::retrieve_facts`) both call `GraphMemory::query` with the same `QueryOptions` shape, and the probe used the same question text, `--depth 2` and `--episodes`. But the probe was run **2026-08-05, one day after the run**, against the same on-disk entity roots with the same binary (verified identical by md5: `/root/.cargo/bin/recall-echo` == `/opt/recall-echo/target/release/recall-echo`). If graph state mutated between the run and the probe — e.g. access-count updates from the original query affecting hotness-weighted scoring — deep ranks could shift. Ranks are used only to distinguish "below cutoff" from "absent", and the margins (rank 29, 34, 41, 138 vs a cutoff of 20; rank 7 and 15 vs a cutoff of 5) are wide enough that small drift does not change any classification.
- **Semantic-match judgement calls.** Presence was tested by keyword co-occurrence over the prompt's fact+episode sections with the `## Question` block excluded (the question text otherwise produces false positives — it did for "super bowl" and "rearranged" on first pass). Borderline calls, all resolved by reading the surrounding text: question 8's rug *is* mentioned but never with an acquisition date, so the anchor is absent; question 4's "whole wheat" appears in the prompt only as a *future* baking plan, not the past baguette; question 5's Suica/TripIt are present and used, so it is C despite scoring 0.
- **Gold-label ambiguity on question 2.** The oracle's three evidence turns for `0a995998` describe only two distinguishable items (the Zara boots appear in two sessions; the blazer in one), yet gold is 3. The model's "2" may be defensible. Classified C because all retrievable evidence reached the prompt, but this question is weak grounds for any design decision.
- **Question-type counts differ from `baseline-2026-08.md`.** The oracle types for this subset are 3 temporal-reasoning, 3 multi-session, 1 knowledge-update, 3 single-session (assistant/preference/user-abstention) — matching `retrieval_metrics.json`. The baseline document's "2 multi-session, 2 knowledge-update" is a miscount and should be corrected there.
- **Single run.** No re-run was performed and no LLM answer calls were made for this analysis; the classification rests on stored artifacts plus no-LLM retrieval probes. Whether fixing the B mechanisms actually converts to correct answers is AC2's job, not this document's.

---

*Method and throwaway scripts: prompts reconstructed from `predictions.jsonl`, verified against recorded `tokens_in`; deep-retrieval probes via `recall-echo graph --entity-root <root> query "<question>" --limit 200 --depth 2 --episodes` (no LLM calls); archive behaviour replicated from `src/search.rs::ranked_search`. No product code was modified.*
