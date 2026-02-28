# recall-echo — Spec v2

## What It Is

recall-echo is a persistent memory system for AI coding agents. It gives Claude Code (and eventually other LLM tools) a three-layer memory architecture that survives across sessions. No more cold starts. Your agent remembers what it learned, what happened last time, and can search its full history on demand.

Inspired by MemGPT (arxiv:2310.08560) — event-driven memory management for LLMs.

## The Problem

Every Claude Code session starts from zero. The built-in auto-memory (`MEMORY.md`) is a single flat file capped at 200 lines. There's no concept of short-term vs long-term memory, no session summaries, no searchable archive. Users who want real persistence have to build it themselves — and most don't know how.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│              MEMORY ARCHITECTURE                     │
│                                                      │
│  Layer 1: CURATED (always in context)                │
│  ┌───────────┐                                       │
│  │ MEMORY.md │  Facts, preferences, patterns         │
│  └───────────┘  Distilled & maintained               │
│                                                      │
│  Layer 2: SHORT-TERM (loaded, then cleared)          │
│  ┌───────────────┐                                   │
│  │ EPHEMERAL.md  │  Last session summary             │
│  └───────────────┘  Read → clear → rewrite at end    │
│                                                      │
│  Layer 3: LONG-TERM (searched on demand)             │
│  ┌─────────────┐    ┌────────────────────────────┐   │
│  │ ARCHIVE.md  │───→│ memories/                  │   │
│  └─────────────┘    │  archive-log-001.md        │   │
│                     │  archive-log-002.md        │   │
│                     │  archive-log-003.md        │   │
│                     │  ...                       │   │
│                     └────────────────────────────┘   │
│                     NOT loaded into context           │
│                     Grep-searchable on demand         │
└─────────────────────────────────────────────────────┘
```

## Memory Lives at User Level

All memory is stored under `~/.claude/`, not per-project. The agent is one entity — its memory of who you are, your preferences, and your history doesn't reset when you switch repos. This is user-level persistence.

### Directory Structure After Setup

```
~/.claude/
├── rules/
│   └── recall-echo.md               # Memory protocol (auto-loaded globally)
├── memory/
│   └── MEMORY.md                    # Layer 1 — curated facts (auto-loaded)
├── EPHEMERAL.md                     # Layer 2 — last session summary
├── ARCHIVE.md                       # Layer 3 — index
└── memories/                        # Layer 3 — archive logs
    ├── archive-log-001.md
    ├── archive-log-002.md
    └── ...
```

Nothing touches project directories. No gitignore needed.

## Layer Details

### Layer 1 — Curated Memory (MEMORY.md)

The source of truth. Distilled facts, user preferences, patterns, key decisions. Always loaded into context at session start (first 200 lines via Claude Code's auto-memory system).

- Lives at: `~/.claude/memory/MEMORY.md`
- Size discipline: Keep under 200 lines. If it grows beyond that, distill or move details to topic files.
- Updated: During conversations when stable facts are confirmed. Not speculatively.
- Topic files: Optional `*.md` files alongside MEMORY.md for detailed notes (e.g., `debugging.md`, `architecture.md`). Not auto-loaded but readable on demand. Created organically by the agent as needed — no templates.

### Layer 2 — Short-Term Memory (EPHEMERAL.md)

A rich summary of the last session. Gives the agent immediate context about what just happened. Read at session start, then cleared. Rewritten at session end.

- Lives at: `~/.claude/EPHEMERAL.md`
- Auto-loaded: Via `@` import in the rules file
- Contents: Date, key topics discussed, decisions made, action items, unresolved threads
- Lifecycle:
  1. Session starts → agent reads EPHEMERAL.md (orients from last session)
  2. Agent clears EPHEMERAL.md (consumed — prevents stale context)
  3. Conversation happens
  4. Session ends → agent writes fresh EPHEMERAL.md with current session summary

### Layer 3 — Long-Term Memory (ARCHIVE.md + memories/)

The full history. Not loaded into context (too large), but searchable on demand via Grep. Each memory checkpoint is a sequentially numbered archive log. ARCHIVE.md serves as a lightweight index.

- ARCHIVE.md lives at: `~/.claude/ARCHIVE.md`
- Memory logs live at: `~/.claude/memories/`
- Format: `archive-log-XXX.md` (sequential numbering: 001, 002, 003...)
- Searchable: Agent uses Grep to search `~/.claude/memories/` when it needs historical context
- ARCHIVE.md: Brief entries per log — sequence number, date, key topics

## Event-Driven Archiving

Inspired by MemGPT, memory checkpoints are triggered by system events rather than relying on the agent to "know" when a session ends.

### Trigger Events

1. **PreCompact** — Context is about to be compressed. The agent saves a checkpoint of the current conversation to `memories/archive-log-XXX.md` before details are lost. This is the primary trigger.

2. **Session end** — When the user explicitly ends the session or the conversation reaches a natural close, the agent writes EPHEMERAL.md and saves a final archive log.

3. **Session start** — If EPHEMERAL.md was never written (crash, abrupt exit), the new session can recover by checking the latest archive log.

### Session Lifecycle

```
Session starts
    │
    ▼
Read EPHEMERAL.md (orient from last session)
    │
    ▼
Clear EPHEMERAL.md (consumed, now empty)
    │
    ▼
Conversation happens...
    │
    ├──► PreCompact fires
    │      Save checkpoint → memories/archive-log-004.md
    │      (continue conversation)
    │
    ├──► PreCompact fires again
    │      Save checkpoint → memories/archive-log-005.md
    │      (continue conversation)
    │
    ▼
Session ends
    │
    ▼
Write EPHEMERAL.md (summary for next session)
Save final log → memories/archive-log-006.md
```

### Hook Configuration

The PreCompact hook is configured in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo 'RECALL-ECHO: Context compaction imminent. Save a memory checkpoint to ~/.claude/memories/ before context is lost.'"
          }
        ]
      }
    ]
  }
}
```

This injects a reminder into the agent's context right before compaction, prompting it to save state.

## The Rules File (recall-echo.md)

Lives at `~/.claude/rules/recall-echo.md`. Auto-loaded into every Claude Code session globally. This is the brain of the system.

Contains:

1. **Memory protocol instructions** — How the agent uses each layer
2. **Session start behavior** — Read EPHEMERAL.md, orient, then clear it
3. **During-session behavior** — When and how to update MEMORY.md (stable facts only)
4. **PreCompact behavior** — Save checkpoint to archive log before context is lost
5. **Session end behavior** — Write EPHEMERAL.md, save final archive log
6. **Search protocol** — How and when to search `~/.claude/memories/` for historical context
7. **Size discipline** — Rules for keeping MEMORY.md under 200 lines
8. **Import** — `@~/.claude/EPHEMERAL.md` to pull Layer 2 into context

## CLI Tool

### Installation & Usage

```bash
# Initialize recall-echo
npx recall-echo init
```

### What `init` Does

1. Creates `~/.claude/rules/recall-echo.md` (the memory protocol)
2. Creates `~/.claude/EPHEMERAL.md` (empty)
3. Creates `~/.claude/ARCHIVE.md` (empty with header)
4. Creates `~/.claude/memories/` directory
5. Creates `~/.claude/memory/MEMORY.md` if it doesn't exist (preserves existing)
6. Adds PreCompact hook to `~/.claude/settings.json` (merges with existing hooks, never overwrites)
7. Prints a summary of what was created and how it works

### What `init` Does NOT Do

- Modify existing CLAUDE.md files
- Modify existing MEMORY.md (if one exists, leave it alone)
- Overwrite any existing files (prompt before overwriting)
- Install any dependencies beyond Node.js
- Require any API keys or configuration
- Touch any project directories

### Additional Commands (future, out of scope for v1)

```bash
npx recall-echo status     # Health check
npx recall-echo compact    # Distill MEMORY.md
npx recall-echo search "q" # Search archive
```

## Technology

- **Runtime**: Node.js (npx-compatible)
- **Language**: TypeScript, compiled to JS for distribution
- **Package**: Published to npm as `recall-echo`
- **Dependencies**: Zero external deps. fs, path, readline (Node built-ins only).
- **License**: MIT

## Model Agnosticism

The memory protocol is written in plain English instructions. While v1 targets Claude Code (`.claude/rules/`), the protocol is model-agnostic. Future versions:

```bash
npx recall-echo init --target cursor    # .cursorrules
npx recall-echo init --target windsurf  # Windsurf rules
npx recall-echo init --target claude    # default
```

Out of scope for v1 but architecture supports it.

## Testing Strategy

### Isolated Test Environment

We will NOT test against an existing production setup. Instead:

1. Create a temporary test directory with a fresh `.claude/` structure
2. Run `recall-echo init` against it
3. Verify file structure and contents are correct
4. Manually validate rules file loads in a Claude Code session (separate from production)

### Automated Tests

- Unit tests for `init` command (file creation, settings merge, path resolution)
- Unit tests for archive log sequencing (correct numbering)
- Integration test: init → verify file structure → verify file contents

## Phases

### Phase 1 — Core (v1.0)

- [ ] Write the `recall-echo.md` memory protocol (the rules file)
- [ ] Build the `init` CLI command
- [ ] Template files (EPHEMERAL.md, ARCHIVE.md, MEMORY.md)
- [ ] PreCompact hook configuration
- [ ] README with clear documentation
- [ ] npm package setup
- [ ] Testing in isolated environment

### Phase 2 — Polish (v1.1)

- [ ] `status` command (health check)
- [ ] Better error handling and edge cases
- [ ] Configurable paths via `.recall-echo.json`

### Phase 3 — Multi-Model (v2.0)

- [ ] `--target` flag for Cursor, Windsurf, Aider
- [ ] Model-specific protocol adaptations
- [ ] Plugin architecture for custom targets

## Resolved Decisions

1. **EPHEMERAL.md location**: Inside `~/.claude/` — clean, out of project directories
2. **Log granularity**: Event-driven checkpoints via PreCompact + session end. Sequential `archive-log-XXX.md` files.
3. **Archive trigger**: PreCompact hook (primary), session end (secondary), session start recovery (fallback)
4. **Topic files**: None created by init. Agent creates them organically.
5. **Git tracking**: Not applicable — everything lives in `~/.claude/`, outside any repo.
6. **Memory scope**: User-level, not project-level. One memory across all projects.
7. **Naming**: `memories/` directory, `archive-log-XXX.md` files
8. **EPHEMERAL lifecycle**: Read at session start → clear immediately → rewrite at session end
