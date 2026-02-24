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
- This contains a summary of the last session. It gives you immediate context about what happened recently.
- At the start of a session, read it to orient yourself, then clear the file (empty it). It has been consumed.
- At the end of a session, write a fresh summary of the current session to this file.
- Contents should include: date, key topics discussed, decisions made, action items, unresolved threads.

### Layer 3 — Long-Term Memory (archive logs)
- Index: `~/.claude/ARCHIVE.md`
- Logs: `~/.claude/memories/archive-log-XXX.md` (sequentially numbered: 001, 002, 003...)
- This is your full history. NOT loaded into context — search it on demand using Grep.
- Each archive log is a checkpoint of a conversation or portion of a conversation.
- ARCHIVE.md is a lightweight index: sequence number, date, and key topics per entry.
- To search history, use: `Grep pattern="search term" path="~/.claude/memories/"`

## Session Lifecycle

### On session start:
1. MEMORY.md is already in your context (auto-loaded).
2. Read EPHEMERAL.md (imported above). Use it to orient — what happened last time?
3. Clear EPHEMERAL.md by writing an empty string to it. It's been consumed.

### During the session:
- Update MEMORY.md when you learn stable facts (user preferences, project decisions, confirmed patterns).
- Do NOT update MEMORY.md with session-specific or speculative information.
- If you need historical context, search the archive: `Grep pattern="topic" path="~/.claude/memories/"`

### On PreCompact (context about to be compressed):
If you see a system message indicating context compaction is imminent:
1. Determine the next archive log number by checking `~/.claude/memories/` for the highest existing number.
2. Write a checkpoint to `~/.claude/memories/archive-log-XXX.md` with:
   - Date and timestamp
   - Trigger: "precompact"
   - Summary of the conversation so far
   - Key decisions, code changes, and unresolved items
3. Add an entry to `~/.claude/ARCHIVE.md` with the log number, date, and key topics.

### On session end:
When the conversation is wrapping up (user says goodbye, task is complete, or you sense the session is ending):
1. Write EPHEMERAL.md with a rich summary of this session.
2. Save a final archive log to `~/.claude/memories/archive-log-XXX.md` with:
   - Date and timestamp
   - Trigger: "session-end"
   - Full session summary
   - Action items and unresolved threads
3. Update ARCHIVE.md index.

## Archive Log Format

Each archive log file should follow this structure:

```
# Archive Log XXX

- **Date**: YYYY-MM-DD HH:MM
- **Trigger**: precompact | session-end
- **Session context**: [brief description of what this session was about]

## Summary
[What was discussed, decided, and accomplished]

## Key Details
[Important specifics — code changes, configurations, decisions with rationale]

## Action Items
[What needs to happen next]

## Unresolved
[Open questions or threads to pick up later]
```

## Rules

- Never write duplicate information to MEMORY.md. Check first, update if exists.
- EPHEMERAL.md is always either empty (mid-session) or contains the last session's summary (between sessions). Never both.
- Archive logs are immutable once written. Never modify an existing archive log.
- When MEMORY.md approaches 200 lines, proactively distill it. Move detailed notes to topic files.
- The memory system is yours. Use it actively — don't wait to be asked.
