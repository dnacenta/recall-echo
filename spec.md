# recall-echo — Spec v2.0

## What It Is

recall-echo is a persistent three-layer memory system for pulse-null entities. It gives AI agents long-term recall across sessions — curated facts, recent session context, and searchable conversation archives. Designed as a native pulse-null plugin (Memory role) with standalone CLI support for administration.

Inspired by MemGPT (arxiv:2310.08560) — event-driven memory management for LLMs.

## The Problem

LLM coding agents start every session from zero. Built-in memory systems are typically a single flat file with no concept of short-term vs long-term memory, no session summaries, and no searchable history. Memory management that depends on the agent choosing to save things is circular — it forgets to remember.

recall-echo makes the entire memory lifecycle mechanical. When integrated with pulse-null, archival and checkpointing happen automatically via the plugin system. The agent writes to MEMORY.md during sessions. Everything else is enforced by the system.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│              MEMORY ARCHITECTURE                      │
│                                                       │
│  Layer 1: CURATED (always in context)                 │
│  ┌───────────┐                                        │
│  │ MEMORY.md │  Facts, preferences, patterns          │
│  └───────────┘  Distilled & maintained by the agent   │
│                                                       │
│  Layer 2: SHORT-TERM (FIFO rolling window)            │
│  ┌───────────────┐                                    │
│  │ EPHEMERAL.md  │  Last N session summaries          │
│  └───────────────┘  Appended on archive, trimmed      │
│                                                       │
│  Layer 3: LONG-TERM (searched on demand)              │
│  ┌─────────────┐    ┌────────────────────────────┐    │
│  │ ARCHIVE.md  │───→│ conversations/             │    │
│  └─────────────┘    │  conversation-001.md       │    │
│                     │  conversation-002.md       │    │
│                     │  ...                       │    │
│                     └────────────────────────────┘    │
│                     YAML frontmatter + markdown       │
│                     LLM-summarized or algorithmic     │
└──────────────────────────────────────────────────────┘
```

All paths are relative to an entity root directory:

```
{entity_root}/memory/
├── MEMORY.md                    # Layer 1 — curated facts
├── EPHEMERAL.md                 # Layer 2 — rolling session window
├── ARCHIVE.md                   # Layer 3 — conversation index
├── conversations/               # Layer 3 — full archives
│   ├── conversation-001.md
│   ├── conversation-002.md
│   └── ...
└── .recall-echo.toml            # Optional configuration
```

## Dual-Mode Operation

recall-echo operates in two modes:

### 1. pulse-null Plugin (Primary)

As a native pulse-null plugin implementing the `Plugin` trait from echo-system-types:

- **Role**: `PluginRole::Memory` (required — exactly one per entity)
- **Factory**: `create(config, ctx) -> Box<dyn Plugin>`
- **Health**: Reports memory directory state (Healthy / Degraded / Down)
- **Setup**: Prompts for entity_root during pulse-null init wizard
- **Lifecycle**: pulse-null calls `archive::archive_session()` and `checkpoint::create_checkpoint()` automatically

The plugin does not contribute tools, scheduled tasks, or HTTP routes. It is a data layer — pulse-null orchestrates when archival and checkpointing happen.

### 2. Standalone CLI

For administration and use outside pulse-null:

```
recall-echo init [entity_root]       # Create memory directory structure
recall-echo status [entity_root]     # Health check with dashboard
recall-echo search <query>           # Line-level archive search
recall-echo search <query> --ranked  # File-ranked search with relevance scoring
recall-echo distill [entity_root]    # Analyze MEMORY.md, suggest cleanup
recall-echo consume [entity_root]    # Output EPHEMERAL.md content
```

## Layer Details

### Layer 1 — Curated Memory (MEMORY.md)

The source of truth. Distilled facts, user preferences, patterns, key decisions. Always loaded into agent context at session start.

- Lives at: `{entity_root}/memory/MEMORY.md`
- Size discipline: Keep under 200 lines
- Updated: During conversations when stable facts are confirmed
- Distillation: `recall-echo distill` analyzes heavy sections and suggests extracting them to topic files (e.g., `memory/debugging.md`)

### Layer 2 — Short-Term Memory (EPHEMERAL.md)

A FIFO rolling window of recent session summaries. Gives the agent immediate context about recent work.

- Lives at: `{entity_root}/memory/EPHEMERAL.md`
- Max entries: Configurable (default 5, range 1–50)
- Format: Separator-delimited entries with session ID, date, duration, message count, summary, and archive pointer
- Updated: Automatically when a session is archived

### Layer 3 — Long-Term Memory (conversations/)

Full conversation archives with structured metadata. Not loaded into context — searched on demand via Grep.

- Index: `{entity_root}/memory/ARCHIVE.md` (markdown table)
- Archives: `{entity_root}/memory/conversations/conversation-NNN.md`
- Format: YAML frontmatter + markdown sections

## Archive Format

Each conversation archive includes structured metadata and content:

```yaml
---
log: 5
date: "2026-03-06T10:30:00Z"
session_id: "abc123"
message_count: 34
duration: "30m"
source: "session"
topics: ["auth", "jwt", "middleware"]
---

## Summary
LLM-generated summary of the conversation.

**Decisions**: Key decisions made during the session.
**Action Items**: Follow-up tasks identified.

### User
(message content)

### Assistant
(message content)

## Tags
**Decisions**: decided to use JWT for auth
**Files**: src/auth.rs, src/middleware.rs
**Tools**: Read, Edit, Bash
```

Archives are created with two source types:
- `session` — end-of-session archive (appends to EPHEMERAL.md)
- `checkpoint` — pre-compaction checkpoint (does not append to EPHEMERAL.md)

## Summarization

recall-echo supports two summarization strategies:

1. **LLM-enhanced** (when an `LmProvider` is available via pulse-null): Structured JSON output with summary, topics, decisions, and action items. Conversations are condensed to ~4000 characters before sending to the model.

2. **Algorithmic fallback** (standalone or when LLM is unavailable): Keyword extraction with stop-word filtering and tool-target boosting. First user message as summary. No decisions or action items extracted.

Fallback is silent — if the LLM call fails, algorithmic extraction runs automatically.

## Tag Extraction

Archives include heuristic-based structured tags:

- **Decisions**: Detected via linguistic markers ("decided to", "let's use", "chose to")
- **Action items**: Detected via markers ("todo:", "need to", "follow up")
- **Files touched**: Extracted from tool inputs (paths containing `/` or `.`)
- **Tools used**: Collected from tool use entries in the conversation

Max: 5 decisions, 5 action items, 10 files per archive.

## Plugin Integration

### echo-system-types Dependency

recall-echo depends on echo-system-types v0.4.0 for:
- `Plugin` trait — lifecycle, health, meta, role, setup prompts, `as_any()`
- `PluginContext` — entity_root, entity_name, `Arc<dyn LmProvider>`
- `PluginRole::Memory`
- `HealthStatus` — Healthy, Degraded(reason), Down(reason)
- `Message`, `ContentBlock` — conversation types for archival

### Factory Pattern

```rust
pub async fn create(
    config: &serde_json::Value,
    ctx: &PluginContext,
) -> Result<Box<dyn Plugin>, Box<dyn Error + Send + Sync>>
```

Config accepts `entity_root` (string). Falls back to `ctx.entity_root` if not specified. Returns a fully initialized `RecallEcho` instance — no two-phase init.

### Health Checks

- **Healthy**: memory/, conversations/, and MEMORY.md all exist
- **Degraded**: memory/ exists but MEMORY.md or conversations/ missing
- **Down**: memory/ directory not found

## Configuration

Optional `.recall-echo.toml` in the memory directory:

```toml
[ephemeral]
max_entries = 5
```

All settings have sensible defaults. Missing file or invalid values fall back silently.

## Technology

- **Language**: Rust
- **License**: AGPL-3.0
- **Dependencies**: echo-system-types, clap, serde, serde_json, dirs
- **Zero external deps** for YAML, TOML, and date handling (hand-rolled parsers)
- **Dev dependencies**: tempfile, tokio

## Testing

80 tests covering:
- Archive operations (creation, indexing, numbering)
- Checkpoint functionality
- Conversation processing (flattening, markdown rendering, topic extraction)
- Ephemeral FIFO window (append, trim, parse)
- Dashboard rendering and health assessment
- Search (line-level and ranked)
- Summarization (algorithmic)
- Status reporting
- Tags and frontmatter parsing

All tests run in isolated temporary directories — no production memory touched.

## Resolved Decisions

1. **Entity-root model**: All paths relative to entity_root, not hardcoded home dir. Supports multi-entity scenarios.
2. **Markdown archives**: Replaced JSONL with markdown + YAML frontmatter. Human-readable, Grep-searchable.
3. **FIFO ephemeral**: Rolling window of N entries (default 5) instead of single session summary.
4. **LLM summaries with fallback**: Graceful degradation when no provider available.
5. **No hooks or rules**: pulse-null manages lifecycle. recall-echo is purely a data layer + CLI admin tool.
6. **Zero external date/YAML/TOML deps**: Hand-rolled parsers for minimal bloat.
7. **Plugin role**: Memory (required, exactly one). No tools, no tasks, no routes contributed.
8. **Async throughout**: Factory and archival functions are async to match pulse-null's architecture.
