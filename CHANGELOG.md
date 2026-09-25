# Changelog

Release notes for tagged versions are generated on GitHub; this file records
changes a user has to know about before upgrading.

## [4.6.0] — Unreleased

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
