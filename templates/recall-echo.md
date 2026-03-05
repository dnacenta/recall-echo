# recall-echo — Memory Protocol

You have a persistent three-layer memory system. Use it to maintain continuity across sessions.

## Memory Layers

### Layer 1 — Curated Memory (MEMORY.md)
- Location: `~/.claude/memory/MEMORY.md`
- Your source of truth. Distilled facts, preferences, patterns, key decisions.
- Auto-loaded at session start (first 200 lines).
- Keep under 200 lines. Only write confirmed, stable information.
- Before adding, check if an existing entry should be updated. No duplicates.

### Layer 2 — Recent Sessions (EPHEMERAL.md)
@~/.claude/EPHEMERAL.md
- Rolling window of your last 5 session summaries.
- Read at session start to orient on recent work.
- Each entry has a pointer to the full archive.
- Managed automatically by recall-echo hooks. Do not edit manually.

### Layer 3 — Full Archive (conversations/)
- Index: `~/.claude/ARCHIVE.md`
- Full conversations: `~/.claude/conversations/conversation-NNN.md`
- NOT loaded into context. Search on demand using Grep.
- To search: `Grep pattern="search term" path="~/.claude/conversations/"`

## Session Lifecycle

### On session start:
1. MEMORY.md is in your context (auto-loaded).
2. EPHEMERAL.md is in your context (via @ import above).
3. Orient from recent sessions. Use archive pointers if you need full context.

### During the session:
- Update MEMORY.md when you learn stable facts.
- When the user references past work, search the archive first.
- Do NOT update MEMORY.md with speculative or session-specific info.

### On PreCompact (context about to be compressed):
The PreCompact hook automatically runs `recall-echo checkpoint --trigger precompact`.
The output tells you the file path and log number. Open that file and fill in the
Summary, Key Details, Action Items, and Unresolved sections with context from the
current conversation.

### On session end:
- The SessionEnd hook archives this conversation automatically.
- No manual action required.

## Commands

- `recall-echo init` — Initialize or upgrade the memory system
- `recall-echo consume` — Output EPHEMERAL.md at session start (PreToolUse hook)
- `recall-echo checkpoint --trigger precompact` — Save checkpoint before context compression
- `recall-echo archive-session` — Archive conversation from JSONL transcript (SessionEnd hook)
- `recall-echo archive --all-unarchived` — Batch archive all missed sessions
- `recall-echo search <query>` — Search conversation archives
- `recall-echo search <query> --ranked` — Ranked search with relevance scoring
- `recall-echo distill` — Analyze MEMORY.md and suggest cleanup
- `recall-echo status` — Memory system health check

## Rules

- Never write duplicates to MEMORY.md. Check first, update if exists.
- When MEMORY.md approaches 200 lines, distill it.
- Archive conversations are immutable. Never modify them.
- When the user says "we discussed this before" — search archives before saying you don't remember.
