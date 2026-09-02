# Spec — Front door: CLI surface, uninstall, doctor, atomic writes

**Status:** draft
**Target version:** 5.0.0
**Drafted:** 2026-09-02
**Baseline:** `main` @ `dc4f24d` (v4.3.0), on top of the rename and trust-boundary specs
**Closes:** #45 items 1, 2, 5

---

## Goal

A stranger runs `pulse-recall --help` and sees a product, not a personal toolbox. Every
command they might touch in the first hour is coherent, reversible, and diagnosable.

## Why this matters

Audit 2026-09-02: 16 top-level + 17 `graph` subcommands. Three overlapping capture verbs
(`archive-session`, `archive --all-unarchived`, `ingest --from`), `status` vs `dashboard`
render the same thing, `graph ingest` vs `graph ingest-all`, stale help text (extract default
still names haiku; archive-session says "Claude Code" though it reads Gemini), and
`graph pipeline`, `graph vigil-sync`, `graph feedback` plus a README section that only make
sense inside D's pulse-null stack. There is no uninstall, no diagnostic bundle, and
`dashboard::auto_distill` is a dead, untested, destructive `pub fn`. EPHEMERAL.md and
archives are written non-atomically while `init.rs:566` already has the right helper.

---

## Design

### 1. Command surface (v5)

```
pulse-recall
  init [path] [--no-default] [--capture <cli>,…|all]   register a root, install hooks, register MCP
  status [--json]                                     health + stats (absorbs `dashboard`)
  doctor [--json]                                     diagnostics for support
  capture                                             hook entry: one session from stdin JSON (was archive-session)
  capture --sweep [--from <cli>|--all]                batch import of un-captured transcripts (was archive --all-unarchived + ingest)
  checkpoint --trigger <t>                            unchanged
  consume                                             unchanged
  search <q> [--ranked] [--entity-root]               unchanged + gains --entity-root
  what-do-you-know [--about]                          unchanged
  distill                                             unchanged (read-only suggestions)
  config …                                            unchanged
  serve / mcp                                         unchanged
  update …                                            unchanged
  uninstall [--purge] [--yes]                         new
  graph
    init | status | search | query | traverse | correct | decay-report | gc | daemon
    ingest [<archive>|--all]                          (merges ingest + ingest-all)
    extract [--all|--log N] …                         help text fixed
    add-entity | relate                               kept, marked "manual"
    export | import | rebuild                         (data-safety spec)
```

Hidden aliases for one major so 4.x hooks keep working until `init` rewrites them:
`archive-session` → `capture`, `archive --all-unarchived` → `capture --sweep --from claude-code`,
`ingest` → `capture --sweep`, `dashboard` → `status`, `graph ingest-all` → `graph ingest --all`.
Aliases are `#[command(hide = true)]` and print a one-line deprecation notice.

### 2. Private stack gated

`graph pipeline *`, `graph vigil-sync`, `graph feedback`, `src/graph/vigil_sync.rs`, the
pipeline modules, and `graph status`'s "Pipeline entities" line compile only under the
`pulse-null` feature. pulse-null builds with that feature and is unaffected. The README's
"As a pulse-null Plugin" section moves to `docs/pulse-null.md`.

### 3. Deletions and dedup

- Delete `dashboard::auto_distill` and its helpers. `distill` stays read-only.
- One `find_sections` and one threshold source (config `memory.max_lines`) shared by
  `distill` and `status`; the two suggestion engines become one.
- Remove `graph/query.rs:569,591` unwraps (entity deleted between list and detail → skip).
- `client_runtime()` defined once.

### 4. `server` feature off by default

`default = ["embedded", "llm", "self-update"]`. `server` stays available and documented as
experimental (no background capture/extraction in that mode — already true). Echo's build
enables it explicitly.

### 5. Atomic writes

`fsx::write_atomic(path, bytes)` (temp in same dir, 0600, `sync_all`, rename) extracted from
`init.rs:566` and used by: EPHEMERAL.md append/trim, archive write, ARCHIVE.md update,
checkpoint write, MEMORY.md writes, config save, registry writes.

### 6. `uninstall`

Removes, in order, reporting each: hook entries in `~/.claude/settings.json` whose binary
basename is `pulse-recall` or `recall-echo` in canonical shape (operator-carrying entries are
listed, not touched, with the exact line to delete); MCP registrations named `pulse-recall`
or `recall-echo` in every detected CLI; the runtime dir; `~/.config/pulse-recall/`.
`--purge` additionally deletes `memory/` under every registered root after printing the list
and asking; `--yes` is required when stdin is not a tty. Never deletes the binary; prints the
command to. `--dry-run` prints the plan.

### 7. `doctor`

Prints: version and binary path; resolved root and *which rule* resolved it; registry
entries with existence/mode checks; config path, parse status, effective provider and
transport; detected CLIs with versions; hook entries found (and whether they point at this
binary); MCP registrations per CLI; daemon socket/pid/version/uptime; embedding model cache
present and size; store size and episode/entity counts; disk free on the root's filesystem;
last 20 lines of `daemon.log`. Exit 1 if any check is red. `--json` for bug reports.

### 8. #45 residuals

- `archive_conversation` is idempotent on `session_id`: an existing archive with the same
  `session_id` in frontmatter is returned, not duplicated, and extraction is not re-billed.
- `agent_cli::shell_line` uses the single-quoting `shell_path`; one function, one module.
- Non-UTF-8 entity root is refused via `to_str()` with a clear error.

---

## Acceptance criteria

- AC1: `pulse-recall --help` lists exactly the commands in §1 (snapshot test on the rendered help, with hidden aliases absent).
- AC2: `pulse-recall archive-session < hook.json` works and prints one deprecation line; same for each alias in §1.
- AC3: Default build has no `graph pipeline`, `vigil-sync`, `feedback`; `--features pulse-null` has them (two `cargo test` invocations in CI).
- AC4: `cargo build` with default features does not compile the `server` backend (`cargo tree -e features` assertion in CI).
- AC5: Killing the process mid-write of EPHEMERAL.md leaves either the old or the new file, never a truncated one (test: inject failure between temp write and rename).
- AC6: `uninstall --dry-run` on a settings.json with one canonical and one `|| true` hook prints "remove" for the first and "leave, delete manually: …" for the second; without `--dry-run` the file reflects that; `.bak` written.
- AC7: `uninstall` with MCP registered in two mock clients issues `mcp remove` for both (argv-level test).
- AC8: `uninstall --purge` without a tty and without `--yes` refuses; with `--yes` deletes only registered roots' `memory/` and prints each path.
- AC9: `doctor` on a healthy install exits 0 with every section present; with a missing model cache exits 1 and the JSON output has `"model": {"ok": false}`.
- AC10: Capturing the same transcript twice produces one archive and one ARCHIVE.md line.
- AC11: `shell_line` of a root containing `"` and `$(x)` is a single-quoted, safe line (test extends RE-42's injection cases).
- AC12: No `auto_distill` symbol exists; `distill` and `status` agree on MEMORY.md health for the same file (property test over generated files).
- AC13: All existing tests pass, renamed where the surface changed.

## Out of scope

Exit-code taxonomy beyond the existing 0/1/10. Interactive TUI. Splitting `main()` into
modules (after launch).

## Verification

`cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test && cargo test --features pulse-null`.
Quality auditor re-run confirms one root resolver, no dead destructive API, coherent help.

## Delivery

Branch `chore/RE-<n>-front-door`. Ships in v5.0.0.
