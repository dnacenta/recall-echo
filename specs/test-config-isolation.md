# Spec — Test config isolation: the suite can never reach the real config

**Status:** implemented (RE-59, PR pending)
**Target version:** 4.4.1 (bug fix, no user-facing surface change)
**Drafted:** 2026-09-21
**Baseline:** `main` @ `c8c66a2` (post-RE-63)
**Issue:** #59
**Relation to #63:** #63 made `~/.config/recall-echo/entity-root` steer every flagless
command. The suite overwrites that file on every run, so what was hygiene in #59 is now a
correctness-and-trust bug: `cargo test` repoints the developer's live memory at `/tmp`.

---

## Goal

`cargo test` never reads-for-write, creates or modifies any file under the real `~/.claude`,
`~/.claude.json`, `~/.codex`, or `$XDG_CONFIG_HOME/recall-echo`, whatever `CARGO_TARGET_DIR`
is set to and whatever path the test binary runs from.

## Why this matters

Verified on the VPS (2026-09-15 and 2026-09-16, 4.4.0):

- `init` guards hook and MCP installation with `is_build_dir(exe)`, which matches only
  `/target/debug/` and `/target/release/`. With a shared `CARGO_TARGET_DIR`
  (`/opt/recall-echo/.claude/shared/target`) the guard is false, and the init tests wrote
  63 hook entries into the developer's `~/.claude/settings.json`, each pointing at
  `…/deps/recall_echo-<hash> … --entity-root '/tmp/.tmpXXXX'`, plus `mcpServers.recall-echo`
  in `~/.claude.json`. The real hooks were gone; capture was silently dead.
- Worse, and independent of the exe path: `init::run_with_reader` calls
  `paths::persist_entity_root` with **no guard at all**, so *every* `cargo test` run —
  including one with a textbook `target/debug/deps` path — rewrites
  `~/.config/recall-echo/entity-root` with a `/tmp/.tmpXXXX` path. Since #63, every flagless
  command resolves through that file. Its trust gate (#63) turns the damage into a refusal
  plus a warning rather than a silent `/tmp` store, but the pin the user set is still gone.

The existing fence is the wrong shape. `is_build_dir` is a *heuristic about the running
binary* used to decide whether to write to a *global path the caller never named*. A test
cannot choose where those writes go; it can only hope the heuristic fires. The fix is to make
the destination an input.

## Design

### One injectable config root

New type `paths::ConfigRoots`: the three global destinations `init` writes to, resolved once,
at the top of the call, and passed down.

```text
  ConfigRoots
  ├── claude_dir        Option<PathBuf>  ~/.claude          (hooks: settings.json)
  ├── entity_root_file  Option<PathBuf>  $XDG_CONFIG_HOME/recall-echo/entity-root
  └── agent_home        Option<PathBuf>  HOME handed to `<cli> mcp add`
                                         (they write ~/.claude.json, ~/.codex/config.toml)
```

Two constructors, and nothing else may compute these paths:

- `ConfigRoots::from_env()` — today's behaviour exactly: `detect_claude_code()`
  (`RECALL_ECHO_CLAUDE_DIR` → `~/.claude` when it exists), `XDG_CONFIG_HOME`/`~/.config`,
  and `agent_home = None` (the child inherits this process's environment, as today).
- `ConfigRoots::sandboxed(dir)` — everything under `dir`: `dir/.claude` (created, so hook
  installation is genuinely exercised), `dir/.config/recall-echo/entity-root`, and
  `agent_home = dir`, which is exported to any spawned agent CLI as `HOME`,
  `XDG_CONFIG_HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.

`agent_home` exists because MCP registration does not write `~/.claude.json` itself — it
shells out to `claude mcp add`, which resolves that path from its own environment. The only
override that works is the child's env, so that is what `ConfigRoots` carries.

### Call chain

```text
  main.rs init ──▶ init::run ──▶ run_with_reader ──▶ run_with(root, reader, &ConfigRoots)
                                                        │
                                       ┌────────────────┼──────────────────┐
                                       ▼                ▼                  ▼
                             roots.persist_       configure_hooks    register_mcp_clients
                             entity_root(root)    (roots, root,      (…, roots) ──▶
                                                   recall_bin)       agent_cli::register_mcp
                                                                     (cli, exe, root, roots)
```

`run_with` is public; `run` and `run_with_reader` keep their signatures and pass
`ConfigRoots::from_env()`. `configure_hooks` takes the binary path as a parameter instead of
calling `recall_binary()` itself, and splits into two: the dispatcher (resolve the Claude
directory, apply the build-directory guard) and `install_hooks(settings_path, root, bin)`,
the writer. A test can then drive the writer against a named file with a production-looking
binary path and prove the write lands in the sandbox.

`paths::persist_entity_root` (free function, env-resolved) is removed in favour of
`ConfigRoots::persist_entity_root`: a writer that cannot be pointed anywhere is exactly the
bug. The read side (`persisted_entity_root`) is unchanged — reading the real file is harmless
and already env-overridable.

`agent_cli::register_mcp` gains a `&ConfigRoots` parameter (breaking for a library caller;
`init` is the only one in tree, and the MCP-registration API has no documented consumer).

### The guard, still

Routing removes the *need* for `is_build_dir`, but it stays, widened, because it also protects
a real user: a developer who runs `cargo run -- init` from a checkout would otherwise pin their
hooks to a binary that `cargo clean` deletes.

```text
  is_build_dir(exe) = cfg!(test)                       // in-crate tests, any path
                    ‖ exe contains "/target/debug/"    // default layout
                    ‖ exe contains "/target/release/"
                    ‖ exe contains "/debug/deps/"      // any CARGO_TARGET_DIR
                    ‖ exe contains "/release/deps/"
```

`cfg!(test)` covers the crate's own unit tests whatever the path; the `deps` arms cover
integration-test binaries and any custom target directory.

### Proof, not hope

A test that asserts "the guard fired" proves nothing about where a write went. The new fence
test is a sentinel: it snapshots (existence, bytes, mtime) of the three real destinations —
computed from the real `HOME`, not from an override — runs the full init flow against a
sandbox, and asserts all three are byte- and mtime-identical afterwards, while the sandbox
copies exist.

No test sets a process-global environment variable. Isolation is an argument, so there is
nothing to race and no serial-test lock to remember.

## Out of scope

- `resolve_init_root` (`init`'s own root choice: env → `~/.claude` → cwd). Noted on #53.
- The read side of the persisted root and `RECALL_ECHO_HOME` semantics (#63).
- A user-facing `--config-dir` flag. `ConfigRoots` makes one a small change later; this spec
  adds no CLI surface.
- `tests/flagless_root.rs`, which already sandboxes the spawned binary with `HOME` and
  `XDG_CONFIG_HOME`; it keeps doing that because it exercises a child process.

---

## Acceptance criteria

- **AC1**: `init::run_with(root, reader, &ConfigRoots::sandboxed(dir))` writes hooks,
  the persisted entity-root file, and any MCP config strictly under `dir`.
- **AC2**: Every in-crate test that calls `init::run*`, `configure_hooks`/`install_hooks` or
  MCP registration passes a sandboxed `ConfigRoots`. No test calls `init::run` or
  `init::run_with_reader` (the two entry points that resolve the real roots for themselves).
  `ConfigRoots::from_env()` may only be *read* by a test asserting production resolution
  (AC7) — never handed to a writer.
- **AC3**: A sentinel test snapshots the real `~/.claude/settings.json`, `~/.claude.json` and
  `$XDG_CONFIG_HOME/recall-echo/entity-root` (existence + bytes + mtime, resolved from the
  real `HOME` with no override in effect), runs the full init flow against a sandbox, and
  asserts each is unchanged — and, for a file that did not exist, still absent.
- **AC4**: `configure_hooks` driven with a production-looking binary path
  (`/usr/local/bin/recall-echo`) and a sandboxed `ConfigRoots` writes the three hooks into
  `<sandbox>/.claude/settings.json` and leaves the real settings.json untouched.
- **AC5**: `ConfigRoots::persist_entity_root` writes `<sandbox>/.config/recall-echo/entity-root`
  (0600 in a 0700 directory, as #63 AC10 requires) and never the real file.
- **AC6**: `is_build_dir` is true for `/target/debug/`, `/target/release/`, `/debug/deps/` and
  `/release/deps/` paths and under `cfg!(test)`; false for `/usr/local/bin/recall-echo` and
  `~/.cargo/bin/recall-echo`.
- **AC7**: Production behaviour is unchanged: with `ConfigRoots::from_env()` the destinations
  are the same paths 4.4.1 writes today, and `RECALL_ECHO_CLAUDE_DIR` still overrides the
  Claude directory.
- **AC8**: MCP registration spawned from a sandboxed `ConfigRoots` runs the agent CLI with
  `HOME`, `XDG_CONFIG_HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` pointing into the sandbox,
  so a client that ignores our flags still cannot reach the real `~/.claude.json`.

## Tests

- `init.rs`: `the_test_binary_is_recognised_as_a_build_directory` (AC6, extended with the
  `deps` shapes), `init_writes_only_inside_the_sandbox` (AC1),
  `the_real_config_is_untouched_by_the_init_flow` (AC3, the sentinel),
  `hooks_are_written_into_the_injected_claude_dir` (AC4), and the four existing `run_with_reader`
  tests converted to `run_with` + sandbox (AC2).
- `paths.rs`: `sandboxed_roots_point_at_the_sandbox` and
  `persisting_through_sandboxed_roots_leaves_the_real_file_alone` (AC5),
  `roots_from_env_honour_the_claude_dir_override` (AC7).
- `agent_cli.rs`: `mcp_registration_env_points_at_the_sandbox` (AC8) — asserts the env pairs
  applied to the child command, without spawning a CLI.
- `status.rs`: `status_on_initialized_env` converted to `run_with` + sandbox (AC2).
