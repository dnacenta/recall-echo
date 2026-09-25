# Changelog

Release notes for tagged versions are generated on GitHub; this file records
changes a user has to know about before upgrading.

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
