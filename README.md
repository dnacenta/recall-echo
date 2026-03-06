# recall-echo

[![License: AGPL-3.0](https://img.shields.io/github/license/dnacenta/recall-echo)](LICENSE)
[![Version](https://img.shields.io/github/v/tag/dnacenta/recall-echo?label=version&color=green)](https://github.com/dnacenta/recall-echo/tags)

Persistent three-layer memory system for pulse-null entities. Gives AI agents long-term recall across sessions — curated facts, recent session context, and searchable conversation archives.

## Why

LLM coding agents start every session from zero. Built-in memory is typically a single flat file with no session continuity, no short-term vs long-term distinction, and no searchable history. Memory management that depends on the agent remembering to save things is circular.

recall-echo makes the memory lifecycle mechanical. When running as a pulse-null plugin, archival and checkpointing happen automatically. The agent writes to MEMORY.md during sessions. Everything else is handled by the system.

## Architecture

recall-echo provides a three-layer memory model:

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
│  └───────────────┘  Appended on archive, auto-trimmed │
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
├── MEMORY.md                 # Layer 1 — curated facts (≤200 lines)
├── EPHEMERAL.md              # Layer 2 — rolling session window (default 5)
├── ARCHIVE.md                # Layer 3 — conversation index
├── conversations/            # Layer 3 — full conversation archives
│   ├── conversation-001.md
│   ├── conversation-002.md
│   └── ...
└── .recall-echo.toml         # Optional configuration
```

## How It Works

recall-echo operates in two modes:

### As a pulse-null Plugin

recall-echo is a native pulse-null plugin implementing the `Plugin` trait from echo-system-types. It fills the required **Memory** role (exactly one per entity).

- pulse-null calls `archive::archive_session()` at session end — creates a conversation archive with LLM-generated summary, updates ARCHIVE.md index, appends to EPHEMERAL.md
- pulse-null calls `checkpoint::create_checkpoint()` before context compaction — preserves conversation state before details are lost
- Health checks report memory directory state (Healthy / Degraded / Down)
- Setup wizard prompts for entity_root during `pulse-null init`

```rust
use recall_echo::RecallEcho;

// pulse-null creates the plugin via factory:
let plugin = recall_echo::create(&config, &ctx).await?;
// plugin.role() == PluginRole::Memory
```

### As a Standalone CLI

For administration and use outside pulse-null:

```bash
recall-echo init [entity_root]         # Create memory directory structure
recall-echo status [entity_root]       # Health check with dashboard
recall-echo search <query>             # Line-level archive search
recall-echo search <query> --ranked    # File-ranked relevance search
recall-echo distill [entity_root]      # Analyze MEMORY.md, suggest cleanup
recall-echo consume [entity_root]      # Output EPHEMERAL.md content
```

## Installation

### cargo install

```bash
cargo install recall-echo
recall-echo init
```

### Prebuilt binaries

Download from [GitHub Releases](https://github.com/dnacenta/recall-echo/releases/latest) for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin` (Apple Silicon)

```bash
tar xzf recall-echo-<target>.tar.gz
./recall-echo init
```

### From source

```bash
git clone https://github.com/dnacenta/recall-echo.git
cd recall-echo
cargo build --release
./target/release/recall-echo init
```

## Commands

### `recall-echo init`

Create the memory directory structure under entity_root. Creates `memory/` with MEMORY.md, EPHEMERAL.md, ARCHIVE.md, and `conversations/`. Idempotent — never overwrites existing files.

### `recall-echo status`

Health check with a dashboard showing memory usage, ephemeral state, archive count, recent sessions, and health assessment. Color-coded bars show MEMORY.md capacity (green → yellow → red at 75% / 90%).

```
recall-echo — healthy

  MEMORY.md:    142/200 lines (71%)
  EPHEMERAL.md: 3 entries
  Archives:     23 conversations
```

### `recall-echo search`

Search conversation archives.

```bash
recall-echo search "auth middleware"              # line-level matches
recall-echo search "auth middleware" -C 3          # with 3 lines of context
recall-echo search "auth middleware" --ranked      # ranked by relevance
recall-echo search "auth middleware" --ranked --max-results 5
```

Ranked search scores files by match count, word coverage, and recency.

### `recall-echo distill`

Analyze MEMORY.md and suggest cleanup. Identifies sections over 30 lines that could be extracted to topic files (e.g., `memory/debugging.md`) with references left in MEMORY.md.

### `recall-echo consume`

Output EPHEMERAL.md content wrapped in memory markers. Used by hooks or scripts that need to inject recent session context into an agent's input.

## Archive Format

Conversation archives use YAML frontmatter with markdown content:

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
Summary of the conversation with key outcomes.

**Decisions**: Chose JWT for authentication.
**Action Items**: Implement token refresh endpoint.

### User
(message content)

### Assistant
(message content)

## Tags
**Files**: src/auth.rs, src/middleware.rs
**Tools**: Read, Edit, Bash
```

Summaries are LLM-generated when a provider is available (via pulse-null), with silent fallback to algorithmic extraction.

## Configuration

Optional `.recall-echo.toml` in the memory directory:

```toml
[ephemeral]
max_entries = 5    # Rolling window size (1-50, default 5)
```

All settings have sensible defaults. Missing file or invalid values fall back silently.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[AGPL-3.0](LICENSE)
