# recall-echo

[![License: MPL-2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](LICENSE)
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
- Evidence is **persisted on the edge** as Beta pseudo-counts (`alpha` for corroboration, `beta` for contradiction). Corroboration adds to α, contradiction adds to β, and the stored confidence is the posterior mean `α / (α + β)`
- Because the counts accumulate, the graph distinguishes "believed at 0.9" from "believed at 0.9 for good reason" — the posterior variance narrows as evidence builds, and a prior's initial skepticism persists rather than being recomputed away
- Updates are gradual — a prior is worth ~10 observations, so a single contradictory mention doesn't erase established knowledge
- **Observations are weighted by provenance.** Every episode is stamped at ingestion with who authored it, and an observation contributes evidence accordingly: an independent external source counts fully (1.0), the human nearly fully (0.8), the agent restating its own belief almost not at all (0.05, configurable via `[graph.provenance]`). Self-corroboration is also tallied separately on the edge, so coherence never passes for evidence
- Multi-hop queries compound confidence along the path, naturally preferring shorter, higher-confidence routes

This means the graph handles contradictions, reinforces patterns over time, and lets uncertain or stale knowledge fade gracefully — instead of requiring manual cleanup or producing false-positive retrievals.

**Entity types:** person, project, tool, service, preference, decision, event, concept, case, pattern, thread, thought, question, observation, policy, measurement, outcome. Mutable types (person, project, tool, etc.) can be updated; immutable types (decision, event, case, etc.) are append-only.

**Extraction pipeline:** When conversations are archived, an LLM-powered pipeline chunks the text (~500 tokens), extracts entities and relationships in parallel (up to 10 concurrent), then deduplicates sequentially. Dedup escalates to the LLM only for candidates it cannot settle itself: an existing entity of the same name and type is the same entity, a nearest neighbour above `certain_similarity` is the same entity, one below `review_similarity` is a new one, and only the band in between buys an LLM skip/create/merge decision — over a set capped at `max_candidates`. The bands are cut on raw cosine similarity, never on the blended retrieval score, so a popular entity does not read as a likelier duplicate and dedup cost stays flat as the graph grows. Re-extracted relationships receive Bayesian corroboration updates weighted by the provenance of the chunk they came from — conversation turn roles are read to tell the human's words from the agent's, and `graph ingest --external` marks genuinely external material — so knowledge confirmed by independent sources gains confidence while the agent repeating itself barely moves the score.

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
recall-echo graph extract --all                 # LLM entity extraction (the daemon also does this when idle)

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
cargo install recall-echo --locked
recall-echo init
```

`--locked` installs the exact dependency versions the release was tested
against. The embedded graph store's on-disk record format is tied to the
SurrealDB version, so a build that resolves a different SurrealDB may be
unable to read a store another build wrote.

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
- `graph extract` — LLM-powered entity extraction. Supports `--log <N>` (single archive), `--all` (all un-extracted), `--dry-run`, `--model`, `--provider` (anthropic or openai), `--delay-ms`. The daemon runs this pass on its own once the machine is quiet (see [Background extraction](#background-extraction)); this command is how you run it *now*, or in `server` mode, or after changing the model.

**Daemon:**

- `graph daemon status` — Socket path, pid, version and uptime of the daemon serving this graph.
- `graph daemon stop` — Stop that daemon. The next graph command starts a fresh one.

**Pipeline & integrations:**

- `graph pipeline sync` — Sync pipeline documents (LEARNING.md, THOUGHTS.md, CURIOSITY.md, REFLECTIONS.md, PRAXIS.md) into the graph. Idempotent — diffs parsed entries vs existing graph entities.
- `graph pipeline status` — Pipeline health with staleness tracking.
- `graph pipeline flow <entity>` — Trace an entity's lineage through the pipeline stages.
- `graph pipeline stale` — List stale pipeline entities. Supports `--days` (threshold, default 7).
- `graph vigil-sync` — Sync vigil-pulse metacognitive signals and caliber outcomes into the graph as Measurement and Outcome entities. Supports `--signals-path` and `--outcomes-path`.

### `recall-echo serve`

Runs the graph daemon for a memory directory. You never need to run this by
hand — graph commands and hooks start it automatically. It exists for
supervised deployments:

```bash
recall-echo serve --dir /path/to/memory --foreground
```

`--foreground` logs to stderr as well as `<memory_dir>/graph/daemon.log` and
disables idle shutdown, leaving lifetime to systemd. Background extraction
still runs — it waits for quiet, not for an idle timeout.

### `recall-echo mcp`

An MCP server over stdio, so an agent can query its own memory mid-conversation.

**Why it exists.** Without it the knowledge graph is effectively write-only.
`SessionEnd` ingests episodes and `SessionStart` runs `consume`, which only
prints EPHEMERAL.md — nothing in a normal session ever reads the graph, so the
Bayesian confidence, semantic search, provenance weighting and temporal decay
sit behind a command a human has to type by hand. The MCP server is the read
path: the agent asks memory the actual question, at the moment it matters.

Add it to Claude Code:

```bash
claude mcp add recall-echo -- recall-echo mcp --entity-root /path/to/entity
```

Or, equivalently, in a project's `.mcp.json`:

```json
{
  "mcpServers": {
    "recall-echo": {
      "command": "recall-echo",
      "args": ["mcp", "--entity-root", "/path/to/entity"]
    }
  }
}
```

`--entity-root` defaults to the current directory, so it can be omitted when
the client is launched from the entity root.

**Tools.** All five are read-only; none can write to the graph.

| Tool | Answers |
| --- | --- |
| `recall_query` | The default lookup: semantic search + one hop of graph expansion + the conversation fragments behind it |
| `recall_search` | Semantic entity search alone — names, types, abstracts, retrieval scores |
| `recall_episodes` | The raw conversation fragments, for what was actually said |
| `recall_traverse` | Relationships out of one named entity, as a tree with edge confidence |
| `recall_status` | Entity, relationship and episode counts — tells an empty memory from a failed lookup |

Every tool runs through the same graph daemon as the CLI, so it inherits the
daemon's auto-start, locking and concurrency, and starting an MCP client never
takes the store away from a hook.

**Writing is deliberately absent.** The graph discounts what the agent asserts
about itself (`[graph.provenance]`), and a tool that let the model create
entities and edges directly would route around exactly that mechanism. Memory
is written on the ingest path, where every episode is stamped with its
authorship.

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

[graph]
mode = "embedded"            # Storage backend: "embedded" (default) or "server"
url = "ws://localhost:8787"  # SurrealDB server URL (server mode only)

[graph.dedup]
certain_similarity = 0.92    # At or above this cosine similarity, the same entity — no model call
review_similarity = 0.82     # Below this, a new entity — no model call
max_candidates = 3           # Existing entities compared per candidate

[serve]
socket_path = ""             # Daemon socket override (default: XDG runtime dir)
idle_timeout_secs = 3600     # Shut the daemon down after this much inactivity (0 = never)

[extraction]
background_enabled = true    # Let the daemon extract entities when the machine is quiet
idle_after_secs = 120        # Quiet period before a background batch starts
batch_size = 3               # Archives per batch (the next batch is one quiet period later)
```

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| `ephemeral` | `max_entries` | `5` | Rolling window size for session summaries (1-50) |
| `llm` | `provider` | `claude` | LLM backend for summarization (`claude`, `claude-code`, `ollama`) |
| `llm` | `model` | provider default | Model name |
| `llm` | `api_base` | provider default | Custom API base URL |
| `pipeline` | `docs_dir` | — | Path to pipeline documents (LEARNING.md, THOUGHTS.md, etc.) |
| `pipeline` | `auto_sync` | `false` | Sync pipeline documents to the knowledge graph on archive |
| `graph` | `mode` | `embedded` | Storage backend: `embedded` (single-process SurrealKV) or `server` (shared SurrealDB) |
| `graph` | `url` | `ws://localhost:8787` | SurrealDB server URL, `server` mode only |
| `graph.dedup` | `certain_similarity` | `0.92` | Cosine similarity at or above which a candidate is the same entity, resolved without a model call |
| `graph.dedup` | `review_similarity` | `0.82` | Cosine similarity below which a candidate is a new entity, created without a model call |
| `graph.dedup` | `max_candidates` | `3` | How many existing entities dedup compares a candidate against |
| `serve` | `socket_path` | XDG runtime dir | Daemon socket path override |
| `serve` | `idle_timeout_secs` | `3600` | Daemon idle shutdown timeout (`0` disables) |
| `extraction` | `background_enabled` | `true` | Extract entities in the daemon once it has been quiet |
| `extraction` | `idle_after_secs` | `120` | Seconds without a request before a background batch may start (`0` = as soon as no connection is open) |
| `extraction` | `batch_size` | `3` | Archives per background batch |

All settings have sensible defaults. Missing file or invalid values fall back silently.

## Concurrency

The default `embedded` backend (SurrealKV) takes a **process-exclusive file
lock** on the graph store: one process may hold it at a time. recall-echo
resolves that with a small daemon rather than with locking rules you have to
remember.

**How it works.** The first graph command or hook that needs the store starts
a daemon in the background (`recall-echo serve`) and talks to it over a unix
socket in `$XDG_RUNTIME_DIR/recall-echo/`. Every later command — from any
number of concurrent sessions — goes through that same daemon, so concurrent
searches, queries and ingests all succeed. The daemon owns the store and the
embedding model, which also removes the per-command ONNX model reload. After
`[serve] idle_timeout_secs` of inactivity (default one hour) it exits.

- One daemon **per memory directory**: separate graphs never share a process.
- **Crash-only**: the daemon keeps no state outside the database. Kill it at
  any moment; the next command cleans up the dead socket and starts a new one.
- **No silent fallback**: if the daemon cannot start, the command fails with a
  named error explaining what went wrong, including the tail of
  `<memory_dir>/graph/daemon.log`.
- **Admin commands take the store exclusively.** `graph init`, `gc`,
  `extract`, `ingest-all`, `pipeline status`/`flow`/`stale`, `vigil-sync` and
  `decay-report` take an admin lock beside the socket, stop the daemon and run
  in-process. A command that arrives while that lock is held waits for it
  instead of starting a daemon, and the lock is released only after the store
  is closed — so there is exactly one owner of the store at every instant.
- **Owner-only, both ends.** The socket and its directory are `0700`/`0600`
  and both ends check the peer's uid, so no other local user can read what you
  ingest or answer your queries. A `[serve] socket_path` you configure must
  already exist as a directory only you can write into: recall-echo validates
  it, but never creates or chmods a directory it did not derive itself.
- Inspect it with `graph daemon status`; stop it with `graph daemon stop`.

**External SurrealDB (advanced).** For deployments that already run a
SurrealDB server (benchmark rigs, shared entity hosts), set
`[graph] mode = "server"` and point `url` at it. Commands then connect
directly and no daemon is involved. The backend is chosen at runtime — no
rebuild needed.

If the embedded store is locked by a foreign process, the daemon retries with
backoff and then fails with a named `store locked` error — never a raw LOCK
panic. Flat-file layers (MEMORY.md, EPHEMERAL.md, archives) are unaffected by
any of this.

## Background extraction

Episodes arrive mechanically: the `SessionEnd` hook ingests every conversation
without anyone remembering to. Turning those episodes into **entities,
relationships, confidence and provenance** used to be a command a human had to
type — so semantic search and Bayesian confidence, the features this project is
actually about, stayed empty for anyone who did not read the docs closely.

The daemon does that pass itself. It already owns the store and already knows
when nobody is using it, so once the memory directory has been quiet for
`[extraction] idle_after_secs` (default two minutes) it takes `batch_size`
un-extracted archives (default three) and runs exactly what
`recall-echo graph extract` runs. Then it goes back to waiting. A backlog
drains one batch per quiet period.

- **A client request always wins.** Nothing is locked for the length of a
  batch: the worker shares the store like any other task, and it stops at the
  next archive boundary as soon as a connection is open.
- **Interruption is safe.** The `extracted` flag flips per archive, after that
  archive succeeded — never per batch. A daemon killed mid-batch simply leaves
  work to do.
- **Idle shutdown waits for the batch, not for the backlog.** The daemon will
  not exit with an archive in flight, and a finished batch restarts the idle
  clock — so a daemon working through a backlog stays up, and one with nothing
  left to do exits after `[serve] idle_timeout_secs` as always.
- **It gives up rather than loops.** An archive that fails twice is written to
  `graph/extraction-quarantine.txt` and skipped; three failures in a row
  disable the worker until the daemon restarts. There is no retry storm and no
  runaway bill.
- **It says what it did.** Every batch logs its count and duration to
  `<memory_dir>/graph/daemon.log`, and `graph daemon status` reports whether
  the worker is on, what it has extracted, and — when it is off — why.

**What it costs.** Extraction calls a model, so this is real money for
API-key providers. Two things bound it. First, the daemon is started with an
allowlisted environment that deliberately excludes API keys, so an
*auto-started* daemon can only ever use the `claude-code` provider, which bills
nothing beyond a Claude subscription; with `provider = "anthropic"` the worker
finds no key, disables itself, and says so once in the daemon log. An API
provider reaches the daemon only if you run `recall-echo serve --foreground`
with the key exported — an explicit act. Second, `batch_size` bounds any single
burst. Set `background_enabled = false` to turn the pass off entirely.

**Not in `server` mode.** With `[graph] mode = "server"` clients bypass the
daemon completely, so a daemon's "quiet" measures a socket nobody connects to
rather than anything about you, and other processes may be writing to the same
store. Background extraction stays off there; `graph extract` is the way.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[MPL-2.0](LICENSE) — file-level copyleft. You may use recall-echo inside a
closed-source product without opening your own code; modifications to
recall-echo's own files must be published under the MPL.

Versions up to and including v3.13.0 were released under AGPL-3.0 and remain
available under those terms.

**Dependency note:** the graph store uses SurrealDB 3.x, under the Business
Source License 1.1 — source-available rather than OSI open source, converting
to Apache-2.0 on 2030-01-01. Its use grant covers embedding the engine (what
recall-echo does); it restricts offering SurrealDB itself as a database
service to third parties.
