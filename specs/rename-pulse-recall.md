# Spec — Rename recall-echo to pulse-recall

**Status:** draft
**Target version:** 4.3.0 → 5.0.0 (breaking; ships with the v5 release train)
**Drafted:** 2026-09-02
**Baseline:** `main` @ `dc4f24d` (v4.3.0)

---

## Goal

The product is called **pulse-recall**. Crate, binary, repository, config file, env vars,
XDG directories, runtime directory, MCP server name, release assets, installer, docs. One
name everywhere, with a one-major compatibility window so every existing install (D's Mac,
Vigil, Echo) migrates by running the new binary once.

## Why this matters

"recall-echo" was named after an entity that no longer exists. "Recall" alone collides with
Recall.ai, Windows Recall, and two same-niche Claude Code memory tools literally named
Recall; "Echo" is Amazon's. pulse-recall sits in the pulse-null family (pulse-null,
pulse-null-voice, vigil-pulse), is free on crates.io and GitHub as of 2026-09-02, and is
distinctive enough to own. This lands **first** in the v5 train because every later PR
touches names in docs, hooks and paths; doing it last would mean writing everything twice.

---

## Design

### Names

| Surface | Old | New | Compat |
| --- | --- | --- | --- |
| crates.io crate / Cargo `name` | `recall-echo` | `pulse-recall` | old crate left at 4.3.0, README on the old crate page points here (GitHub redirect) |
| Rust lib | `recall_echo` | `pulse_recall` | none (pulse-null follow-up, see Delivery) |
| Binary | `recall-echo` | `pulse-recall` | binary behaves identically under any argv[0]; when argv[0] file name is `recall-echo` it prints one stderr line: `recall-echo is now pulse-recall — run 'pulse-recall init' to migrate hooks and registrations` |
| GitHub repo | `dnacenta/recall-echo` | `dnacenta/pulse-recall` | GitHub redirects old URLs, git remotes and API calls |
| Config file | `memory/.recall-echo.toml` | `memory/.pulse-recall.toml` | `init` renames it in place; other commands read the old name with a one-line deprecation warning |
| XDG config dir | `~/.config/recall-echo/` | `~/.config/pulse-recall/` | first run copies `entity-root` across (never deletes the old dir) |
| Runtime dir | `$XDG_RUNTIME_DIR/recall-echo/` | `$XDG_RUNTIME_DIR/pulse-recall/` | none; a running 4.x daemon is left alone |
| Env | `RECALL_ECHO_HOME`, `RECALL_LLM_API_KEY`, `RECALL_LLM_MAX_RETRIES`, `RECALL_LLM_RETRY_DELAY_MS` | `PULSE_RECALL_HOME`, `PULSE_RECALL_LLM_API_KEY`, `PULSE_RECALL_LLM_MAX_RETRIES`, `PULSE_RECALL_LLM_RETRY_DELAY_MS` | old names honoured with a deprecation warning for the 5.x line |
| MCP server name | `recall-echo` | `pulse-recall` | `init` removes the old registration from every client where it finds one, then adds the new; tool names `recall_*` are unchanged |
| MCP `serverInfo.name` / instructions text | `recall-echo` | `pulse-recall` | — |
| Hook commands in `~/.claude/settings.json` | `<path>/recall-echo …` | `<path>/pulse-recall …` | `init` rewrites hooks whose command's binary basename is `recall-echo` (canonical-shape matcher from RE-42; operator-carrying hooks such as `… \|\| true` are rewritten only in the binary path segment, never in operators) |
| Update temps | `.recall-echo-update.*`, `.recall-echo-update-probe.*` | `.pulse-recall-update.*`, `.pulse-recall-update-probe.*` | stale-sweep also removes the old prefixes |
| Release assets | `recall-echo-<target>.tar.gz` | `pulse-recall-<target>.tar.gz` | **5.0.x releases also attach `recall-echo-<target>.tar.gz`** containing the same binary as an entry named `recall-echo`, so a 4.x `recall-echo update` migrates in one hop (4.x self-check only requires the version word to appear in `--version` output — verified `update.rs:559`) |
| `install.sh` | `REPO=dnacenta/recall-echo`, `BIN=recall-echo` | `dnacenta/pulse-recall`, `pulse-recall` | raw URL under the old repo path keeps working via redirect |
| Feature flags | `pulse-null`, `server`, `embedded`, `llm`, `self-update`, `bench` | unchanged | — |
| Workflow prefix | `RE` | `RE` for the v5 train; `PR` from the next issue after release | `~/.claude/skills/workflow/SKILL.md` project map updated at deploy |

### What is NOT renamed

- MCP tool names (`recall_query`, `recall_search`, `recall_episodes`, `recall_traverse`,
  `recall_overview`, `recall_status`) — short, descriptive, already registered in clients.
- The `pulse-null` cargo feature and the `Plugin` role — pulse-null's concern.
- Historic specs under `specs/` and `docs/benchmarks/` — dated records; a header note is
  added to `specs/README.md` (new, 5 lines) saying older specs use the old name.
- Git history, tags ≤ v4.3.0.

### Migration behaviour in `init`

`init` is already idempotent. On a machine with a 4.x install it additionally:

1. Renames `memory/.recall-echo.toml` → `memory/.pulse-recall.toml` if only the old exists.
2. Copies `~/.config/recall-echo/entity-root` → `~/.config/pulse-recall/entity-root` if only the old exists.
3. Rewrites hook entries whose binary basename is `recall-echo` to the current executable path.
4. For each detected CLI: if an MCP server named `recall-echo` is registered, `mcp remove` it, then register `pulse-recall`.
5. Prints a migration summary listing exactly what changed and what was left (e.g. the old binary file, which it never deletes).

All of this is reported, none of it is silent.

---

## Acceptance criteria

- AC1: `cargo build --release` produces `target/release/pulse-recall`; `pulse-recall --version` prints `pulse-recall 5.0.0`.
- AC2: `grep -rn 'recall-echo\|recall_echo\|RECALL_ECHO' src/ tests/ install.sh .github/ README.md CONTRIBUTING.md templates/ Cargo.toml` returns only the compatibility sites enumerated in the Names table (asserted by a test that lists the allowed files and fails on any other hit).
- AC3: Running the binary via a symlink or copy named `recall-echo` prints the single migration line to stderr and otherwise behaves identically (test: spawn with argv[0] override).
- AC4: With only `memory/.recall-echo.toml` present, `status` loads it and prints one deprecation warning; `init` renames it and the warning stops.
- AC5: With only `~/.config/recall-echo/entity-root` present, flagless hooks resolve through it; after any command the new path exists with identical content and the old is untouched.
- AC6: `init` on a settings.json carrying `/usr/local/bin/recall-echo archive-session || true` rewrites it to `<exe> archive-session || true` (operator preserved) — extends the existing hook-upsert tests.
- AC7: `init` on a client with an MCP server `recall-echo` registered removes it and registers `pulse-recall` (argv-level test through the existing `mcp_add_argv` / removal seam).
- AC8: `RECALL_ECHO_HOME=/x pulse-recall status` behaves as `PULSE_RECALL_HOME=/x` and warns once.
- AC9: `release.yml` on a `v5.*` tag uploads both `pulse-recall-<target>.tar.gz` and `recall-echo-<target>.tar.gz` for all four targets, and the legacy tarball's entry is named `recall-echo` (workflow asserts with `tar tzf`).
- AC10: `update.rs` looks for `pulse-recall-<target>.tar.gz`, extracts the entry named `pulse-recall`, and its stale sweep removes both old and new temp prefixes.
- AC11: 801 existing tests still pass (renamed where they assert names), plus the new ones above.

## Verification

`cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`. Manual: run
`target/release/pulse-recall init` on Vigil's own root (`/root/.claude`) in a copy and
diff settings.json / config dir before and after.

## Delivery

- Branch `chore/RE-<n>-rename-pulse-recall`, squash-merged to `main`.
- After merge: `gh repo rename pulse-recall` (GitHub redirects). Crate publish waits for the
  v5.0.0 release train (release-integrity spec).
- Follow-ups outside this repo, filed as issues, not done here:
  - pulse-null: dependency `recall-echo` → `pulse-recall`, `use recall_echo` → `use pulse_recall` (30 sites in 10 files), `[plugins.recall-echo]` config section → `[plugins.pulse-recall]` with a compat alias.
  - VPS: install pulse-recall, leave a `recall-echo` symlink for the sudoers-pinned `recall-deploy` wrapper, re-run `init` as root and as pulse.
  - `~/.claude/skills/workflow/SKILL.md` project map row.
