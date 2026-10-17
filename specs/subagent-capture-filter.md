# Spec — Subagent transcripts are not sessions: capture, hooks and batch archive skip them

**Status:** draft (RE-74)
**Target version:** 4.6.2 → 4.6.3 (bug fix, no surface change)
**Drafted:** 2026-10-17
**Baseline:** `main` @ `86c7590` (v4.6.2 + docs)
**Issue:** #74

---

## Goal

A Claude Code subagent's transcript is never archived as a conversation, never rolled into
EPHEMERAL.md, and never ingested into the graph — on every path that turns a Claude Code
transcript into an archive: the daemon's capture sweep, `recall-echo ingest`,
`recall-echo archive --all-unarchived`, the SessionEnd hook (`archive-session`) and the
PreCompact hook (`checkpoint`). Main-session archiving is unchanged.

## Why this matters

Verified on the VPS 2026-10-17 (4.6.2):

- **156 of 251 archives are subagent runs** (`session_id: agent-<id>`, first one
  `conversation-015`, 2026-09-15), most with `message_count: 2`.
- **All 156 carry `source: "claude-code"`** — the transcript-adapter path (daemon capture sweep
  or `recall-echo ingest`). None carries `jsonl` (the SessionEnd hook) or `precompact`. The
  issue title blames `archive-session`; the observed culprit is discovery.
- **Discovery takes every `*.jsonl`.** `ClaudeCodeTranscripts::discover`
  (`src/transcript/claude_code.rs`) walks `projects/` to depth 4 via `walk_files`. Claude Code
  writes each subagent to `projects/<proj>/<session>/subagents/agent-<id>.jsonl` (depth 2), and
  the file stem `agent-<id>` becomes the session id.
- **EPHEMERAL is crowded out.** Three of the five slots were parallel subagents of one parent
  session, so SessionStart surfaced worker prompts instead of the user's last real sessions.
- **Provenance is wrong.** A subagent's first "user" turn is the parent *model's* prompt, but
  `jsonl::parse_transcript` records it as `UserMessage` — `user` evidence to the confidence
  model (contract in `src/transcript.rs`). Every subagent archive enters model-authored text
  into the graph as something the human asserted.
- **`archive --all-unarchived` has its own walker** (`archive::find_jsonl_files`, recursive,
  unbounded) with the same flaw, and resolves its store as `paths::claude_dir()` — the pre-4.x
  layout `~/.claude/conversations`. On a 4.x install (`~/.claude/memory/conversations`) it
  either fails with "conversations/ directory not found" or writes into a legacy store that
  nothing reads.

---

## Design

### Detection — `jsonl::is_subagent_transcript(path) -> bool`

Survey of this machine's transcripts (2026-10-17): 147 subagent files, 45 main.

| Signal | Subagent (147) | Main (45) |
| --- | --- | --- |
| Parent directory named `subagents` | 147 | 0 |
| First entry *carrying* `isSidechain` has it `true` | 147 | 0 (37 `false`, 8 absent) |
| First **line** has `isSidechain: true` | 102 — the 45 forks start with `fork-context-ref` | 0 |
| Line index of that first flagged entry | ≤ 2 | ≤ 8 |

The predicate is **true if either** holds:

1. the file's parent directory is named exactly `subagents` (no I/O), or
2. among the first 64 non-empty lines, the first entry that has an `isSidechain` field has it
   `true`. Unparseable lines are skipped, as `parse_transcript` skips them. A file that cannot
   be opened is decided by signal 1 alone.

Either signal suffices: the path is free and matches today's layout; the content flag is Claude
Code's own sidechain marker and survives a layout change or a subagent file handed to a hook
from anywhere. The 64-line bound means a multi-megabyte transcript is never read to the end to
decide. "First line" is explicitly **not** the test — it misses every fork.

The predicate lives in `jsonl.rs` because knowledge of Claude Code's format lives there. Codex
and Grok adapters are untouched.

### Where it applies

```
 daemon sweep ─┐
               ├─▶ capture::pending ─▶ ClaudeCodeTranscripts::discover ──[filter]──▶ archive
 ingest ───────┘

 archive --all-unarchived ─▶ ClaudeCodeTranscripts::discover(None) ──[filter]──▶ archive
                             (replaces find_jsonl_files; store = resolved_hook_base_dir)

 SessionEnd ─▶ archive-session ─▶ classify ─▶ HookTarget::Subagent ─▶ stderr note, exit 0
 PreCompact ─▶ checkpoint ───────▶ subagent? ─▶ stderr note, exit 0, nothing numbered
```

- **Discovery.** `ClaudeCodeTranscripts::discover` drops subagent transcripts after the
  watermark filter, so only files newer than the watermark are sniffed. A dropped file is not a
  session: it is not `active`, not a duplicate, not a failure, and cannot stall the watermark.
- **SessionEnd.** `classify` gains `HookTarget::Subagent`, checked after `Absent` and before
  `ClaudeJsonl`. `run_with_hook_input` prints one stderr line —
  `recall-echo: <path> is a subagent transcript (part of a session, not one), nothing archived`
  — and returns `Ok(())`. No hook-produced subagent archive has been observed; the guard covers
  a hand-run `archive-session` and any Claude Code version that fires SessionEnd for a
  background agent.
- **PreCompact.** `checkpoint::run_from_hook_with_paths` returns before numbering or writing
  when the hook input names a subagent transcript, with the same stderr shape. Checkpoints with
  no hook input are unchanged.
- **`archive --all-unarchived`.** Discovery goes through `ClaudeCodeTranscripts` (one walker,
  one filter; `find_jsonl_files` is deleted), and the store resolves like `ingest`'s
  (`paths::resolved_hook_base_dir(None)`), so the command writes where every reader looks. No
  settle time and no watermark — it stays "everything not yet archived".

### Out of scope

- **Removing what is already written** (156 archives, their ARCHIVE.md rows and graph
  episodes). Archives are immutable by contract, and episode removal is a graph operation;
  this wants its own `prune` design. Follow-up issue. EPHEMERAL heals by itself: the next five
  real sessions push the subagent entries out of the window.
- **Folding a subagent into its parent's archive.** The parent transcript already records the
  Agent tool call (`ToolUse`) and, for foreground agents, its result (`ToolResult`).
- **Duplicate archives of main sessions** (19 session ids archived 2–4 times, mixing `jsonl`,
  `claude-code` and `precompact` sources). Separate issue.
- **Orphaned transcripts** (`<uuid>.orphaned-<ts>-<hash>.jsonl`) and their session ids.
- Codex / Grok sub-agent equivalents (none known).

---

## Acceptance criteria

Happy path
- AC1: Given a projects tree with `<proj>/<session>.jsonl` and
  `<proj>/<session>/subagents/agent-<id>.jsonl`, when the capture sweep or
  `recall-echo ingest --from claude-code` runs, then only the session is archived: no
  `agent-<id>` conversation file, ARCHIVE.md row, EPHEMERAL entry or graph episode.
- AC2: Given the same tree, when `recall-echo archive --all-unarchived` runs, then only the
  session is archived, into the store the hooks resolve (`<root>/memory/conversations`), not
  `~/.claude/conversations`.
- AC3: Given a SessionEnd payload whose `transcript_path` is a subagent transcript, when
  `archive-session` runs, then nothing is archived, one stderr line names the path as a
  subagent transcript, and the exit status is 0.
- AC4: Given a PreCompact payload whose `transcript_path` is a subagent transcript, when
  `checkpoint` runs, then no conversation file, index row or graph episode is written, one
  stderr line says why, and the exit status is 0.
- AC5: Non-subagent transcripts archive exactly as in 4.6.2 — existing archive, capture and
  hook tests pass unchanged.

Edge
- AC6: A fork transcript (first line `fork-context-ref` without `isSidechain`, later entries
  `isSidechain: true`) is detected by content alone, even outside a `subagents/` directory.
- AC7: A transcript inside a `subagents/` directory with no `isSidechain` field anywhere is
  detected by path alone.
- AC8: A main transcript whose first `isSidechain` entry is `false`, or that has none — including
  one whose first 8 lines are metadata entries (`ai-title`, `last-prompt`, `mode`,
  `queue-operation`) — is not detected.
- AC9: Detection reads at most 64 non-empty lines of a transcript.
- AC10: A sweep over a tree whose only new files are subagent transcripts reports nothing
  active, archived, duplicated or failed; the next real session still advances the watermark.

Failure
- AC11: A transcript that cannot be opened is decided by its path alone; existing handling of
  unreadable or vanished transcripts (sweep `failed`, hook `Absent` / `Unreadable`) is unchanged.
- AC12: Malformed JSON lines inside the sniff window are skipped, not fatal.

Tests
- Unit tests in `jsonl.rs` drive `is_subagent_transcript` over temp files: subagent, fork,
  subagents-dir-without-flag, main with metadata preamble, main without any flag, malformed
  lines, flag beyond line 64, unopenable path (AC6–AC9, AC11, AC12).
- `transcript/claude_code.rs`: discovery over a tree with a session, a subagent and a fork
  returns only the session (AC1).
- `tests/capture.rs`: `ingest` over that tree writes one archive and one EPHEMERAL entry; a
  subagent-only tree reports nothing and leaves the watermark for the next real session (AC1,
  AC10).
- `archive.rs`: `classify` on a subagent transcript is `Subagent`, and `run_with_hook_input`
  writes nothing (AC3); `archive_all` over the fixture tree writes one archive into
  `<root>/memory/conversations` (AC2).
- `checkpoint.rs`: a subagent hook input writes no file and no index row (AC4).
- AC5 is the existing suite, unchanged.
