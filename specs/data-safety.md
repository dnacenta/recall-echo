# Spec — Data safety: redaction, capture consent, hook-flow tests, export/import/rebuild, forget

**Status:** draft
**Target version:** 5.0.0
**Drafted:** 2026-09-02
**Baseline:** `main` @ `dc4f24d` (v4.3.0), on top of rename, trust-boundary, front-door
**Closes:** #44

---

## Goal

A user can trust pulse-recall with every transcript on their machine: secrets never reach
disk or a provider, nothing is captured they did not agree to, the data can be exported,
rebuilt and deleted, and the core capture path is proven by tests rather than by hope.

## Why this matters

Audit 2026-09-02: no redaction exists anywhere in archive, jsonl, capture or ingest.
Capture defaults on for every detected CLI and background extraction defaults on, so a stock
install copies every Claude, Codex, Grok and Gemini transcript on the machine to disk and
posts chunks to the configured provider, pasted `.env` files included. Codex's native
memory redacts secrets; this does not. There is no export, no rebuild, no deletion, and the
on-disk store is tied to SurrealDB 3.2 with no escape hatch. `archive::run_from_hook` and
`checkpoint::run_from_hook` have no end-to-end test. #44's scan predicate is un-indexable and
runs on every daemon poll tick.

---

## Design

### 1. Redaction (structural, not optional)

Module `redact`. Applied once, at the transcript → `Conversation` boundary, so archives,
graph, MCP and the extraction provider all see redacted text. There is no flag to disable it;
`[capture] redact_extra = ["regex", …]` lets users add patterns.

Detectors (each yields a kind):

| Kind | Pattern |
| --- | --- |
| `key` | known prefixes: `sk-`, `sk-ant-`, `sk-proj-`, `ghp_`, `gho_`, `ghu_`, `ghs_`, `github_pat_`, `xox[abpr]-`, `AKIA[0-9A-Z]{16}`, `AIza[0-9A-Za-z_-]{35}`, `npm_`, `pypi-`, `glpat-`, `hf_`, `r8_`, `sq0`, `SG.` |
| `pem` | `-----BEGIN [A-Z ]*PRIVATE KEY-----` … `-----END … -----` blocks |
| `jwt` | `eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+` |
| `bearer` | `(?i)(authorization:\s*)?bearer\s+[A-Za-z0-9._~+/=-]{16,}` |
| `assign` | `(?i)\b(api[_-]?key|secret|token|password|passwd|pwd|client[_-]?secret|access[_-]?key)\b\s*[=:]\s*["']?([^\s"']{12,})` — value part only |
| `url-cred` | `://user:pass@` userinfo in URLs |
| `entropy` | tokens of ≥32 chars from `[A-Za-z0-9+/=_-]` with Shannon entropy > 4.0 bits/char, only when preceded by `=`, `:`, or whitespace after an `assign`-style key |

Replacement: `[REDACTED:<kind>:<first 8 hex of sha256(value)>]` — repeats are recognisable,
values unrecoverable. Counts per kind are reported by `capture` on stderr and stored in the
archive frontmatter (`redacted: {key: 2, jwt: 1}`) so `status`/`doctor` can show totals.

Fixtures use obviously fake values (e.g. `sk-ant-FAKE0000…`), never real material.

### 2. Capture consent

- Interactive `init`: one multi-select question listing detected CLIs, all pre-checked,
  "Capture transcripts from: [x] Claude Code [x] Codex [x] Grok [x] Gemini". The answer is
  written to `[capture.sources]`.
- Non-interactive `init` (no tty, or `--yes`): captures only from the CLI chosen for
  extraction unless `--capture all` or `--capture codex,grok` is given.
- `init` prints, in both modes, what will be captured and that background extraction will
  send redacted chunks to `<provider>`; the same text is `docs/data-and-privacy.md`.
- `config set capture.sources …` edits it later.

### 3. Hook-flow integration tests

`tests/hook_flow.rs`: fixture transcripts for claude-code (JSONL), codex (rollout JSONL),
grok (chat_history.jsonl), gemini (session JSON), each containing one fake secret of each
kind. For each: run `capture` with the hook stdin JSON → assert `conversations/NNN.md`
exists with frontmatter, ARCHIVE.md gained one line, EPHEMERAL.md gained one entry, no fake
secret appears anywhere under `memory/`, the redaction count matches, a second run is a
no-op (front-door AC10). Same for `checkpoint --trigger precompact`.

### 4. Export / import / rebuild

- `graph export [--out file.jsonl]` — one JSON object per line: `{"kind":"entity",…}`,
  `{"kind":"relationship", alpha, beta, self_count, provenance…}`, `{"kind":"episode", …
  without embedding}`. Deterministic order. Header line with schema version and pulse-recall
  version.
- `graph import <file.jsonl> [--replace]` — merge by name+type (entities), by
  from/rel/to (relationships, summing evidence), by log_number+chunk (episodes);
  `--replace` empties the store first (asks). Re-embeds what it imports.
- `graph rebuild [--extract]` — empties the store, re-ingests every archive under
  `conversations/`, optionally re-runs extraction. This is the SurrealDB-major-upgrade path
  and the "forget then rebuild" path.

### 5. Forget

- `forget --session <log N | session-id>` — removes the archive file, its ARCHIVE.md line,
  its EPHEMERAL.md entry, and its episodes. Prints what went. Entities and relationships
  extracted from it are **not** traced (edges carry no per-episode link); the command says
  so and names `graph rebuild --extract` as the way to a graph without that session.
- `forget --all` — removes everything under `memory/` except the config file; asks; `--yes`
  required without a tty.

### 6. #44 — indexable extraction scan

Schema version 2: backfill `UPDATE episode SET extracted = false WHERE extracted IS NONE`,
`DEFINE INDEX IF NOT EXISTS episode_extracted ON episode FIELDS extracted`, predicate back
to `extracted = false`. `extracted_absent` diagnostic goes to zero by design.

### 7. Disk visibility

`status` shows model cache size, store size, archives size, and disk free.

---

## Acceptance criteria

- AC1: For every detector kind, a fixture string is replaced by `[REDACTED:<kind>:<8hex>]` and the same value twice yields the same marker (unit tests per kind; negative cases: a 40-char English sentence, a git SHA in a `commit` line, a UUID — not redacted).
- AC2: Hook-flow tests pass for all four CLIs as described in §3, including the no-secret-on-disk grep.
- AC3: Redaction cannot be disabled by config; `redact_extra` adds a pattern that is then applied.
- AC4: Non-interactive `init` with claude-code and codex detected captures only claude-code; `--capture all` captures both (config assertion).
- AC5: `graph export` followed by `graph import --replace` into an empty store yields identical `graph status` counts and identical `what-do-you-know` output.
- AC6: `graph rebuild` on a store with 3 archives yields exactly the episodes those archives produce; `--extract` re-runs extraction through the configured provider (mock).
- AC7: `forget --session 2` removes conversation-002.md, its ARCHIVE.md line, its EPHEMERAL.md entry and its episodes, and prints the rebuild hint; a following `graph rebuild` produces a store with no trace of log 2.
- AC8: `forget --all` without tty and without `--yes` refuses.
- AC9: After migration to schema 2, no episode has `extracted = NONE`, the index exists, and `pending()` uses the indexed predicate (query plan or explain assertion).
- AC10: `status` prints the four size lines.
- AC11: Existing tests pass.

## Out of scope

Redacting inside already-written 4.x archives (a `capture --sweep --re-redact` may follow).
Per-edge provenance to episodes (would let `forget` be exact; separate spec).

## Verification

`cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`. Gate: a pasted fake
key in a fixture transcript never reaches disk or the mock provider.

## Delivery

Branch `feat/RE-<n>-data-safety`. Ships in v5.0.0.
