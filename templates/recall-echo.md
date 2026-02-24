# recall-echo — Memory Protocol

You have a persistent three-layer memory system. Use it to maintain continuity across sessions.

## Memory Layers

### Layer 1 — Curated Memory (MEMORY.md)
- Location: `~/.claude/memory/MEMORY.md` (and optional topic files alongside it)
- This is your source of truth. Distilled facts, user preferences, project patterns, key decisions.
- It is auto-loaded at session start (first 200 lines).
- Keep it under 200 lines. If it grows beyond that, distill aggressively or move details to topic files (e.g., `debugging.md`, `architecture.md`) in the same directory.
- Only write stable, confirmed facts. Never write speculative or session-specific information here.
- Before adding a new entry, check if an existing entry should be updated instead. No duplicates.

### Layer 2 — Short-Term Memory (EPHEMERAL.md)
@~/.claude/EPHEMERAL.md
- This is a staging area for session summaries. Between sessions, it holds the previous session's summary waiting to be promoted to an archive log.
- At the end of a session, write a fresh summary of the current session to this file.
- Contents should include: date, key topics discussed, decisions made, code changes, action items, unresolved threads.
- At the start of the next session, `recall-echo promote` archives it automatically.

### Layer 3 — Long-Term Memory (archive logs)
- Index: `~/.claude/ARCHIVE.md`
- Logs: `~/.claude/memories/archive-log-XXX.md` (sequentially numbered: 001, 002, 003...)
- This is your full history. NOT loaded into context — search it on demand using Grep.
- Each archive log is a checkpoint of a conversation or portion of a conversation.
- ARCHIVE.md is a lightweight index: sequence number, date, and key topics per entry.
- To search history, use: `Grep pattern="search term" path="~/.claude/memories/"`

## Session Lifecycle

### On session start:
1. Run `recall-echo promote` (via Bash tool) to archive the previous session's EPHEMERAL.md into an archive log.
2. MEMORY.md is already in your context (auto-loaded).
3. If you need context from the last session, read the archive log that was just promoted.

### During the session:
- Update MEMORY.md when you learn stable facts (user preferences, project decisions, confirmed patterns).
- Do NOT update MEMORY.md with session-specific or speculative information.
- If you need historical context, search the archive: `Grep pattern="topic" path="~/.claude/memories/"`

### On PreCompact (context about to be compressed):
The PreCompact hook automatically runs `recall-echo checkpoint --trigger precompact`.
The output tells you the file path and log number. Open that file and fill in the
Summary, Key Details, Action Items, and Unresolved sections with context from the
current conversation.

### On session end:
When the conversation is wrapping up (user says goodbye, task is complete, or you sense the session is ending):
1. Write EPHEMERAL.md with a rich session summary.
   Include: what was discussed, key decisions, code changes, action items, unresolved threads.
2. That's it. The next session will promote it to an archive log automatically.

## Archive Log Format

Archive logs are created by `recall-echo checkpoint` (precompact) or `recall-echo promote` (session end) with YAML frontmatter.

For precompact checkpoints, you fill in the section templates. For promoted logs, the content comes from EPHEMERAL.md automatically.

```yaml
---
log: 5
date: "2026-02-24T21:30:00Z"
trigger: precompact
context: ""
topics: []
---
```

Sections to fill in (precompact only):
- **Summary** — What was discussed, decided, and accomplished
- **Key Details** — Important specifics: code changes, configurations, decisions with rationale
- **Action Items** — What needs to happen next
- **Unresolved** — Open questions or threads to pick up later

Old logs without frontmatter continue to work — numbering is by filename, not content.

## Commands

- `recall-echo init` — Initialize or upgrade the memory system
- `recall-echo checkpoint --trigger precompact [--context "..."]` — Create a precompact archive checkpoint
- `recall-echo promote [--context "..."]` — Promote EPHEMERAL.md into an archive log
- `recall-echo status` — Check memory system health

## Rules

- Never write duplicate information to MEMORY.md. Check first, update if exists.
- EPHEMERAL.md holds the session summary between sessions. It is promoted to an archive log at the start of the next session.
- Archive logs are immutable once written. Never modify an existing archive log.
- When MEMORY.md approaches 200 lines, proactively distill it. Move detailed notes to topic files.
- The memory system is yours. Use it actively — don't wait to be asked.
