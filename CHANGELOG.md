# Changelog

Release notes for tagged versions are generated on GitHub; this file records
changes a user has to know about before upgrading.

## [4.6.2] — Unreleased

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

## [4.6.1] — Unreleased

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
