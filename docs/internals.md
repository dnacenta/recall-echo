# Internals

## Layers

| Layer | What | Read |
|---|---|---|
| 0 Knowledge graph | entities, relationships, episodes in embedded SurrealDB, local embeddings | by meaning, on demand |
| 1 `MEMORY.md` | curated facts, kept short | in every Claude Code session, imported by `~/.claude/rules/recall-echo.md` |
| 2 `EPHEMERAL.md` | the last few session summaries (5 by default) | printed by the SessionStart hook on startup, resume and `/clear` |
| 3 Archives | every conversation in full, markdown with frontmatter | searched on demand |

The two layers take different paths into a Claude Code session because Claude Code inlines
a hook's stdout only up to about 10,000 characters; anything longer arrives as a short
preview and a file path. EPHEMERAL.md fits (five entries are about 2 KB), so `consume`
prints it. A curated MEMORY.md routinely does not, so `init` writes
`~/.claude/rules/recall-echo.md` instead: a memory file whose single `@` import is the
store's MEMORY.md, inlined whole on every startup, resume and `/clear`. Its first line marks
it as managed. `init` rewrites only a file carrying that line. A hand-written file at that
path is left alone: if it already imports this MEMORY.md `init` says so, otherwise it prints
the import line the file needs. A pulse root (a `pulse-null.toml` beside `memory/`) gets no
rules file, and neither does a user who runs pulses, because pulse-null puts MEMORY.md into
each pulse's prompt itself.

Because that import puts MEMORY.md into every session, `init` refuses to write it unless the
root, `memory/` and MEMORY.md are this user's alone: owned by them, not writable by group or
others, and MEMORY.md not a symlink. Claude Code also follows imports inside an imported
file, so `status`, `distill` and `init` warn about any `@` line inside MEMORY.md.

Every relationship carries a Beta posterior, not a yes or no. Corroboration raises it,
contradiction lowers it, and the evidence stays on the edge, so one offhand remark cannot
erase something the graph has good reason to believe. Evidence is weighted by who said
it: an external source 1.0, you 0.8, the agent repeating itself 0.05, so memory cannot
talk itself into things. The maths: [bayesian-confidence.md](bayesian-confidence.md).

A correction is evidence at your authority, not an override: a claim resting on twenty
observations survives one `--wrong` with less confidence, a claim resting on one collapses.
`--forget` deletes outright and leaves no trace, so prefer `--wrong`.

## Layout

All paths hang off a pulse root, the home of the agent whose memory this is:

```
{pulse_root}/memory/
├── MEMORY.md              layer 1: curated facts (keep it under ~200 lines)
├── EPHEMERAL.md           layer 2: rolling window of session summaries (default 5, FIFO)
├── ARCHIVE.md             layer 3: conversation index
├── conversations/         layer 3: conversation-001.md, conversation-002.md, …
├── graph/                 layer 0: surreal/ (data), models/ (embeddings), daemon.log
└── .recall-echo.toml      optional configuration
```

## Knowledge graph

When a conversation is archived, recall-echo extracts entities and the relationships
between them and stores them in an embedded SurrealDB database, with fastembed
embeddings for semantic search (HNSW). A search for "authentication" surfaces the session
about JWT and login flows even when nobody used that word.

**Confidence.** Each relationship starts from a prior set by how it was established:
authoritative 1.0, explicit 0.9, inferred 0.6, speculative 0.3. Evidence is persisted on
the edge as Beta pseudo-counts, `alpha` for corroboration and `beta` for contradiction,
and the stored confidence is the posterior mean `α / (α + β)`. Because the counts
accumulate, the graph tells "believed at 0.9" from "believed at 0.9 for good reason": the
variance narrows as evidence builds, and a prior's skepticism persists instead of being
recomputed away. A prior is worth about ten observations, so a single contradicting
mention does not erase established knowledge. Multi-hop queries compound confidence along
the path, which prefers shorter, surer routes. Stale relationships decay; `graph
decay-report` shows stored against effective confidence. The derivation is in
[bayesian-confidence.md](bayesian-confidence.md).

**Provenance.** Every episode is stamped at ingestion with who authored it, and an
observation counts accordingly: an independent external source 1.0, the human 0.8, the
agent restating its own belief 0.05 (`[graph.provenance]`). Conversation turn roles tell
the human's words from the agent's; `graph ingest --external` marks genuinely external
material. Self-corroboration is also tallied separately on the edge, so coherence never
passes for evidence.

**Extraction.** An LLM pipeline chunks the text (~500 tokens), extracts entities and
relationships in parallel (up to 10 at once), then deduplicates sequentially. Dedup asks
the model only about what it cannot settle itself: an existing entity of the same name
and type is the same entity, a nearest neighbour at or above `certain_similarity` is the
same entity, one below `review_similarity` is new, and only the band between buys a
skip/create/merge decision over at most `max_candidates`. The bands are cut on raw
cosine similarity, never the blended retrieval score, so a popular entity does not look
like a likelier duplicate and dedup cost stays flat as the graph grows. Re-extracted
relationships get Bayesian corroboration weighted by the provenance of their chunk.

**Entity types.** person, project, tool, service, preference, decision, event, concept,
case, pattern, thread, thought, question, observation, policy, measurement, outcome.
Mutable types (person, project, tool, …) are updated in place; immutable ones (decision,
event, case, …) are append-only.

**Tiered content.** Entities store an L0 abstract (embeddings and cheap traversal), an L1
overview (reranking) and L2 full content (pulled on demand), which keeps traversal fast.

## MCP

Without the MCP server the graph is effectively write-only: `SessionEnd` ingests,
`SessionStart` prints EPHEMERAL.md, and nothing in a session reads the graph. The server
is the read path.

| Tool | Answers |
|---|---|
| `recall_query` | the default lookup: semantic search, one hop of expansion, and the fragments behind it |
| `recall_search` | semantic entity search alone: names, types, abstracts, scores |
| `recall_episodes` | the raw conversation fragments, for what was actually said |
| `recall_traverse` | relationships out of one entity, as a tree with edge confidence |
| `recall_overview` | what memory holds without being asked anything, and where it is unsure |
| `recall_status` | counts, which tell an empty memory from a failed lookup |

`init` registers it with every agent CLI it finds. By hand, each vendor spells it
differently:

```sh
claude mcp add recall-echo -s user -- recall-echo mcp --pulse-root /path/to/pulse
gemini mcp add -s user recall-echo  recall-echo mcp --pulse-root /path/to/pulse
grok   mcp add recall-echo -s user -- recall-echo mcp --pulse-root /path/to/pulse
codex  mcp add recall-echo -- recall-echo mcp --pulse-root /path/to/pulse
```

Any other client (Cursor, Zed, …) takes the same command in its MCP config, for example a
project's `.mcp.json`:

```json
{ "mcpServers": { "recall-echo": { "command": "recall-echo", "args": ["mcp", "--pulse-root", "/path/to/pulse"] } } }
```

`--pulse-root` can usually be omitted. Before 4.6.0 the flag was `--entity-root`; it is
still accepted, and the persisted root moved to `~/.config/recall-echo/pulse-root` (the
old `entity-root` file is still read while no `pulse-root` exists).

All six tools are read-only and go through the same daemon as the CLI, so an MCP client
never takes the store away from a hook. Writing is absent on purpose: a tool letting the
model create entities would route around the provenance discount. Correction is absent
too: `graph correct` records a contradiction at user authority, which is only true while
a human is typing it.

## Daemon and concurrency

The embedded backend (SurrealKV) takes a process-exclusive lock on the store. recall-echo
resolves that with a small daemon instead of locking rules to remember. The first graph
command or hook that needs the store starts `recall-echo serve` in the background and
talks to it over a unix socket in `$XDG_RUNTIME_DIR/recall-echo/`; every later command,
from any number of sessions, goes through it. The daemon owns the store and the embedding
model, so there is no per-command model reload, and it exits after
`[serve] idle_timeout_secs` (one hour) of inactivity.

- **One daemon per memory directory.** Separate graphs never share a process.
- **Crash-only.** No state outside the database; kill it at any moment and the next
  command cleans up the dead socket and starts a new one.
- **No silent fallback.** If it cannot start, the command fails with a named error and
  the tail of `graph/daemon.log`. A store locked by a foreign process is retried with
  backoff, then reported as `store locked`, never a raw lock panic.
- **Admin commands take the store exclusively.** `graph init`, `gc`, `extract`,
  `ingest-all`, `pipeline status`/`flow`/`stale`, `vigil-sync` and `decay-report` take an
  admin lock beside the socket, stop the daemon and run in-process. A command arriving
  meanwhile waits instead of starting a daemon, and the lock is released only after the
  store is closed, so there is exactly one owner at every instant.
- **Owner-only, both ends.** The socket and its directory are `0700`/`0600` and both ends
  check the peer's uid. A configured `socket_path` must already be a directory only you
  can write into; recall-echo validates it but never creates or chmods one it did not
  derive itself.

**External SurrealDB.** With `[graph] mode = "server"` and a `url`, commands connect
directly and no daemon is involved. The backend is chosen at runtime, no rebuild needed.
The flat-file layers are unaffected by any of this.

## Background work

Once the memory directory has been quiet for `[extraction] idle_after_secs` (two
minutes), the daemon takes `batch_size` un-extracted archives (three) and runs exactly
what `graph extract` runs, then waits again; a backlog drains one batch per quiet period.
It also imports transcripts from the CLIs that have no hooks (Codex, Grok): a transcript
counts once it has been untouched for `[capture] settle_secs` (five minutes), a per-CLI
watermark avoids rescanning, and a session already in `conversations/` is never imported
twice, so a live session is never half-archived.

- **A client request wins.** Nothing is locked for a whole batch; the worker stops at the
  next archive or transcript boundary as soon as a connection is open.
- **Interruption is safe.** The `extracted` flag flips per archive after it succeeded, so
  a daemon killed mid-batch just leaves work to do.
- **Idle shutdown waits for the batch, not the backlog.** A finished batch restarts the
  idle clock, so a daemon working through a backlog stays up.
- **It gives up rather than loops.** An archive that fails twice is written to
  `graph/extraction-quarantine.txt` and skipped; three failures in a row disable the
  worker until the daemon restarts.
- **It says what it did.** Every batch logs its count and duration to `daemon.log`, and
  `graph daemon status` reports whether the worker is on, what it extracted and why it is
  off.

**Cost.** The daemon starts with an allowlisted environment that excludes API keys, so an
auto-started daemon can only use a CLI provider that authenticates through `$HOME`, which
bills nothing beyond the subscription you have. With `provider = "anthropic"` the worker
finds no key, disables itself and says so once in the log. An API provider reaches the
daemon only through `recall-echo serve --foreground` with the key exported, an explicit
act. `[extraction] background_enabled = false` and `[capture] enabled = false` turn the
two passes off. In `server` mode there is no daemon, so neither runs; use `graph extract`
and `ingest`.

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

## Gemini hooks

`gemini hooks migrate --from-claude` copies the `archive-session` hook into Gemini, where
it is handed a Gemini chat session (`~/.gemini/tmp/<hash>/chats/session-*.json`). That
format is read, but it was implemented from Gemini's type declarations, not checked
against a real file; if sessions are not archived, the message names the file, and an
[issue](https://github.com/dnacenta/recall-echo/issues) with its shape is welcome. All
output goes to stderr, because Gemini requires a hook's stdout to be JSON, and input it
cannot read exits zero, because a failing hook fails the session it is attached to.
`consume` prints markdown, which Gemini's JSON-only stdout swallows; use it there at your
own risk.

## pulse-null plugin

Built with the `pulse-null` feature, recall-echo is a native
[pulse-null](https://github.com/dnacenta/pulse-null) plugin implementing the `Plugin`
trait from pulse-system-types, and fills the required Memory role, one per pulse.
pulse-null calls `archive::archive_session()` at session end and
`checkpoint::create_checkpoint()` before compaction; health reports Healthy, Degraded or
Down; `pulse-null init` prompts for `pulse_root` (a config still carrying the pre-4.6.0
`entity_root` key is read too).
