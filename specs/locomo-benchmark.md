# Spec: LoCoMo Benchmark Harness for recall-echo

**Status**: PROPOSED
**Scope**: Out-of-tree harness at `/opt/recall-echo-locomo/`; minor library exposure inside `/opt/recall-echo/`
**Author**: Vigil (drafted) + D (to approve)
**Date**: 2026-05-13

## Problem

recall-echo has no published benchmark number. Competitors do:

- **mem0** reports 91.6 (LLM-as-judge "J" metric, GPT-4o-mini) on LoCoMo across 1,540 questions, 5 categories.
- **Zep/Graphiti** reports 94.8% on DMR (different benchmark). On LoCoMo itself the methodology is disputed — Zep originally claimed 84%, mem0 recalculated 58.44%, Zep counter-claimed 75.14%. Both vendors contest each other's setup.
- **MemMachine, Engram, MemoryLake, Memobase, Backboard** all publish LoCoMo runs.

LoCoMo (snap-research/locomo, arxiv 2402.17753) is the de-facto agent-memory yardstick. Without a number, recall-echo can't be compared. The goal is **one credible, reproducible score** — not chart-topping. The harness must be honest about methodology so we don't end up in a Zep-vs-mem0 dispute.

## Goal

Produce a reproducible LoCoMo run that:

1. Ingests the 10 LoCoMo conversations as recall-echo archives + knowledge-graph extractions.
2. Answers the 1,540 QA pairs using recall-echo as the retrieval layer behind an LLM agent.
3. Scores results with the same LLM-as-judge protocol mem0 publishes (GPT-4o-mini, binary J), so numbers are comparable.
4. Also reports the original LoCoMo F1 (Maharana et al.) so the dispute is sidestepped.
5. Produces a markdown report with per-category breakdown, token/latency costs, and a methodology section.

## Non-Goals

- Event summarization task (LoCoMo task 2). QA only.
- Multimodal dialogue generation (LoCoMo task 3). Text only.
- Beating mem0 or Zep. We want a defensible number, not a marketing claim.
- Multi-run statistical analysis (mem0 does 10 runs ± stddev). v1 is single-run.
- DMR benchmark — separate spec if useful later.

## Current State

| Component | File | Status |
|-----------|------|--------|
| `archive::archive_session` | `src/archive.rs` | Public, takes JSONL transcript path |
| `graph::ingest::ingest_archive` | `src/graph/ingest.rs` | Public, takes archive markdown path |
| `graph::extract` (LLM extraction) | `src/graph/extract.rs` | CLI-only, `--log <N>` per archive |
| `graph::query::hybrid_query` | `src/graph/query.rs` | Public, semantic + expansion + episodes |
| `search::ranked_search` | `src/search.rs` | Public, archive line search |
| `recall_echo::config::Config` | `src/config.rs` | Loads `.recall-echo.toml` |
| LLM provider abstraction | `src/llm_provider.rs` | Anthropic + OpenAI + Ollama + claude-code |
| LoCoMo dataset | external | CC BY-NC 4.0, `data/locomo10.json` upstream |
| LoCoMo eval harness | external | `task_eval/evaluate_qa.py` (F1) — Python |
| mem0 J-metric harness | external | `mem0/evaluation/metrics/llm_judge.py` — Python |

**Gap**: no Rust-side "drive a conversation through recall-echo programmatically as if it were N entity sessions, then ask M questions" entry point. We need one, but a thin Python harness shelling out to the `recall-echo` CLI is acceptable — and matches how mem0's harness drives its own Python SDK.

## Design

### Layout

A new sibling repo, not inside `/opt/recall-echo/` (LoCoMo data is CC BY-NC, harness pulls in Python deps, separate concern):

```
/opt/recall-echo-locomo/
├── README.md
├── pyproject.toml                 # uv-managed Python env
├── data/
│   ├── locomo10.json              # downloaded, gitignored (license)
│   └── .gitkeep
├── harness/
│   ├── __init__.py
│   ├── ingest.py                  # LoCoMo conv → recall-echo archives
│   ├── answer.py                  # QA pipeline using recall-echo
│   ├── judge.py                   # GPT-4o-mini J-metric (mem0-compatible)
│   ├── f1.py                      # Original LoCoMo F1 (port of snap-research)
│   ├── report.py                  # Markdown report generator
│   └── cli.py                     # Typer/argparse entry
├── runs/                          # per-run outputs (gitignored)
│   └── 2026-05-XX/
│       ├── predictions.jsonl
│       ├── judged.jsonl
│       ├── scores.json
│       └── report.md
└── Makefile                       # make ingest / answer / score / report
```

Inside `/opt/recall-echo/`, expose two new library functions so the harness doesn't shell out 1,540 times:

- `recall_echo::bench::ingest_conversation(entity_root, conversation_id, sessions)` — accept a LoCoMo `sessions` vec directly, write per-session archives with the correct timestamps, run graph ingest + extract.
- `recall_echo::bench::answer_question(entity_root, question, provider)` — wraps `graph::hybrid_query` + ranked archive search + MEMORY.md read, builds prompt, calls LLM, returns answer + retrieval trace.

Gated behind a `bench` Cargo feature so production builds don't ship it. Bench feature also exposes a `recall-echo bench` CLI subcommand (`bench answer <question>`, `bench ingest <conv-json>`) so the Python harness can call it cleanly via subprocess.

### Data Flow

```
┌────────────────────────────────────────────────────────────────────┐
│  LoCoMo conv (locomo10.json sample_id=conv-1)                      │
│  ~35 sessions, ~300 turns, ~150 QA pairs                           │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ harness/ingest.py
┌────────────────────────────────────────────────────────────────────┐
│  Per-conv entity root: runs/<date>/entities/conv-1/memory/         │
│  - One archive per session, dated session.timestamp                │
│  - ARCHIVE.md, EPHEMERAL.md populated as if entity ran live        │
│  - graph/surreal/ embedded DB                                      │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ recall-echo graph extract --all
┌────────────────────────────────────────────────────────────────────┐
│  KG populated: entities, relations, episodes, Bayesian confidence  │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ harness/answer.py (for each QA)
┌────────────────────────────────────────────────────────────────────┐
│  recall-echo bench answer "<question>"                             │
│  → graph hybrid_query + ranked archive search → context bundle     │
│  → LLM (configurable; default GPT-4o-mini for parity)              │
│  → predicted_answer + retrieval_trace                              │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ harness/judge.py + harness/f1.py
┌────────────────────────────────────────────────────────────────────┐
│  J-metric (GPT-4o-mini, 0/1 binary)                                │
│  F1-metric (token overlap, normalized)                             │
│  Per-category aggregates (single/multi/temporal/open/adversarial)  │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ harness/report.py
┌────────────────────────────────────────────────────────────────────┐
│  runs/<date>/report.md with scores, methodology, costs             │
└────────────────────────────────────────────────────────────────────┘
```

### Why Python harness driving a Rust binary

- LoCoMo upstream tooling is Python; staying compatible with their JSON shape costs nothing.
- mem0's harness is Python; reusing their judge prompt verbatim avoids prompt-drift disputes.
- recall-echo's interesting work (graph, retrieval, ingestion) stays in Rust where it lives.
- Python only orchestrates: read JSON, call `recall-echo bench …`, collect predictions, judge, aggregate.

## Phases

### Phase 1 — Harness scaffold + dataset ingestion

**Goal**: one LoCoMo conversation ingested end-to-end with a populated graph.

Tasks:

1. Bootstrap `/opt/recall-echo-locomo/` with `pyproject.toml`, uv lockfile, Makefile, `.gitignore` (exclude `data/`, `runs/`).
2. Add `bench` Cargo feature to `/opt/recall-echo/Cargo.toml`. Behind feature: `src/bench/mod.rs`, `src/bench/ingest.rs`, `src/bench/answer.rs`. Wire `bench` subcommand into `src/main.rs`.
3. Write `harness/ingest.py`:
   - Parse `data/locomo10.json` (10 conversations).
   - For each conversation, create `runs/<date>/entities/<sample_id>/`.
   - Map sessions → archive files. Use session timestamp as `date:` frontmatter. Each session becomes one archive (`conversation-NNN.md`) with speakers as `### User`/`### Assistant` blocks.
   - Shell out: `recall-echo bench ingest --entity-root <path> --conv-json <session-json>` per session.
   - After all sessions: `recall-echo graph extract --all --provider openai --model gpt-4o-mini`.
4. `make ingest CONV=conv-1` works end-to-end. `recall-echo graph status` shows entities + relationships.

Effort: **2 days** (1 day Rust bench feature + library, 1 day Python ingest + dataset wrangling).

Deliverable: one conv ingested, graph populated, archives readable.

### Phase 2 — Question-answering pipeline

**Goal**: answer all 1,540 QA pairs using recall-echo retrieval.

Tasks:

1. Implement `recall_echo::bench::answer_question`:
   - Call `graph::query::hybrid_query(question, depth=2, episodes=true, limit=20)`.
   - Fall back to `search::ranked_search` for archive lines (top-5).
   - Read MEMORY.md (curated layer, but for LoCoMo it'll be empty — that's fine, we document it).
   - Compose prompt: system instruction + retrieved facts + retrieved episodes + question.
   - Call LLM via `llm_provider`. Return `BenchAnswer { answer, retrieved_facts, retrieved_episodes, tokens_in, tokens_out, latency_ms }`.
2. `recall-echo bench answer --entity-root <path> --question "<q>" --model gpt-4o-mini` → JSON to stdout.
3. `harness/answer.py`:
   - Iterate `qa[]` for each conversation in `locomo10.json`.
   - Skip `category == "adversarial"` answers where ground-truth is "no answer" — still ask, judge separately (these are intentionally unanswerable).
   - Parallel: 10 concurrent (matches mem0's default).
   - Write `runs/<date>/predictions.jsonl`: `{sample_id, qa_id, category, question, gold_answer, predicted_answer, tokens, latency}`.
4. `make answer CONV=all` produces predictions.jsonl with 1,540 rows.

Effort: **3 days** (1.5 day Rust answer pipeline + prompt engineering, 1.5 day Python orchestration + rate limiting + checkpointing for resume).

Deliverable: predictions.jsonl complete. Spot-check 20 answers manually before phase 3.

### Phase 3 — Scoring + result reporting

**Goal**: J and F1 scores with per-category breakdown.

Tasks:

1. `harness/judge.py`:
   - Port mem0's judge prompt verbatim from `mem0/evaluation/metrics/llm_judge.py`. Cite source in comment.
   - Model: `gpt-4o-mini`. Temperature 0. Binary output 0/1.
   - Input: question, gold_answer, predicted_answer. Output: `{score: 0|1, reasoning}`.
   - Parallel 20-wide.
   - Write `runs/<date>/judged.jsonl`.
2. `harness/f1.py`:
   - Port `task_eval/evaluation.py` F1 from snap-research/locomo. Same normalization (lowercase, strip punctuation, strip articles, whitespace-tokenize, set overlap → F1).
   - Compute per QA, write into judged.jsonl.
3. `harness/report.py`:
   - Aggregate by category: single-hop, multi-hop, temporal, open-domain, adversarial.
   - Emit `scores.json` and `report.md` with: overall J, overall F1, per-category J + F1, total tokens, total cost (rough $), median + p95 latency, methodology section.
4. `make score` and `make report`.

Effort: **2 days** (judge port + F1 port + report rendering).

Deliverable: `runs/<date>/report.md` with a credible number.

### Phase 4 — Comparison report vs mem0/Graphiti

**Goal**: a public-facing one-pager comparing recall-echo to published competitors.

Tasks:

1. Add comparison table to report.md using public numbers:

   | System | J overall | Single | Multi | Temporal | Open | Adv | Source |
   |--------|-----------|--------|-------|----------|------|-----|--------|
   | mem0 (token-efficient) | 91.6 | 76.6 | 92.3 | 93.3 | 70.2 | 57.3 | mem0.ai/research-2 |
   | Zep | disputed | — | — | — | — | — | arxiv 2501.13956 + issue #5 |
   | recall-echo | <ours> | … | … | … | … | … | this run |

2. Methodology disclosure section in report:
   - Judge model + prompt SHA.
   - Answer model.
   - Embedding model (FastEmbed default: bge-small-en-v1.5).
   - Graph config (depth, limit, episode count).
   - LoCoMo data version (commit SHA of snap-research/locomo).
   - Total LLM cost.
3. Write `/opt/recall-echo/docs/benchmarks/locomo.md` summarizing the result with a link to the run output and the harness repo.
4. Optionally: README badge `LoCoMo J: <score>%`.

Effort: **1 day** (writing + cross-checking competitor numbers).

Deliverable: shareable result, link-ready, with methodology that survives scrutiny.

## Total Effort

**8 days end-to-end** (~64 hours). Realistic with one engineer focused; double it if interleaved.

| Phase | Days | LLM $ rough |
|-------|------|-------------|
| 1 — Ingest | 2 | ~$5 (extraction across 10 convs) |
| 2 — Answer | 3 | ~$15 (1,540 q × gpt-4o-mini) |
| 3 — Score | 2 | ~$3 (judge runs) |
| 4 — Report | 1 | $0 |

Total LLM cost per full run: **~$25** using GPT-4o-mini end-to-end. Re-runs cheap.

## Open Questions

1. **Should we use Anthropic models (Sonnet) for answer LLM to match recall-echo's primary provider?** Then we're not apples-to-apples with mem0's GPT-4o-mini. Decision: run **both** — primary = GPT-4o-mini for comparability, secondary = Claude Sonnet 4.7 for "what does recall-echo actually do in production?" Report both. ~$50 total.
2. **MEMORY.md curation during ingestion?** LoCoMo doesn't simulate a curating agent. Leave MEMORY.md empty and document the limitation, or auto-distill from extracted graph entities? Default: leave empty (faithful to "raw memory" baseline). Note in methodology.
3. **One entity per conversation, or one entity for all 10?** Cross-conv contamination is bad. Default: one entity root per conv, scrub between.
4. **Adversarial handling.** Some adversarial QA expect "unanswerable" / refusal. Judge prompt must handle this — mem0's prompt does, we inherit it.
5. **Re-extraction on re-run.** Graph extraction is non-deterministic. Cache extracted graph by `(conv_id, extract_model, prompt_hash)` to keep re-runs cheap and comparable.

## Risks

- **Cost overrun.** GPT-4o-mini at 1,540 questions × multiple retrievals × judge × possibly Sonnet rerun. Mitigation: per-phase budget gate (`make answer LIMIT=50` for dev runs). Hard ceiling $200/run; abort if exceeded.
- **Methodology dispute (Zep vs mem0 redux).** Publish judge prompt SHA, dataset SHA, full code; refuse to claim a single number out of context. Always present J alongside F1 alongside cost.
- **LoCoMo license (CC BY-NC 4.0).** Non-commercial. Harness is fine; results are fine to publish. Don't bundle dataset in our repo — download script only.
- **Graph extraction latency.** 10 convs × ~35 sessions × extraction ≈ 350 LLM calls during phase 1. With `--delay-ms 200` and concurrency 10, ~15 minutes. Acceptable.
- **Bench feature drift from production code paths.** If `bench::answer_question` reimplements retrieval instead of reusing existing CLI paths, scores reflect the harness, not recall-echo. Mitigation: `bench::answer_question` is a thin wrapper around `graph::hybrid_query` + `search::ranked_search` + `llm_provider` — same calls a real agent would make. PR review should enforce this.
- **Judge model deprecation.** GPT-4o-mini may be retired. Pin OpenAI model version (`gpt-4o-mini-2024-07-18` style); document in report.

## Acceptance Criteria

- `make all` runs ingest → answer → score → report end-to-end on a single command.
- `runs/<date>/report.md` exists with overall J, F1, per-category breakdown, methodology disclosure.
- Score landed: **target band 60-85% J** (honest expectation; mem0's 91.6% is best-in-class with a tuned algorithm; we want a respectable first number, not a #1 claim).
- Harness reproducible from clean checkout in <2 hours of wall time.
- One paragraph in `/opt/recall-echo/docs/benchmarks/locomo.md` linking to the run + methodology.

## References

- LoCoMo paper: https://arxiv.org/abs/2402.17753
- LoCoMo repo: https://github.com/snap-research/locomo
- mem0 paper: https://arxiv.org/abs/2504.19413
- mem0 eval framework: https://github.com/mem0ai/mem0/tree/main/evaluation
- mem0 91.6% writeup: https://mem0.ai/research-2
- Zep paper: https://arxiv.org/abs/2501.13956
- Zep/mem0 methodology dispute: https://github.com/getzep/zep-papers/issues/5
- MemMachine LoCoMo blog: https://memmachine.ai/blog/2025/09/memmachine-reaches-new-heights-on-locomo/
