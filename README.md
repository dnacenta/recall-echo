# recall-echo

[![License: MPL-2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](LICENSE)
[![Version](https://img.shields.io/github/v/tag/dnacenta/recall-echo?label=version&color=green)](https://github.com/dnacenta/recall-echo/tags)

**Your coding agent forgets everything when the session closes.** recall-echo gives it real memory: every session captured, turned into a knowledge graph with confidence on every claim, queryable by the agent mid-conversation. Nothing to maintain.

```bash
curl -fsSL https://raw.githubusercontent.com/dnacenta/recall-echo/main/install.sh | sh
recall-echo init
```

`init`: finds your agent CLIs (Claude Code, Codex, Grok, Gemini), picks one for extraction (asks only if real choice), installs Claude Code hooks, registers MCP server with each CLI found, pre-downloads embedding model. Then: sessions captured at end, extracted when machine idle, available next session.

## Why it's different

- **No curation.** Archive, checkpoint, extraction all automatic. Memory that depends on the agent remembering to save = circular.
- **Search by meaning.** "authentication" finds the JWT/login session, no keyword match needed.
- **Confidence, not facts.** Every relationship = Beta posterior. Corroboration raises it, contradiction lowers it, stale claims decay. One offhand contradiction doesn't erase a well-supported claim.
- **Knows who said what.** Your words outweigh agent inferences; agent repeating itself ≈ no evidence. No echo chamber.
- **Local-first.** Embedded DB, local embeddings → retrieval free + offline. Only extraction calls a model: your existing CLI subscription, an API key, or fully local Ollama.
- **Agent asks for itself.** Read-only MCP server. `init` registers it with Claude Code, Codex, Grok, Gemini; other MCP clients (Cursor, Zed…) by hand, see [MCP](#mcp).

## Does it work

LongMemEval-S, stratified 10-question subset: **0/9 → 7/9** correct, right evidence retrieved for all 9. Small sample, stated as such. Methodology, regressions, limits: [`docs/benchmarks/`](docs/benchmarks/).

## Using it

Day to day: you don't. To look or fix:

```bash
recall-echo what-do-you-know               # what memory holds + how sure
recall-echo what-do-you-know --about D     # one subject
recall-echo status                         # health
recall-echo search "deployment"            # grep archives
recall-echo graph query "auth flow"        # semantic + graph expansion
recall-echo graph traverse recall-echo     # neighbours, edge confidence
recall-echo graph correct D USES Vim --wrong   # claim is mistaken
recall-echo graph correct Vim --forget         # memory shouldn't exist
```

Inside the agent:

> **You:** why did we drop the websocket approach?
> **Agent:** *(calls `recall_query`)* You moved off it in June — reconnect logic kept
> dropping messages under load; you settled on polling every 30s.

**Corrections are evidence, not overrides.** `--wrong` = one contradicting observation at your authority (`weight_user`, 0.8). Claim backed by 20 observations survives with less confidence; claim backed by 1 collapses. Say it twice, counts twice. `--forget` deletes outright, no trace (asks first, `--yes` skips) — prefer `--wrong`. CLI only: a model calling a correction tool would record its judgement as yours.

## Architecture

```
┌─ Layer 0  KNOWLEDGE GRAPH ── SurrealDB + fastembed ──────────────┐
│  entities · relationships · episodes                             │
│  Bayesian confidence · HNSW semantic search · LLM extract+dedup  │
├─ Layer 1  MEMORY.md ──── curated facts, always in context ───────┤
├─ Layer 2  EPHEMERAL.md ─ last N session summaries (FIFO, def. 5) ┤
├─ Layer 3  ARCHIVE.md ──→ conversations/conversation-NNN.md ──────┤
│           full sessions, YAML frontmatter + markdown, on demand  │
└──────────────────────────────────────────────────────────────────┘
```

```
{pulse_root}/memory/
├── MEMORY.md  EPHEMERAL.md  ARCHIVE.md
├── conversations/        # full archives
├── graph/                # surreal/ data, models/ embeddings, daemon.log
└── .recall-echo.toml     # optional config
```

Pulse root = home of the agent whose memory this is. `init` default: `~/.claude` if Claude Code installed, else cwd. Later commands: initialised cwd, else root `init` persisted.

**Confidence.** Prior by how a claim was established: authoritative 1.0, explicit 0.9, inferred 0.6, speculative 0.3, worth ~10 observations. Evidence persists on the edge as Beta pseudo-counts (α corroborate, β contradict); confidence = α/(α+β). Counts accumulate → "0.9 with evidence" ≠ "0.9 by prior". Multi-hop compounds along the path → shorter, surer routes win. Deep dive: [`docs/bayesian-confidence.md`](docs/bayesian-confidence.md).

**Provenance.** Each episode stamped with its author at ingest. Evidence weight (`[graph.provenance]`): external 1.0, user 0.8, agent itself 0.05. Self-corroboration tallied separately (`self×N`), never counted as evidence. `graph ingest --external` marks genuinely external material.

**Extraction.** Chunk ~500 tokens → extract in parallel (≤10) → dedup sequentially. Dedup on raw cosine: same name+type or ≥ `certain_similarity` = same entity; < `review_similarity` = new; in between = LLM decides against ≤ `max_candidates`. Cost stays flat as graph grows.

**Entity types.** person, project, tool, service, preference, decision, event, concept, case, pattern, thread, thought, question, observation, policy, measurement, outcome. Mutable ones (person, project, tool…) update; immutable ones (decision, event, case…) append-only. Content tiered: L0 abstract (embeddings, traversal), L1 overview (rerank), L2 full (on demand).

## Commands

```bash
# setup
recall-echo init [pulse_root]
recall-echo status | dashboard             # health; dashboard adds stats + recent sessions
recall-echo config show | set <key> <val>  # show prints exact provider argv
recall-echo update [--check | --version vX.Y.Z | --force]

# capture (hooks call these)
recall-echo archive-session                # SessionEnd: archive Claude Code / Gemini transcript
recall-echo checkpoint                     # PreCompact
recall-echo consume                        # SessionStart: print EPHEMERAL.md
recall-echo archive --all-unarchived       # backfill missed Claude Code sessions
recall-echo ingest --from codex|grok|claude-code   # import a CLI's own transcripts (--all = every one)

# read
recall-echo search <q> [-C N] [--ranked [--max-results N]]
recall-echo what-do-you-know [--about X] [--limit N]
recall-echo graph search <q>               # entities only; --type, --keyword, --limit
recall-echo graph query <q>                # + graph expansion; --depth (0 = semantic only), --episodes
recall-echo graph traverse <entity>        # DFS, edges <0.1 hidden; --depth, --type-filter
recall-echo graph status | decay-report

# write / maintain
recall-echo graph correct <entity | from REL to> --wrong | --forget
recall-echo graph add-entity --name N --type T --abstract A
recall-echo graph relate <from> --rel REL --target <to>
recall-echo graph ingest <archive> | ingest-all      # episodes only, no LLM
recall-echo graph extract --all | --log N [--dry-run] [--provider P] [--model M]
recall-echo graph gc [--execute] [--episodes]        # prune stale/dead edges, orphans; dry-run default
recall-echo graph feedback <session> --outcome success|partial|failed
recall-echo graph migrate [--force]        # normally no-op; fixes a half-finished migration marker
recall-echo distill                        # MEMORY.md sections >30 lines → suggest topic files

# daemon
recall-echo serve [--dir D] [--foreground] # auto-started; run by hand only under systemd
recall-echo graph daemon status | stop
recall-echo mcp                            # MCP server, stdio
```

Flags: `--help` on any command. `--entity-root` (pre-4.6.0 name) still accepted everywhere; re-running `init` rewrites hooks to `--pulse-root`.

## MCP

Without it, the graph is write-only: hooks ingest, nothing in a session reads it. MCP = read path.

Six tools, all read-only:

- `recall_query` — default lookup: semantic + one hop + conversation fragments behind it
- `recall_search` — entity search alone
- `recall_episodes` — raw fragments, what was actually said
- `recall_traverse` — relationships out of one entity, with confidence
- `recall_overview` — what memory holds + where it's unsure
- `recall_status` — counts; tells empty memory from failed lookup

No write tools on purpose: the graph discounts what the agent says about itself, and a write tool would route around that.

`init` registers it automatically. By hand (`--pulse-root` usually optional):

```bash
claude mcp add recall-echo -s user -- recall-echo mcp --pulse-root /path/to/pulse
gemini mcp add -s user recall-echo  recall-echo mcp --pulse-root /path/to/pulse
grok   mcp add recall-echo -s user -- recall-echo mcp --pulse-root /path/to/pulse
codex  mcp add recall-echo -- recall-echo mcp --pulse-root /path/to/pulse
```

Any other client, `.mcp.json`:

```json
{ "mcpServers": { "recall-echo": { "command": "recall-echo", "args": ["mcp", "--pulse-root", "/path/to/pulse"] } } }
```

Tools go through the same daemon as the CLI → an MCP client never locks hooks out.

## LLM providers

Needed for extraction, dedup, summaries. **Embeddings always local** (fastembed/ONNX) → search + traversal never cost anything.

| `provider` | runs | cost |
|---|---|---|
| `claude-code` | `claude -p` | subscription |
| `codex` | `codex exec` | subscription |
| `grok` | `grok` | subscription |
| `gemini` | `gemini` | subscription / free tier |
| `cli` | whatever `[llm.cli]` says | that CLI's |
| `ollama` | `localhost:11434/v1` | free, local |
| `openai` | any OpenAI-compatible endpoint | per token |
| `anthropic` | Anthropic API | per token |

No config → `anthropic`; `init` writes the CLI it found instead.

```bash
recall-echo config set llm.provider grok
recall-echo config show                    # exact argv it will run
```

CLI providers = one implementation. Name picks a preset of `[llm.cli]` defaults; every key overridable → new CLI is config, not a release:

```text
claude-code  claude -p --model <M> --output-format text --system-prompt <system> \
                    --no-session-persistence  < <prompt>
gemini       gemini -m <M> -o json -p "<system>\n\n<prompt>"
grok         grok -m <M> --output-format json -p "<system>\n\n<prompt>"
codex        codex exec -m <M> --json --skip-git-repo-check  < "<system>\n\n<prompt>"
```

`-m` dropped when no model set. Binary overrides: `CLAUDE_BIN`, `GEMINI_BIN`, `GROK_BIN`, `CODEX_BIN`, `RECALL_CLI_BIN`, or `[llm.cli] command`. Output shapes: claude `raw`, gemini/grok `single-json` + `result_json_path`, codex `ndjson` (last `type=item.completed`, `item.type=agent_message`). Unparseable JSON or missing path → stdout verbatim; non-zero exit or self-reported failure → error with CLI's stderr.

Caveats:

- **codex:** `--skip-git-repo-check` required (memory dir usually not a trusted repo); `-p` is `--profile`, so prompt goes on stdin.
- **gemini result field unverified:** flags checked against 0.27.3, no authenticated call. Tries `response`, `result`, then raw stdout; set `result_json_path` if yours differs.
- **Tokens:** measured where reported (codex, grok, APIs → "14K measured"); claude-code prose has none → `~ estimated`. `config show` says which before you spend. Point `usage_input_path` / `usage_output_path` at any CLI's counters.
- **Daemon = file-based auth only:** started with allowlisted env, no API keys → see [Background work](#background-work).

## Configuration

`.recall-echo.toml` in the memory dir. All optional; missing file or invalid values → defaults, silently.

```toml
[ephemeral]
max_entries = 5               # rolling window, 1–50

[llm]
provider = "anthropic"        # see LLM providers
model = ""                    # empty = provider default
api_base = ""                 # HTTP providers only

[llm.cli]                     # CLI providers; each key defaults to the preset
# command = "mycli"           # binary name/path
# args = ["chat"]             # fixed args before generated flags
# prompt_delivery = "flag"    # stdin | flag | arg
# prompt_flag = "--ask"       # when delivery = flag
# model_flag = "--model"      # empty = omitted
# output_format_flag = "--format"   # empty value → flag passed alone
# output_format_value = "json"
# system_prompt_flag = ""     # empty = prepended to message
# output_mode = "single-json" # raw | single-json | ndjson
# result_json_path = "data.text"    # dotted path; empty = stdout
# ndjson_match = []           # path=value selectors, ndjson only
# usage_input_path = ""       # token counters; empty = estimated
# usage_output_path = ""
# extra_args = ["--quiet"]
timeout_secs = 300            # 0 = forever

[graph]
mode = "embedded"             # embedded (SurrealKV) | server (shared SurrealDB)
url = "ws://localhost:8787"   # server mode only

[graph.dedup]
certain_similarity = 0.92     # ≥ → same entity, no model call
review_similarity = 0.82      # < → new entity, no model call
max_candidates = 3

[graph.provenance]
weight_external = 1.0
weight_user = 0.8
weight_self = 0.05

[serve]
socket_path = ""              # default: XDG runtime dir
idle_timeout_secs = 3600      # 0 = never

[extraction]
background_enabled = true
idle_after_secs = 120         # quiet before a batch
batch_size = 3                # archives per batch

[capture]
enabled = true                # daemon imports CLI transcripts
# sources = ["claude-code", "codex", "grok"]   # default: every CLI with sessions here
settle_secs = 300             # untouched this long = session finished

[pipeline]                    # pulse-null stack only
docs_dir = ""
auto_sync = false
```

## Daemon & concurrency

Embedded store (SurrealKV) = one process at a time. So: one daemon per memory dir, unix socket in `$XDG_RUNTIME_DIR/recall-echo/`, all commands and sessions go through it.

- **Auto-started** by first graph command or hook. Owns store + embedding model (no per-command model reload). Exits after `idle_timeout_secs` (1h).
- **Crash-only.** No state outside the DB; kill anytime, next command cleans up.
- **No silent fallback.** Can't start → named error + tail of `graph/daemon.log`. Foreign lock → backoff, then named `store locked` error, never a raw panic.
- **Admin commands exclusive.** `graph init`, `gc`, `extract`, `ingest-all`, `pipeline status|flow|stale`, `vigil-sync`, `decay-report` take an admin lock, stop the daemon, run in-process; others wait. Exactly one store owner at all times.
- **Owner-only.** Socket dir/file `0700`/`0600`, peer uid checked both ends. Custom `socket_path` must already be a dir only you can write; never created or chmodded for you.
- **`mode = "server"`:** commands hit external SurrealDB directly, no daemon, no background work. Runtime switch, no rebuild.

Flat-file layers (MEMORY.md, EPHEMERAL.md, archives) unaffected by all of this.

## Background work

Once the memory dir is quiet, the daemon does what used to need typing:

- **Extraction.** After `idle_after_secs` (2 min), takes `batch_size` (3) un-extracted archives, runs exactly `graph extract`. Backlog drains one batch per quiet period.
- **Capture.** Imports transcripts from CLIs without hooks (Codex, Grok) once untouched for `settle_secs` (5 min). Per-CLI watermark + session-id check → nothing imported twice; live sessions never half-archived.
- **Client wins.** Batch stops at next archive/transcript boundary when a connection opens.
- **Kill-safe.** `extracted` flag flips per archive, after success. Idle shutdown waits for the batch in flight, not the backlog.
- **Gives up, doesn't loop.** Archive fails twice → `graph/extraction-quarantine.txt`, skipped. 3 failures in a row → worker off until restart. No retry storm, no runaway bill.
- **Reports.** Each batch logged to `daemon.log`; `graph daemon status` shows worker state, progress, and why it's off.
- **Cost bounded.** Daemon env excludes API keys → auto-started daemon only uses CLIs with file-based auth (`$HOME`), i.e. subscription you already pay. `provider = "anthropic"` → worker finds no key, disables itself, logs once. API provider in the daemon only via `serve --foreground` with key exported — explicit act. Off switches: `[extraction] background_enabled = false`, `[capture] enabled = false`.

## Archive format

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
Key outcomes. **Decisions**: Chose JWT. **Action Items**: token refresh endpoint.

### User
...
### Assistant
...

## Tags
**Files**: src/auth.rs  **Tools**: Read, Edit, Bash
```

Summary: LLM when a provider is available, algorithmic fallback otherwise.

**Gemini hooks** (`gemini hooks migrate --from-claude`): `archive-session` reads Gemini chat JSON too — format built from Gemini's type declarations, not a real file; if sessions aren't archived, the error names the file, please [open an issue](https://github.com/dnacenta/recall-echo/issues). All its output goes to stderr (Gemini wants JSON-only stdout); unreadable input exits 0 so a hook never fails the session. `consume` prints markdown → Gemini swallows it; your risk.

## pulse-null

Native [pulse-null](https://github.com/dnacenta/pulse-null) plugin (`Plugin` trait from pulse-system-types), fills the required Memory role, one per pulse. pulse-null calls `archive::archive_session()` at session end and `checkpoint::create_checkpoint()` before compaction; health reports Healthy / Degraded / Down; `pulse-null init` prompts for `pulse_root` (old `entity_root` key still read). `graph pipeline …` (journal docs → graph) and `graph vigil-sync` exist for that stack.

## Install

- **Script:** see top. Bootstrap only; afterwards `recall-echo update`.
- **Cargo:** `cargo install recall-echo --locked`. `--locked` matters: embedded store's on-disk format tied to the SurrealDB version tested.
- **Prebuilt:** [Releases](https://github.com/dnacenta/recall-echo/releases/latest) — Linux x86_64/aarch64 (gnu), macOS x86_64/Apple Silicon.
- **Source:** `git clone https://github.com/dnacenta/recall-echo && cd recall-echo && cargo build --release`.
- **Update:** `recall-echo update` swaps binary only after the new one answers `--version`. Liveness check over HTTPS, not cryptographic verification (checksums planned). `--check` exits 10 if update available.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md): branch naming, commit conventions, workflow.

## License

[MPL-2.0](LICENSE): file-level copyleft. Use inside closed-source products freely; changes to recall-echo's own files must be published under MPL. ≤ v3.13.0 released under AGPL-3.0, still available under those terms.

**Dependency note:** SurrealDB 3.x is BSL 1.1 (source-available, → Apache-2.0 on 2030-01-01). Use grant covers embedding the engine (what recall-echo does); restricts offering SurrealDB itself as a service.
