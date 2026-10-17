# Commands

Every command that takes a root accepts `--pulse-root` (or a positional root where shown).
It defaults to the current directory when that is an initialised pulse root, then to the
root `init` persisted. `--entity-root`, the pre-4.6.0 name, is still accepted everywhere,
and re-running `init` rewrites old hooks to `--pulse-root`. `--help` on any command shows
its full flags.

## Install

The install script puts one binary in `~/.local/bin` (set `RECALL_ECHO_INSTALL_DIR` to
change it) and touches nothing else. With cargo, keep `--locked`: the store's on-disk
format is tied to the SurrealDB version the release was tested with, so a build that
resolves a different SurrealDB may not read a store another build wrote. Prebuilt
binaries cover Linux and macOS on x86_64 and arm64. After the first install,
`recall-echo update` keeps it current.

## Setup

```
recall-echo init [ROOT]              create memory/, install hooks + rules file, register MCP, fetch the model
recall-echo status [ROOT]            health: MEMORY.md use, ephemeral entries, archives
recall-echo dashboard [ROOT]         status plus stats and recent sessions
recall-echo config show              effective settings and the exact provider command
recall-echo config set KEY VALUE     change one setting
recall-echo update                   install the latest release
```

`init` defaults its root to `~/.claude` when Claude Code is installed, else the current
directory, and never overwrites existing files. With Claude Code installed it also writes
`~/.claude/rules/recall-echo.md`, which imports MEMORY.md into every session; a
hand-written file at that path is left alone. When Claude Code's built-in auto-memory is
on, `init` asks once whether to turn it off (`autoMemoryEnabled: false`), so there is a
single curated store. A scripted install only prints a notice. `update` swaps the binary only after the
new one answers `--version`: a liveness check over HTTPS, not a cryptographic one
(checksummed releases are planned). `--check` only reports and exits 10 when an update
exists, `--version vX.Y.Z` pins a release, `--force` reinstalls.

`status` is painted in Gruvbox Dark, truecolor when `COLORTERM` says so; the MEMORY.md
line turns yellow past 70 % and red past 90 %. Colour is dropped when stdout or stderr is
not a terminal, `NO_COLOR` is set or `TERM` is `dumb`; `CLICOLOR_FORCE=1` forces it on.

## Capture

```
recall-echo archive-session          SessionEnd hook: archive the session that just ended
recall-echo checkpoint               PreCompact hook: save a checkpoint before compaction
recall-echo consume                  SessionStart hook (startup, resume, /clear): print EPHEMERAL.md
recall-echo archive --all-unarchived archive every missed transcript under ~/.claude/projects/
recall-echo ingest --from CLI        import codex, grok or claude-code transcripts (repeatable)
recall-echo ingest --all             import from every CLI in [capture] sources, or every one installed
recall-echo distill                  suggest moving MEMORY.md sections over 30 lines into topic files
```

`archive-session` reads Claude Code's JSONL and Gemini CLI's JSON chat sessions, sniffing
the file rather than trusting its extension. It summarises with the LLM when a provider
is available and falls back to an algorithmic summary otherwise.

## Reading

```
recall-echo what-do-you-know         strongest entities per type, how firmly they are held
    --about X                        one subject, with each claim and its confidence
    --limit N                        entities per type (default 3)
recall-echo search QUERY             line matches in the archives
    -C N                             lines of context
    --ranked [--max-results N]       rank files by match count, word coverage and recency
recall-echo graph search QUERY       semantic entity search; --limit, --type, --keyword
recall-echo graph query QUERY        semantic search + confidence-weighted expansion
    --depth N                        expansion depth (default 1, 0 = semantic only)
    --episodes                       include conversation fragments
recall-echo graph traverse ENTITY    DFS with cycle detection, edge confidence shown as [85%]
    --depth N --type-filter T        default depth 2; edges under 0.1 are hidden
recall-echo graph status             entity, relationship and episode counts
recall-echo graph decay-report       stored vs effective confidence; --entity, --all
```

Without `--about`, `what-do-you-know` also lists the relationships it is least sure of and
the ones it believes largely because it kept repeating them (`self×N`, the coherence
tally, deliberately kept out of confidence).

## Correcting

```
recall-echo graph correct FROM REL TO --wrong    contradict one claim (direction need not match)
recall-echo graph correct ENTITY --wrong         contradict its claim; asks which if several
    --all-edges                                  contradict every one of them
recall-echo graph correct NAME --forget          remove an entity and its relationships
recall-echo graph correct FROM REL TO --forget   remove one relationship
    --yes                                        skip the confirmation
```

`--wrong` records one contradicting observation at `weight_user` (0.8 by default) and
lets the posterior move: say it twice and it counts twice. `--forget` prints what would go
and asks first; a non-interactive stdin is never taken as consent. Corrections exist only
on the CLI, because a model calling them would record its own judgement as yours.

## Writing and maintenance

```
recall-echo graph add-entity --name N --type T --abstract A   [--overview, --source]
recall-echo graph relate FROM --rel REL --target TO           [--description, --source]
recall-echo graph ingest ARCHIVE     one archive as episodes, no LLM; --external for documents
recall-echo graph ingest-all         every un-ingested archive; --external likewise
recall-echo graph extract            LLM entity extraction
    --all | --log N                  every un-extracted archive, or one
    --dry-run                        show what would be extracted
    --provider P --model M           override the configured ones
    --delay-ms N                     pause between archives (default 100)
    --max-tokens N                   spend cap, measured and estimated (default 5000000, 0 = none)
recall-echo graph gc                 prune stale/dead relationships and orphaned entities
    --execute                        actually delete (default is a dry run)
    --episodes                       also sweep old, never-retrieved, self-authored episodes
    --stats-only                     health stats only
recall-echo graph feedback SESSION --outcome success|partial|failed   utility feedback
recall-echo graph migrate [--force]  re-run schema migrations
recall-echo graph init               initialise the graph store
```

`gc` thresholds: `--stale-days 30`, `--stale-confidence 0.5`, `--dead-confidence 0.2`,
`--dead-min-age-days 14`, `--episode-max-age-days 180`. `migrate` normally reports
nothing to do, since opening the store already migrates it; it repairs a version marker
that claims a migration which did not finish. `--force` clears the marker and runs every
migration again, which is safe because each only touches rows still missing its value.

## Daemon and MCP

```
recall-echo serve [--dir D] [--foreground]   run the graph daemon (normally auto-started)
recall-echo graph daemon status              socket, pid, version, uptime, worker state
recall-echo graph daemon stop                stop it; the next command starts a fresh one
recall-echo mcp                              MCP server on stdio
```

`--foreground` logs to stderr as well as `graph/daemon.log` and disables idle shutdown,
leaving the lifetime to systemd. Background extraction still runs: it waits for quiet,
not for an idle timeout.

## pulse-null stack

```
recall-echo graph pipeline sync      sync LEARNING/THOUGHTS/CURIOSITY/REFLECTIONS/PRAXIS; --docs-dir
recall-echo graph pipeline status    counts by stage, stale entities; --days (default 7)
recall-echo graph pipeline flow E    trace an entity through the pipeline stages
recall-echo graph pipeline stale     stale pipeline entities; --days (default 7)
recall-echo graph vigil-sync         vigil-pulse signals and caliber outcomes as Measurement/Outcome
```
