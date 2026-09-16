# Spec — Background extraction: fail loudly, never mark a failed archive done, find the CLI without PATH luck

**Status:** implemented (RE-61, PR pending)
**Target version:** 4.3.0 → 4.3.1 (bug fix, no surface change)
**Drafted:** 2026-09-16
**Baseline:** `main` @ `d64f668` (v4.3.0)
**Issue:** #61 (also the reopened #42's "mark regardless of yield" item)

---

## Goal

A background extraction that produced nothing because the provider failed must be visible in
the daemon log, must not mark the archive extracted, and must trip the worker's existing
retry → quarantine → wedged ladder. Separately, the agent CLI binary must be found even when
the daemon was started with a minimal `PATH`, and when it cannot be found the daemon must say
so at startup instead of failing once per archive.

## Why this matters

On D's Mac every one of 3,661 background runs across two stores logged `+0 entities` since
2026-08-07, while `graph extract --log N` by hand on the same archives yields normally. The
code explains the silence exactly:

- `GraphExtractionUnit::extract` (`src/serve_extract.rs`) reads only the entity and
  relationship counts from the `IngestionReport`. The per-chunk `errors` list — where a
  provider spawn failure, non-zero exit, empty output or parse failure lands — is dropped,
  and `mark_extracted` runs unconditionally right after. The CLI path prints that same list
  under an "Errors" heading, which is why the manual run is diagnosable and the daemon is not.
- The auto-started daemon runs under an allowlisted environment (`PATH`, `HOME`,
  `XDG_RUNTIME_DIR`, `RECALL_ECHO_HOME`, `RECALL_ECHO_BIN`). Its `PATH` is whatever launched
  it — a Claude Code hook or the MCP server — not the user's shell. The hook commands already
  bake an absolute `~/.local/bin/recall-echo` because that directory is *not* on that `PATH`;
  a `claude` living beside it is invisible to `Command::new("claude")`. The spawn error is
  swallowed by the first bullet. The reported 2.7s per archive is chunking and embedding with
  the model never called.

The root cause on the Mac cannot be verified from here; the fix makes it name itself.

---

## Design

### 1. Outcome of an archive is decided in one place

`IngestionReport` gains two methods (`src/graph/types.rs`):

```rust
/// Entities created or merged plus relationships created.
pub fn total_yield(&self) -> u32
/// Nothing was produced and at least one chunk or dedup step errored.
/// A report with no yield and no errors is a genuinely empty archive.
pub fn failed_outright(&self) -> bool
```

### 2. The daemon honours it

`GraphExtractionUnit::extract`:

- `failed_outright()` → return `Err(GraphError::Llm("all N extraction steps failed, nothing extracted; first: <error>"))` **without** `mark_extracted`. The worker's existing `record_failure` then does the rest: retry once, quarantine on the second failure, and after 3 consecutive failures log `background extraction off: …` and disable with that reason, which `graph status` already shows as `Extraction: off — <reason>`.
- otherwise → `mark_extracted` and return `UnitReport { entities, relationships, warnings: report.errors }`.

The worker logs warnings on their own line right after the existing yield line:
`extracted log 123 in the background: 2 warnings; first: extraction chunk 3: claude exited …`.

### 3. The CLI path follows the same rule

`graph extract` (`src/graph_cli.rs`) currently marks an archive extracted after printing a
warning label even when every chunk failed. With `failed_outright()` it prints
`✗ … nothing extracted (N errors)`, skips `mark_extracted`, and counts the archive in
`totals.errors`. Partial yield is unchanged (marked, warning label).

### 4. The CLI binary is located once, absolutely, or refused loudly

`CliSpec::locate_command()` (`src/cli_provider.rs`) → `Result<PathBuf, RecallError>`:

1. `resolve_command()` as today (`*_BIN` override, else the preset/config command).
2. If it contains a path separator: must be an executable file.
3. Else walk `PATH` (`agent_cli::resolve_binary`).
4. Else look in the well-known per-user and system install dirs, in order:
   `$HOME/.local/bin`, `$HOME/.claude/local`, `$HOME/.npm-global/bin`, `$HOME/.cargo/bin`,
   `$HOME/bin`, `/opt/homebrew/bin`, `/usr/local/bin`.
5. Else `Err(Config("<cmd> not found on PATH (<PATH>) or in <dirs searched>"))`.

The pure worker `locate_in(command, path: Option<&OsStr>, home: Option<&Path>)` is what tests
exercise. `create_provider` calls `locate_command()` for CLI providers and writes the absolute
path into the spec's `command`, so the child is spawned by path. For the daemon this turns a
per-archive swallowed spawn error into one startup line:
`background extraction off: no usable LLM provider (claude not found on PATH (/usr/bin:/bin) or in …)`
and `graph status` shows the same reason. The startup "background extraction on" line names
the resolved binary.

### 5. Daemon environment

`DAEMON_ENV_ALLOWLIST` also passes the CLI override variables `CLAUDE_BIN`, `CODEX_BIN`,
`GROK_BIN`, `GEMINI_BIN`, `RECALL_CLI_BIN`. They are paths, not credentials.

Out of scope (tracked on #42 / #55): a bulk re-extraction path for archives already marked
extracted with zero yield; recording yield alongside the `extracted` flag; a mismatch warning
for split roots (#51 / #53).

---

## Acceptance Criteria

### Happy
- AC1: A report with yield 0 and ≥1 error is `failed_outright()`; yield > 0 with errors, or yield 0 with no errors, is not. Unit tests.
- AC2: The daemon does not call `mark_extracted` for a `failed_outright` archive and returns `Err`; the worker logs `background extraction failed on log NNN: all … failed …`, quarantines after 2 attempts, and disables itself after 3 consecutive failures with the reason in the log and in `graph status`.
- AC3: A partially failed archive is marked extracted and the daemon log carries a warnings line naming the count and the first error.
- AC4: `graph extract --log N` on a `failed_outright` archive prints `✗`, does not mark it extracted, and `graph extract --all --dry-run` afterwards still lists it.
- AC5: `create_provider` for a CLI provider whose binary is only in `$HOME/.local/bin` (not on `PATH`) succeeds and the spec's command is that absolute path.
- AC6: With no resolvable binary, `create_provider` fails with a message naming the command, the `PATH` searched and the extra dirs; `serve` logs `background extraction off: no usable LLM provider (…)` once at startup.

### Edge
- AC7: A configured `[llm.cli] command = "/abs/path"` that is not executable fails with the same shape of error; a relative path with a separator is checked as a path, not on `PATH`.
- AC8: `CLAUDE_BIN` exported in the client's shell reaches the auto-started daemon.
- AC9: `locate_in` is pure: tests inject `PATH` and `HOME` and touch only a tempdir.

### Failure
- AC10: The existing CLI retry-then-quarantine for `Err` from `extract_from_archive` is unchanged; `failed_outright` archives are not retried by the CLI (they are printed and skipped), only by the daemon's own ladder.
- AC11: Full test suite passes; no test depends on a real `claude` binary.
