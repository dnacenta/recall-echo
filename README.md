# recall-echo

[![License: AGPL-3.0](https://img.shields.io/github/license/dnacenta/recall-echo)](LICENSE)
[![Version](https://img.shields.io/github/v/tag/dnacenta/recall-echo?label=version&color=green)](https://github.com/dnacenta/recall-echo/tags)

Persistent four-layer memory system for pulse-null entities. Gives AI agents long-term recall across sessions — a knowledge graph with Bayesian confidence, curated facts, recent session context, and searchable conversation archives.

## Why

LLM coding agents start every session from zero. Built-in memory is typically a single flat file with no session continuity, no short-term vs long-term distinction, and no searchable history. Memory management that depends on the agent remembering to save things is circular.

recall-echo makes the memory lifecycle mechanical. When running as a pulse-null plugin, archival and checkpointing happen automatically. The agent writes to MEMORY.md during sessions. Everything else is handled by the system.

## Architecture

recall-echo provides a four-layer memory model:

```
┌──────────────────────────────────────────────────────────┐
│              MEMORY ARCHITECTURE                          │
│                                                           │
│  Layer 0: KNOWLEDGE GRAPH (structured, semantic)          │
│  ┌──────────────────────────────────────────────────┐     │
│  │ SurrealDB + FastEmbed                            │     │
│  │ Entities, relationships, episodes                │     │
│  │ Bayesian confidence · Semantic search (HNSW)     │     │
│  │ LLM-powered extraction + deduplication           │     │
│  └──────────────────────────────────────────────────┘     │
│                                                           │
│  Layer 1: CURATED (always in context)                     │
│  ┌───────────┐                                            │
│  │ MEMORY.md │  Facts, preferences, patterns              │
│  └───────────┘  Distilled & maintained by the agent       │
│                                                           │
│  Layer 2: SHORT-TERM (FIFO rolling window)                │
│  ┌───────────────┐                                        │
│  │ EPHEMERAL.md  │  Last N session summaries              │
│  └───────────────┘  Appended on archive, auto-trimmed     │
│                                                           │
│  Layer 3: LONG-TERM (searched on demand)                  │
│  ┌─────────────┐    ┌────────────────────────────┐        │
│  │ ARCHIVE.md  │───→│ conversations/             │        │
│  └─────────────┘    │  conversation-001.md       │        │
│                     │  conversation-002.md       │        │
│                     │  ...                       │        │
│                     └────────────────────────────┘        │
│                     YAML frontmatter + markdown           │
│                     LLM-summarized or algorithmic         │
└──────────────────────────────────────────────────────────┘
```

### Knowledge Graph (Layer 0, default)

The knowledge graph is the structural foundation of recall-echo. It turns conversation archives into structured, searchable memory. Enabled by default via the `graph` feature.

**What it does.** When conversations are archived, recall-echo extracts entities (people, projects, tools, concepts) and the relationships between them, then stores them in an embedded SurrealDB graph database. Semantic search via fastembed embeddings lets agents find relevant memories by meaning, not just keywords — so a search for "authentication" surfaces conversations about JWT, OAuth, and login flows even if those exact words weren't in the query.

**Why Bayesian confidence.** Traditional knowledge graphs store facts as absolutes — "Dani uses NeoVim" is either true or not. But memories aren't binary. Things change, context matters, and some things are more certain than others. recall-echo uses a Beta-Binomial Bayesian confidence model on every relationship edge:

- Each relationship starts with a confidence prior based on how it was established: authoritative (1.0), explicit (0.9), inferred (0.6), or speculative (0.3)
- When new evidence corroborates a relationship, confidence increases. When evidence contradicts it, confidence decreases
- Updates are gradual — it takes ~10 observations to overwhelm the prior, so a single contradictory mention doesn't erase established knowledge
- Multi-hop queries compound confidence along the path, naturally preferring shorter, higher-confidence routes

This means the graph handles contradictions, reinforces patterns over time, and lets uncertain or stale knowledge fade gracefully — instead of requiring manual cleanup or producing false-positive retrievals.

**Entity types:** person, project, tool, service, preference, decision, event, concept, case, pattern, thread, thought, question, observation, policy, measurement, outcome. Mutable types (person, project, tool, etc.) can be updated; immutable types (decision, event, case, etc.) are append-only.

**Extraction pipeline:** When conversations are archived, an LLM-powered pipeline chunks the text (~500 tokens), extracts entities and relationships in parallel (up to 10 concurrent), then deduplicates sequentially with LLM-assisted skip/create/merge decisions. Re-extracted relationships receive Bayesian corroboration updates, so knowledge confirmed across multiple conversations gains confidence automatically.

**Tiered content:** Entities store content at three levels — L0 (abstract, used for embeddings and cheap traversal), L1 (overview, used for reranking), and L2 (full content, pulled on demand). This keeps graph traversal fast.

**Graph commands:**

```bash
# Core
recall-echo graph init                          # Initialize the graph store
recall-echo graph status                        # Show graph statistics

# Search & traversal
recall-echo graph search <query>                # Semantic search across entities
recall-echo graph query <query>                 # Hybrid: semantic + graph expansion + episodes
recall-echo graph traverse <entity>             # Graph traversal from entity (shows confidence)

# Data management
recall-echo graph add-entity --name <n> --type <t> --abstract <a>   # Add entity manually
recall-echo graph relate <from> --rel <type> --target <to>          # Create relationship
recall-echo graph ingest <archive>              # Ingest single archive (episodes only)
recall-echo graph ingest-all                    # Ingest all un-ingested archives
recall-echo graph extract --all                 # LLM entity extraction from archives

# Pipeline & integrations
recall-echo graph pipeline sync                 # Sync pipeline documents into the graph
recall-echo graph pipeline status               # Pipeline health from the graph
recall-echo graph pipeline flow <entity>        # Trace entity lineage through pipeline
recall-echo graph pipeline stale                # List stale pipeline entities
recall-echo graph vigil-sync                    # Sync vigil-pulse signals into the graph
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
├── graph/                    # Layer 0 — knowledge graph
│   ├── surreal/              # SurrealDB embedded data
│   └── models/               # FastEmbed cached models
└── .recall-echo.toml         # Optional configuration
```

## How It Works

recall-echo operates in two modes:

### As a pulse-null Plugin

recall-echo is a native pulse-null plugin implementing the `Plugin` trait from pulse-system-types. It fills the required **Memory** role (exactly one per entity).

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
recall-echo dashboard [entity_root]    # Full dashboard with health, stats, recent sessions
recall-echo search <query>             # Line-level archive search
recall-echo search <query> --ranked    # File-ranked relevance search
recall-echo distill [entity_root]      # Analyze MEMORY.md, suggest cleanup
recall-echo consume [entity_root]      # Output EPHEMERAL.md content
recall-echo archive-session            # Archive a Claude Code session from JSONL transcript
recall-echo archive --all-unarchived   # Batch archive all missed sessions
recall-echo checkpoint                 # Save checkpoint before context compression
recall-echo config                     # View or modify configuration
recall-echo graph <subcommand>         # Knowledge graph operations
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

### `recall-echo archive-session`

Archive a Claude Code session from a JSONL transcript. Extracts messages, generates a summary (LLM-powered when available, algorithmic fallback), updates ARCHIVE.md, and appends to EPHEMERAL.md. Designed to run as a SessionEnd hook.

### `recall-echo checkpoint`

Save a checkpoint before context compression. Creates a numbered checkpoint file so the agent can fill in summary details. Designed to run as a PreCompact hook.

### `recall-echo graph`

Knowledge graph operations. See the Architecture section above for the full command list.

**Search & traversal:**

- `graph search <query>` — Semantic search across entities. Supports `--limit`, `--type` (filter by entity type), and `--keyword` (filter by name/abstract).
- `graph query <query>` — Hybrid query combining semantic search, confidence-weighted graph expansion, and optional episode retrieval. Supports `--depth` (expansion depth, default 1, 0 = semantic only), `--episodes` (include episode results), `--limit`, `--type`, `--keyword`.
- `graph traverse <entity>` — DFS traversal from a named entity with cycle detection. Displays confidence percentages on edges (e.g. `[85%]`). Edges below 0.1 confidence are filtered. Supports `--depth` (default 2) and `--type-filter`.

**Data management:**

- `graph add-entity` — Manually add an entity. Requires `--name`, `--type`, `--abstract`. Supports `--overview` and `--source`.
- `graph relate <from> --rel <type> --target <to>` — Create a relationship between two entities. Supports `--description` and `--source`.
- `graph ingest <archive>` — Ingest a single archive file (creates episodes, no LLM required).
- `graph ingest-all` — Scan conversations/ and ingest all un-ingested archives.
- `graph extract` — LLM-powered entity extraction. Supports `--log <N>` (single archive), `--all` (all un-extracted), `--dry-run`, `--model`, `--provider` (anthropic or openai), `--delay-ms`.

**Pipeline & integrations:**

- `graph pipeline sync` — Sync pipeline documents (LEARNING.md, THOUGHTS.md, CURIOSITY.md, REFLECTIONS.md, PRAXIS.md) into the graph. Idempotent — diffs parsed entries vs existing graph entities.
- `graph pipeline status` — Pipeline health with staleness tracking.
- `graph pipeline flow <entity>` — Trace an entity's lineage through the pipeline stages.
- `graph pipeline stale` — List stale pipeline entities. Supports `--days` (threshold, default 7).
- `graph vigil-sync` — Sync vigil-pulse metacognitive signals and caliber outcomes into the graph as Measurement and Outcome entities. Supports `--signals-path` and `--outcomes-path`.

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
max_entries = 5              # Rolling window size (1-50, default 5)

[llm]
provider = "claude-code"     # LLM provider: "claude", "claude-code", or "ollama"
model = ""                   # Model name (provider default if empty)
api_base = ""                # Custom API base URL (provider default if empty)

[pipeline]
docs_dir = "/path/to/journal"  # Directory containing pipeline documents
auto_sync = true               # Auto-sync pipeline docs to graph on archive
```

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| `ephemeral` | `max_entries` | `5` | Rolling window size for session summaries (1-50) |
| `llm` | `provider` | `claude` | LLM backend for summarization (`claude`, `claude-code`, `ollama`) |
| `llm` | `model` | provider default | Model name |
| `llm` | `api_base` | provider default | Custom API base URL |
| `pipeline` | `docs_dir` | — | Path to pipeline documents (LEARNING.md, THOUGHTS.md, etc.) |
| `pipeline` | `auto_sync` | `false` | Sync pipeline documents to the knowledge graph on archive |

All settings have sensible defaults. Missing file or invalid values fall back silently.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[AGPL-3.0](LICENSE)
