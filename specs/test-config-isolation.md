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
  ├── claude_dir        Option<PathBuf>            ~/.claude   (hooks: settings.json)
  ├── entity_root_file  Result<PathBuf, &str>      $XDG_CONFIG_HOME/recall-echo/entity-root,
  │                                                or why no pointer is written
  ├── agent_home        Option<PathBuf>            HOME handed to `<cli> mcp add`
  │                                                (~/.claude.json, ~/.codex/config.toml)
  ├── recall_bin        String                     the binary hooks and MCP point at
  └── spawns_agents     bool                       may this run an agent CLI / fetch a model?
```

Two constructors, and nothing else may compute these paths:

- `ConfigRoots::from_env()` — today's behaviour: `detect_claude_code()`
  (`RECALL_ECHO_CLAUDE_DIR` → `~/.claude` when it exists), `XDG_CONFIG_HOME`/`~/.config`,
  `recall_bin = current_exe()`, `agent_home = None` (the child inherits this process's
  environment, as today), `spawns_agents = true`.
- `ConfigRoots::sandboxed(dir)` — everything under `dir`, created: `dir/.claude` (0700, so
  hook installation is genuinely exercised), `dir/.config/recall-echo/entity-root`,
  `recall_bin = dir/bin/recall-echo` (production-shaped, so no guard short-circuits the
  writers), `agent_home = dir`, exported to any spawned agent CLI as `HOME`,
  `XDG_CONFIG_HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME`, and `spawns_agents = false`.
  `.without_claude_code()` yields the same roots with no hook directory at all.

The binary path belongs here for the same reason the destinations do: it decides what gets
written into those files, and every guard reads it. `spawns_agents` is an explicit answer to
"may this configuration touch the machine?" — MCP registration and the 127 MB embedding-model
download are both skipped when it is false. Neither is a compile-time `cfg`, so the production
binary and the test binary run the same code.

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
`ConfigRoots::from_env()`. `configure_hooks` takes its binary from `roots` instead of calling
`recall_binary()` itself, and splits into two: the dispatcher (resolve the Claude directory,
apply the build-directory guard) and `install_hooks(settings_path, root, bin)`, the writer.
The sentinel therefore drives the whole flow — dispatcher included — and watches the hooks
land in the sandbox.

`paths::persist_entity_root` (free function, env-resolved) is removed in favour of
`ConfigRoots::persist_entity_root`, which returns `PersistOutcome::{Written, Skipped}`: a
writer that cannot be pointed anywhere is exactly the bug, and "no pointer, and why" is an
outcome rather than an error. The pointer is guarded exactly as the hooks are — a binary in a
build directory persists nothing, so `cargo run -- init /tmp/scratch` from a checkout no
longer repoints every flagless command at `/tmp/scratch`. The read side
(`persisted_entity_root`) is unchanged.

`agent_cli::register_mcp` gains a `&ConfigRoots` parameter (breaking for a library caller;
`init` is the only one in tree, and the MCP-registration API has no documented consumer).

### The guard, still

Routing removes the *need* for the build-directory guard, but it stays — rewritten — because
it protects a real user: a developer who runs `cargo run -- init` from a checkout would
otherwise pin their hooks, MCP registrations and global root pointer to a binary that
`cargo clean` deletes.

```text
  is_build_dir(exe) = parent(exe) named debug | release
                    ‖ parent(exe) named deps and grandparent named debug | release
```

The shape Cargo produces, not the substring `target`: `CARGO_TARGET_DIR` renames that
directory freely, so `/home/d/build/debug/recall-echo` is a build binary and
`/opt/apps/release/deps/bin/recall-echo` is not. There is deliberately **no `cfg!(test)`
arm**: it would make the guard fire in the crate's own tests, the writers would return before
writing anything, and the sentinel below would prove nothing. Tests are fenced by routing, not
by compiling differently.

### Proof, not hope

A test that asserts "the guard fired" proves nothing about where a write went. The sentinel
runs the full init flow against a sandbox with a production-shaped binary path — so
`configure_hooks` really writes — then asserts the hooks are in the sandbox's `settings.json`
and that the real `~/.claude/settings.json`, `~/.claude.json` and entity-root pointer are
unchanged.

"Unchanged" is asserted on a **digest** (exists, length, SHA-256), never on contents: this
runs on a developer's machine, and a failure message that dumped `~/.claude.json` would print
every project path and MCP credential on it. `settings.json` and the pointer file are strict —
bytes and mtime. `~/.claude.json` is not: Claude Code rewrites it continuously during a live
session, which is exactly when the suite runs, so its mtime is used as a *witness* (unchanged
mtime ⇒ nobody else wrote ⇒ the bytes must match; a newer one ⇒ somebody did, and the shape
assertions carry it). On top of the digests, two shape assertions that hold either way: no
hook command mentioning `recall-echo` and no `mcpServers` key appears that was not there
before. The real paths are computed in the test from `HOME` and `XDG_CONFIG_HOME` directly,
not asked of the code under test.

No test sets a process-global environment variable. Isolation is an argument, so there is
nothing to race and no serial-test lock to remember. A source-scan test (in the style of the
theme escape scan) fails the build if `run_with_reader(`, `init::run(` or
`ConfigRoots::from_env()` appears in any `#[cfg(test)]` region or `tests/` file without an
explicit `sanctioned:` marker.

### Host dependence

`init` also *reads* the machine: `agent_cli::installed()` looks for `claude`, `codex`, `grok`
and `gemini` on `PATH`, and `agent_cli::capturing()` walks `~/.claude`, `~/.codex` and
`~/.grok` for recorded sessions. Both are read-only and stay that way, but they mean the
summary a sandboxed init prints — which CLIs it would register with — depends on the machine
running the suite. No assertion in these tests depends on that answer.

## Out of scope

- `resolve_init_root` (`init`'s own root choice: env → `~/.claude` → cwd). Noted on #53.
- The read side of the persisted root and `RECALL_ECHO_HOME` semantics (#63).
- A user-facing `--config-dir` flag. `ConfigRoots` makes one a small change later; this spec
  adds no CLI surface.
- `tests/flagless_root.rs`, which already sandboxes the spawned binary with `HOME` and
  `XDG_CONFIG_HOME`; it keeps doing that because it exercises a child process.
- `install_hooks`' shape (pre-existing) and the `HOME` resolution outside `init`
  (`archive`, `cli_provider`) — tracked under #53/#55.

### Public API

This is a breaking library change, so the release carrying it is **4.5.0**, not 4.4.1:
`paths::persist_entity_root` is gone (use `ConfigRoots::persist_entity_root`), and
`agent_cli::register_mcp` takes a fourth argument. The CLI surface is unchanged.

---

## Acceptance criteria

- **AC1**: `init::run_with(root, reader, &ConfigRoots::sandboxed(dir))` writes the hooks, the
  persisted entity-root pointer and any MCP config strictly under `dir`.
- **AC2**: Every in-crate test that calls `init::run*`, `configure_hooks`/`install_hooks` or
  MCP registration passes a sandboxed `ConfigRoots`. No test calls `init::run` or
  `init::run_with_reader` (the two entry points that resolve the real roots for themselves).
  `ConfigRoots::from_env()` may only be *read* by a test asserting production resolution —
  never handed to a writer — and only on a line marked `sanctioned:`. Enforced by a
  source-scan test, not by convention.
- **AC3**: A sentinel test snapshots a digest (existence, length, SHA-256) of the real
  `~/.claude/settings.json`, `~/.claude.json` and `$XDG_CONFIG_HOME/recall-echo/entity-root`,
  resolved from the real `HOME` inside the test, runs the full init flow against a sandbox,
  and asserts each is unchanged — bytes and mtime for the two files nothing else rewrites,
  bytes-when-the-mtime-says-nobody-else-wrote for `~/.claude.json`, and no new recall-echo
  hook command or `mcpServers` key in either JSON. Failure messages name paths only, never
  contents.
- **AC4**: The same sentinel run leaves the three hooks in `<sandbox>/.claude/settings.json`,
  written through the `configure_hooks` dispatcher with no guard short-circuiting it, naming
  the sandbox's own binary path.
- **AC5**: `ConfigRoots::persist_entity_root` writes `<sandbox>/.config/recall-echo/entity-root`
  (0600 in a 0700 directory, as #63 AC10 requires) and never the real file. With
  `from_env()` from a build directory it writes nothing and reports
  `PersistOutcome::Skipped("running from a build directory")`, which `init` prints as a
  skipped step.
- **AC6**: `is_build_dir` is true for a binary whose parent directory is `debug`/`release`, or
  `deps` inside one, whatever the target directory is called
  (`/home/d/build/debug/recall-echo` included); false for `/usr/local/bin/recall-echo`,
  `~/.cargo/bin/recall-echo`, `/opt/apps/release/deps/bin/recall-echo` and
  `/home/d/debug/bin/recall-echo`. There is no `cfg!(test)` arm.
- **AC7**: Production resolution is unchanged: `from_env()` names the same Claude directory
  `detect_claude_code()` does — including its `RECALL_ECHO_CLAUDE_DIR` override, which is
  unchanged by this work and, being a process-global env var, is only safely exercised by a
  subprocess test in the style of `tests/flagless_root.rs` (none is added here) — and spawns
  agent CLIs with the ambient environment.
- **AC8**: MCP registration built from a sandboxed `ConfigRoots` carries `HOME`,
  `XDG_CONFIG_HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` pointing into the sandbox, so a
  client that ignores our flags still cannot reach the real `~/.claude.json`. A sandbox does
  not spawn one at all (`spawns_agents() == false`).

## Tests

- `init.rs`: `the_real_config_is_untouched_by_the_init_flow` (AC3 + AC4, the sentinel),
  `init_persists_the_entity_root_inside_the_sandbox` (AC1),
  `hooks_are_skipped_when_claude_code_is_absent` (the no-Claude-Code branch), the four
  existing `run_with_reader` tests converted to `run_with` + sandbox (AC2), and
  `suite_fence::no_test_reaches_the_real_configuration` (AC2, the source scan).
- `paths.rs`: `sandboxed_roots_point_at_the_sandbox` (AC1),
  `persisting_through_sandboxed_roots_leaves_the_real_file_alone` (AC5),
  `from_env_in_a_build_directory_persists_nowhere` (AC5, AC7),
  `build_directories_are_recognised_by_shape` (AC6),
  `roots_without_claude_code_name_no_hook_directory`.
- `agent_cli.rs`: `mcp_registration_env_points_at_the_sandbox` (AC8) and
  `mcp_registration_inherits_the_environment_in_production` (AC7) — both assert the env
  applied to the child command, without spawning a CLI.
- `status.rs`: `status_on_initialized_env` converted to `run_with` + sandbox (AC2).
