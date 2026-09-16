# Spec — Flagless root resolution: every command finds the store `init` pinned

**Status:** draft
**Target version:** 4.4.0 → 4.4.1 (bug fix, no surface change)
**Drafted:** 2026-09-16
**Baseline:** `main` @ `0a760e4` (v4.4.0)
**Issue:** #63
**Relation to #51 / #53:** stop-gap. #53's root registry replaces `entity_root()` wholesale;
this spec only makes the existing persisted root visible to the commands that ignore it.

---

## Goal

`recall-echo`, `recall-echo status`, `search`, `distill`, `dashboard`, `config`, `graph *`,
`serve`, `mcp` and `what-do-you-know`, invoked with no `--entity-root`/positional root and no
`RECALL_ECHO_HOME`, must resolve to the same store the hooks resolve to: the root that
`recall-echo init` persisted. Today they resolve to the literal cwd, so from any directory
other than the entity root itself they report `memory/ directory not found. Run
\`recall-echo init\` first.` — immediately after `init` said `memory initialised at
/root/.claude/memory`.

## Why this matters

Verified on the VPS 2026-09-16 (4.3.0 and 4.4.0): `init` from `/root` chooses `~/.claude`
(`resolve_init_root` prefers a detected Claude Code dir), persists it to
`~/.config/recall-echo/entity-root`, and registers hooks and MCP with that root baked in.
The bare command then fails from the same shell. The launch audit (2026-09-02) already
listed this as the source of "memory vanished" tickets; #51 reproduces it from a
subdirectory.

The split is a single function. `paths::entity_root()` is `RECALL_ECHO_HOME` → cwd, and it is
what `resolve_entity_root(None)` in `main.rs`, `status::run`, `distill::run`,
`search::run*` (via `paths::memory_dir()`) and `RecallEcho::from_default` all call. The
capture hooks instead use `paths::hook_entity_root(None)`: explicit → `RECALL_ECHO_HOME` →
cwd *when it looks initialised* → persisted root. The fix is to make `entity_root()` use the
same chain.

---

## Design

### Resolution order for `paths::entity_root()`

```
 RECALL_ECHO_HOME set (non-empty)          ──▶ use it, as today (no existence check)
        │ unset
        ▼
 cwd looks initialised                     ──▶ cwd   (pulse-null entities: cwd = entity home)
   (memory/conversations or conversations)
        │ no
        ▼
 persisted root exists AND looks initialised ──▶ persisted root
        │ absent / not initialised
        ▼
 cwd                                        ──▶ legacy: existing "run init first" errors
                                                keep naming the directory the user is in
```

Implementation: `entity_root()` becomes `hook_entity_root(None)` filtered by
`looks_initialized`, falling back to cwd. `hook_entity_root` is unchanged for the hooks
except that the persisted-root arm also gains the `looks_initialized` filter (see below);
`resolved_hook_base_dir`'s loud `~/.claude` fallback is untouched.

### Stale persisted root

A persisted root that no longer looks initialised (deleted directory, or a `/tmp` path left by
the #59 test pollution) is **ignored, with one stderr line**:

```
recall-echo: persisted entity root <path> is not initialised — ignoring it; re-run `recall-echo init`
```

Without this, honouring the persisted root everywhere would turn #59 from "hooks broken" into
"every command silently reads a temp dir". The warning goes to stderr only when the fallback
is actually taken, so scripted output on stdout is unaffected. `hook_entity_root` applies the
same filter so hooks and commands agree on what "pinned" means.

### `--help` text

The positional `entity_root` help on `status`, `distill`, `consume`, `dashboard`, `config`,
`mcp`, `what-do-you-know`, `graph` says "defaults to current directory". Change to
"defaults to the initialised cwd, else the root `init` persisted". `search` gains no flag
(out of scope; it goes through `memory_dir()` and is fixed by the same change).

### Out of scope

- Directory walk-up (#51) and the registry (#53). No ancestor search is added.
- A claude-style root without `memory/` (`~/.claude/conversations` only): `status` still
  requires `memory/`; unchanged, same as with an explicit root today.
- `init`'s own root choice (`resolve_init_root`) and the #59 persist guard.

---

## Acceptance criteria

Happy path
- AC1: After `recall-echo init` from `/root` (root persisted = `/root/.claude`), bare
  `recall-echo` and `recall-echo status` from `/root` print the healthy status, not
  `memory/ directory not found`.
- AC2: From `/root`, `recall-echo search <term>`, `distill`, `dashboard`, `config show`,
  `graph status` and `what-do-you-know` all operate on `/root/.claude/memory`.
- AC3: With cwd = an initialised entity root that is *not* the persisted one, the cwd wins
  (pulse-null entities unchanged).
- AC4: `RECALL_ECHO_HOME` set wins over both cwd and the persisted root, as today.

Edge
- AC5: Persisted root points at a directory that does not look initialised → it is ignored,
  one stderr warning naming the path, and resolution falls back to cwd. The resulting
  `NotInitialized` error names cwd, as before.
- AC6: No persisted file, cwd not initialised → cwd, identical error text to 4.4.0.
- AC7: `consume`, `archive-session` and `checkpoint` (the hook path) resolve to the same root
  as `status` in every case above — one chain, tested through both entry points.

Failure
- AC8: Persisted file unreadable or empty → treated as absent (existing behaviour of
  `persisted_entity_root`), no panic.

Tests
- Unit tests in `paths.rs` drive `entity_root_from(env, cwd, persisted)` (a pure inner
  function; the public `entity_root()` wires the real env/cwd/file) through AC3–AC6/AC8.
  `XDG_CONFIG_HOME` is never touched by the tests: the inner function takes the persisted
  path as a parameter, so the suite cannot rewrite the developer's real file.
- One test asserts `hook_entity_root` and `entity_root` return the same root for the same
  inputs (AC7).
