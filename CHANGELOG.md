# Changelog

Release notes for tagged versions are generated on GitHub; this file records
changes a user has to know about before upgrading.

## [4.8.0] — Unreleased

### Fixed

- MEMORY.md now reaches Claude Code sessions (#75). Before, nothing loaded
  it: the SessionStart hook printed EPHEMERAL.md only, and the rules template
  that claimed MEMORY.md was "auto-loaded" was never installed. `init` now
  writes `~/.claude/rules/recall-echo.md`, a memory file whose single `@`
  import is the store's `memory/MEMORY.md`. Claude Code inlines it whole on
  every startup, resume and `/clear`. The hook could not carry it: Claude Code
  inlines hook output only up to about 10,000 characters and replaces the
  rest with a preview and a file path.
  - The file is 0600, in a `rules/` directory created 0700. `init` warns
    when an existing `rules/` is writable by others or owned by another uid.
  - It is written only when the root, `memory/` and MEMORY.md are this
    user's alone: owned by them, writable by nobody else, and MEMORY.md a
    regular file rather than a symlink. Group-write is fine when the group is
    the user's private group (Ubuntu and Fedora, umask 0002). Each refusal
    prints the `chmod g-w,o-w` that fixes it. `init` creates `memory/` 0700
    and its files 0600 whatever the umask, and `status` reports a store that
    would be refused. A root with whitespace, control characters, a backslash
    or invisible format characters is refused too.
  - When the rules step is refused, or skipped for a pulse, a managed rules
    file that imports this store is removed, and `init` says so.
  - A file counts as managed when its first line starts with
    `<!-- recall-echo:managed`. A hand-written file is never touched; `init`
    says whether it already imports this MEMORY.md or prints the line to add.
    A symlink or FIFO at that path is never read or replaced.
- The SessionStart hook also fires on `/clear`, so a cleared session still
  gets the recent-session summaries.
- The rules template matches the 4.x layout. It used to point at
  `~/.claude/EPHEMERAL.md` and `~/.claude/conversations/` and describe
  checkpoint steps that `checkpoint` now does itself. It now also tells the
  model never to put secrets or `@` lines in MEMORY.md.

### Added

- `status`, `distill` and `init` warn when MEMORY.md carries `@` import
  lines. MEMORY.md is imported into every session, and Claude Code follows
  imports inside it.
- `init` and `status` report MEMORY.md's size in bytes and approximate
  tokens. `distill` suggests distilling above 25 KB as well as at 140 lines.
- `init` warns when a skipped or refused run leaves a managed rules file
  importing a different store's MEMORY.md, and leaves that file in place.

### Changed

- When Claude Code's built-in auto-memory is on, `init` asks once whether to
  turn it off, so there is one curated store. Enter or `y` writes
  `"autoMemoryEnabled": false` to `~/.claude/settings.json`; `n` or end of
  input leaves it on. `init` asks only when stdin and stderr are both
  terminals; otherwise it never changes the setting and prints a notice
  (this also applies to the provider menu). Notes already under
  `~/.claude/projects/*/memory/` are not moved.
- `init` reads settings.json once and writes it at most once, after every
  step. The bytes it replaces are kept as `settings.json.bak`. If the file
  changes while `init` is running, the new content is kept and the changes
  are applied to it again; if it is no longer valid JSON, nothing is written.
  The backup is owner-only and never written through a symlink. A
  settings.json that is a symlink, or becomes one during the prompt, is never
  replaced; `init` prints what to add instead.

### Upgrading

- Re-run `recall-echo init` to get the rules file and the new hook matcher.
- A hand-written `~/.claude/rules/recall-echo.md` (no
  `recall-echo:managed` first line) is left alone. To switch to the managed
  file, move yours aside and re-run `init`.
- Pulse-null pulses are unaffected. `init` writes no rules file and asks no
  auto-memory question for a root holding `pulse-null.toml`, or for a user
  whose home holds pulses (`~/pulse-null/<n>`, `~/pulse-null/pulses/<n>`,
  `~/pulse-null/entities/<n>` or `~/entity`), and says why. For such a user's
  own `~/.claude` it writes no user-level hooks either: they would fire for
  every pulse. `consume` prints exactly what it printed before.

## [4.7.0] — 2026-10-17

### Fixed

- Claude Code subagent runs are no longer archived as sessions (#74). On one
  install 156 of 251 archives were subagents: the capture sweep and `ingest`
  took every `*.jsonl` under `projects/`, including
  `<session>/subagents/agent-<id>.jsonl`. They crowded the five-slot
  EPHEMERAL.md window and entered the parent model's prompts into the graph
  as `user` evidence.
  - A transcript is a subagent's when its directory is `subagents`, or when
    the first entry carrying `isSidechain` has it `true` (a fork's
    transcript opens with an unflagged `fork-context-ref`). The read is
    bounded: 64 lines, blank ones included, and 256 KiB; only lines
    mentioning `"isSidechain"` are parsed, and an undecodable line is
    skipped rather than ending the read.
  - Discovery sets aside `subagents/` files by layout, unread. The flag is
    read only for transcripts not already archived, so finished sessions
    are not re-read on every sweep. Capture, `ingest` and
    `archive --all-unarchived` skip both kinds; the SessionEnd and
    PreCompact hooks decline them with one stderr line and exit 0.
  - Already-written subagent archives are left in place; pruning them is
    #76. EPHEMERAL.md clears itself within five real sessions.
- `archive --all-unarchived` writes into `--pulse-root`, `RECALL_ECHO_HOME`
  or the root `init` persisted, and never takes its store from the current
  directory, so a cloned repo that looks initialised cannot steer it. With
  none of those pinned it falls back to the legacy `~/.claude`, and says so
  on stderr. It names the store and why before writing. It used to write
  `~/.claude/conversations` unconditionally, which on a 4.x install failed
  with "conversations/ directory not found".
- Claude Code discovery reads regular files only: a FIFO or a link to a
  device named `*.jsonl` is skipped instead of hanging the sweep.

### Added

- `archive --all-unarchived --pulse-root <dir>` (alias `--entity-root`).
- Skipped subagent runs are counted: `Pending::subagents_skipped`,
  `CaptureReport::subagents_skipped`, shown by `ingest` and
  `archive --all-unarchived`. With `RECALL_ECHO_DEBUG` set, each skip is
  named on stderr with the signal that caught it.
- `jsonl::is_subagent_transcript`, `Transcript::is_sub_run`,
  `Transcript::discover_with_skipped` (both provided),
  `archive::archive_all_with` (returns `archive::BatchArchiveReport`,
  `#[non_exhaustive]`), `archive::archive_all_unarchived_at`,
  `checkpoint::run_with_hook_input`.

### Changed

- `capture::pending` checks the archive before the settle time: a live
  transcript whose session is already archived counts as a duplicate, not
  as active.
- **Source-breaking for struct literals:** `capture::Pending` and
  `capture::CaptureReport` gain a public `subagents_skipped` field. Both have
  only public fields and no `#[non_exhaustive]`, so code that builds either
  with a struct literal not ending in `..Default::default()` stops
  compiling. Field access and `Default` are unaffected. Both become
  `#[non_exhaustive]` in v5.

### Deprecated

- `archive::archive_all_with_base`: use `archive_all_with`, which takes the
  store and the transcripts separately.

## [4.6.2] — 2026-09-25

### Fixed

- Extraction no longer drops a whole chunk over one bad answer. Measured on a
  real 54-chunk archive (claude-code, sonnet): 3 of 54 first answers were
  rejected by 4.6.1 and contributed nothing — two with `"relationships": [[]]`
  (once followed by `.length ? null : null`), one with an entity typed
  `artifact`. Now 54 of 54 extract.
  - The answer is read with a real JSON parser from its first `{`: fences and
    prose around the object are ignored, and a `}` inside a string no longer
    ends it early.
  - Arrays are read element by element. A malformed element costs itself;
    `null`/`[]`/`{}` placeholders are skipped; an entity type outside the
    schema is kept as `concept`.
  - An unusable answer gets exactly one retry: a truncated answer is asked
    again as two halves of the chunk, anything else is asked again whole. If
    the retry fails too, the complete leading elements of a truncated answer
    are salvaged (the cut is closed and re-parsed — never guessed).
  - Warnings name the failure class (no JSON / truncated / invalid JSON /
    unexpected shape), the parser's position and the answer's size, instead
    of quoting its first 200 characters. A chunk recovered with loss is
    reported as `extraction chunk N (recovered): …`.
  - Calls that yielded nothing are billed: an archive whose every chunk
    failed reports what those calls cost.
- Dedup decisions wrapped in prose were rejected even when the JSON object in
  them was valid.
- Relationships are no longer lost to dedup. Replaying the same archive, 55
  of 395 extracted relationships failed with `entity not found` although 51
  of them named an entity the run had extracted; now 4 warnings remain, each
  for an endpoint no chunk ever extracted (`User`, `Rust`).
  - Every candidate name is recorded, case-folded, against the stored entity
    it resolved to — created, merged, or skipped as a duplicate — and
    relationship endpoints resolve through that before the store is asked.
    `synth` finds `Synth`; a skipped `Synth pulse` finds the `Synth` it
    duplicates.
  - An endpoint from an earlier archive is found in any case.
  - A relationship whose endpoints resolve to one entity is dropped.
  - The warning says whether the endpoint was never extracted or was
    extracted and failed dedup.
- Chunk answers are deduplicated in transcript order, not in the order the
  model finished them, so an archive resolves the same way on every run.

### Added

- `graph::extract::extract_chunk`, returning `ChunkExtraction` (result,
  usage, calls, recovery notes) or `ChunkFailure` (error, usage, calls).
  `extract_from_chunk` keeps its signature.
- `graph::llm_json`: `first_json_object`, `salvage_truncated`, `JsonFailure`.
- `ExtractionResult::element_count` and `ExtractionResult::append`.
- `graph::aliases::EntityAliases`, `GraphMemory::get_entity_ignoring_case`.

### Changed

- `dedup::ResolvedEntity::Skipped` carries the stored entity the candidate
  duplicates (`Skipped(Entity)`). A model-issued skip names no target; the
  nearest neighbour it was shown is taken.

## [4.6.1] — 2026-09-25

### Fixed

- Extraction finds `grok` in `~/.grok/bin`, where its official installer puts
  it. The installer only adds that directory to the shell's rc file, so a
  service or background process never had it on `PATH` and every extraction
  failed with "grok not found". The directory is searched for `grok` alone.
- A CLI provider that exits non-zero with nothing on stderr reports the first
  line of its stdout instead: `claude -p` says "Not logged in" there and
  nowhere else, and "exited 1:" alone could not be diagnosed.

### Added

- `archive_extract::extract_archive(memory_dir, log, &CliOverrides)` (feature
  `llm`): extract one archive with the provider `.recall-echo.toml`
  configures, in either graph mode, without handing recall-echo a model.
  For hosts that archive their own conversations (pulse-null) and so know
  when an archive needs extracting — `[graph] mode = "server"` has no daemon
  to do it in the background. One call is one attempt; it reports the tokens
  spent, skips an archive with nothing pending, and leaves an archive pending
  when every chunk failed.
- `CliOverrides` — how a host wants the CLI spawned: `command` (wins over
  `[llm.cli] command`, `*_BIN` and the search), `env` (the child's entire
  environment, for a host that allowlists what its agent CLI sees, login
  included) and `current_dir` (agent CLIs read project instructions and hooks
  from it). All `None` is exactly what `graph extract` does.
- `llm_provider::create_provider_with_overrides` and
  `GraphMemory::log_awaits_extraction`, which the above is built on.

The binary a CLI provider spawns can already be pinned in config, and still
wins over the search: `[llm.cli] command = "/home/you/.grok/bin/grok"`.

## [4.6.0] — 2026-09-25

The agent home recall-echo serves is now called a **pulse** (a pulse-null
pulse such as Echo or Synth). The knowledge-graph *entity* — what extraction
pulls out of conversations — is unchanged.

### Changed

- `--entity-root` is now `--pulse-root` on every command that takes it
  (`archive-session`, `checkpoint`, `config`, `mcp`, `what-do-you-know`,
  `graph`, `bench ingest|answer`). Positional roots (`init`, `status`,
  `distill`, `consume`, `dashboard`) are unchanged apart from their help text.
- `init` persists the root to `~/.config/recall-echo/pulse-root`
  (`$XDG_CONFIG_HOME` honoured) and writes `--pulse-root` into the Claude Code
  hooks and MCP registrations it installs. Re-running `init` rewrites plain
  `--entity-root` hooks in place; customised ones (`|| true`, wrappers) are
  reported and left alone.
- Archives written for a pulse-null session record the pulse as `pulse:` in
  their frontmatter.
- The pulse-null plugin's setup prompt key is `pulse_root`.
- Library: `paths::pulse_root`, `paths::pulse_root_described`,
  `paths::persisted_pulse_root`, `paths::hook_pulse_root`,
  `ConfigRoots::{pulse_root_file, persist_pulse_root}` and
  `RecallEcho::pulse_root` replace their `entity_*` names;
  `SessionMetadata::entity_name` is now `pulse_name`.

### Deprecated — still accepted

- `--entity-root`, as a hidden alias of `--pulse-root`.
- `~/.config/recall-echo/entity-root`, read when no `pulse-root` file exists
  (with a one-line stderr note, and `status` labels the root as coming from
  the legacy file). It is never written, and never deleted.
- The `entity:` frontmatter key, read as the pulse when no `pulse:` is present.
- The plugin config key `entity_root`.
- The library's `entity_*` functions and methods, as `#[deprecated]` wrappers.
