# Phase 0 — Unblock Measurement

**Status**: In progress
**Date**: 2026-08-04
**Parent roadmap**: `/opt/pulse-vault/pulse-null/specs/todo/recall-echo-v4-roadmap-spec.md` (Phase 0 of 5)
**Doctrine**: "No number, no claim" — every later phase (confidence rework, retrieval modernization) must land as a measured before/after. This phase makes measurement possible.

## Problem

Three blockers prevent any measured claim about recall-echo today:

1. **Pending fixes unreleased.** The SessionEnd hook fix (`src/archive.rs`: exit 0 when transcript is missing — the `claude -p --no-session-persistence` case) sits uncommitted; the bench Cargo feature (commit `9491daa`) sits unreleased past the v3.11.0 tag. Downstream (Echo's scheduler) depends on a `|| true` workaround.
2. **Exclusive lock.** Embedded SurrealKV takes a process-exclusive file lock. Two concurrent invocations hard-fail with a raw LOCK error; this killed the only benchmark pilot run. Server-mode store code exists but is compile-time mutually exclusive with embedded. The constraint is documented nowhere.
3. **Zero benchmark numbers.** The LoCoMo harness (`/opt/recall-echo-locomo/`) produced one prediction, and it was the LOCK error. LoCoMo itself is discredited (6.4% wrong answer key; judge accepts up to 63% of wrong answers); LongMemEval is the credible target. The hybrid recall path (`src/graph/query.rs`) has zero tests.

## Work items

### 1. Release v3.11.1
- Commit the `archive.rs` hook fix (already in working tree, verified needed).
- Delete stray `history.txt`.
- Add a CI test job (`cargo test` + clippy) — the release workflow currently builds only.
- Tag `v3.11.1`, let CI build binaries, publish to crates.io. Release notes cover: hook fix, bench feature.
- Ships as its own early PR from this branch, merged and tagged before serve work continues — the hook fix is blocking Echo today and the release must not carry unfinished daemon code.

### 2. Runtime server mode + transparent auto-start
- Make embedded-vs-server a **runtime** decision (config + CLI flag), not a compile-time feature split. Both backends compiled in by default. Mechanism: `surrealdb::engine::any` (`surrealkv://` or `ws://` chosen from config) — also enables pointing at an external SurrealDB server (benchmark harness, VPS).
- `recall-echo serve` subcommand: long-running daemon owning the DB, listening on a unix socket (default) with idle auto-shutdown (configurable timeout, default 15 min). Protocol: command-level JSON over the socket (the SurrealDB SDK cannot serve its own wire protocol); the daemon holds the single `FastEmbedder`, eliminating per-invocation ONNX reload.
- **Transparent auto-start**: CLI commands and hooks try the socket first; if absent, spawn the daemon in the background and proceed. No user ever needs to run `serve` manually. (D's decision 2026-08-04: no opt-in.)
- Embedded direct-open remains as fallback (config-disable of serve) and gains lock detection: friendly error naming the constraint + bounded retry/backoff instead of a raw SurrealKV LOCK panic.
- Document the concurrency model in README (currently zero mentions).
- One `FastEmbedder` instance lives in the daemon → eliminates per-invocation ONNX model reload as a side effect.

### 3. Benchmark baseline
- Golden-set fixtures + tests for `query.rs` (hybrid recall path — currently untested).
- Run LongMemEval (headline) + LoCoMo (legacy, labeled as such) on the current stack (BGE-Small, dense-only), via server mode.
- Report **retrieval metrics (recall@k, MRR) separately from answer accuracy**; include full-context and filesystem/grep baselines; publish judge prompts.
- Results land in `docs/benchmarks/baseline-2026-08.md` — the reference every later phase diffs against.

## Acceptance criteria

### Happy path
- [ ] `v3.11.1` tagged; CI release drafted with binaries; crates.io published; SessionEnd hook with a missing transcript exits 0 with an explanatory note.
- [ ] Two concurrent `recall-echo graph search` invocations both succeed (via shared daemon) — the pilot's LOCK failure mode is gone.
- [ ] First CLI/hook invocation with no daemon running transparently starts one and completes; daemon idle-shuts-down after the configured timeout.
- [ ] Full LongMemEval run completes and `docs/benchmarks/baseline-2026-08.md` exists with retrieval metrics, answer metrics, and both baselines.
- [ ] `query.rs` golden-set tests pass in CI.

### Edge cases
- [ ] Stale socket (daemon crashed): client detects, cleans up, respawns, completes.
- [ ] `serve` disabled by config: embedded path works as before, and a lock collision yields the friendly retry/error, not a raw LOCK panic.
- [ ] Concurrent auto-start race (two clients, no daemon): exactly one daemon wins; both clients complete.

### Failure modes
- [ ] Daemon cannot start (port/socket denied): clear error + automatic embedded fallback; no silent hang.
- [ ] Benchmark harness against a genuinely locked embedded DB: named, actionable error — never a panic.

## Out of scope

Confidence/provenance changes (Phase 1), retrieval changes (Phase 2), MCP server (Phase 3 — but `serve` is designed so MCP can mount on it), default-feature changes (Phase 3).
