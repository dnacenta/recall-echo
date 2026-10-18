{{MARKER}}
# recall-echo — Memory Protocol

You have a persistent four-layer memory, kept by recall-echo in `{{MEMORY_DIR}}/`. Use it to keep continuity across sessions.

## Memory Layers

### Layer 0 — Knowledge graph
- Entities, relationships (each with a Bayesian confidence) and conversation episodes, built automatically from archived sessions.
- Read it with the recall-echo MCP tools. There are six, all read-only: `recall_query` first, then `recall_search`, `recall_episodes`, `recall_traverse`, `recall_overview` and `recall_status`. The CLI equivalent is `recall-echo graph query|search|traverse`.
- Query it before answering anything that depends on earlier sessions and is not in MEMORY.md.

### Layer 1 — Curated memory (MEMORY.md)
{{MEMORY_IMPORT}}

- `{{MEMORY_DIR}}/MEMORY.md`, imported above into every session. Your source of truth: distilled facts, preferences, patterns and decisions.
- Keep it under 200 lines. Move detail into topic files beside it and link them from MEMORY.md; `recall-echo distill` suggests what to move.
- Only confirmed, stable information. Before adding an entry, check whether an existing one should be updated instead.

### Layer 2 — Recent sessions (EPHEMERAL.md)
- `{{MEMORY_DIR}}/EPHEMERAL.md`: a rolling window of recent session summaries, each pointing at its archive file.
- The SessionStart hook prints it into context on startup, resume and `/clear`. The SessionEnd hook writes it — never edit it by hand.

### Layer 3 — Full archive
- `{{MEMORY_DIR}}/conversations/conversation-NNN.md`, indexed in `{{MEMORY_DIR}}/ARCHIVE.md`. Immutable.
- Not loaded into context. Search it on demand: `recall-echo search "<query>" --ranked`, or grep `{{MEMORY_DIR}}/conversations/`.

## Session Lifecycle

- Claude Code hooks do the plumbing. SessionStart runs `recall-echo consume`. PreCompact runs `recall-echo checkpoint`, which writes the conversation so far into the archive. SessionEnd runs `recall-echo archive-session`, which archives the session, rolls EPHEMERAL.md and feeds the graph. Nothing needs archiving by hand.
- When the user refers to earlier work, check MEMORY.md, then the graph, then the archive before saying you don't remember.

## Rules

- No duplicates in MEMORY.md — update the existing entry.
- Archive conversation files are immutable.
- When MEMORY.md nears 200 lines, distill it into topic files.
- Never put secrets (keys, tokens, passwords) in MEMORY.md, and never add `@` lines to it. It is imported into every session, and Claude Code follows the imports inside it.
