# Spec — Flagless root resolution: every command finds the store `init` pinned

**Status:** implemented (RE-63, PR pending)
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
 persisted root is trusted                 ──▶ persisted root (canonical)
   (absolute, exists, a directory this user
    owns, not writable by other users)
        │ absent / not trusted
        ▼
 cwd                                        ──▶ legacy: existing "run init first" errors
                                                keep naming the directory the user is in
```

Implementation: one pure resolver (`resolve_root_source`) returns which arm won; `entity_root()`
maps that to a path with the cwd fallback, `hook_entity_root()` maps it to `Option` with no
fallback. `resolved_hook_base_dir`'s loud `~/.claude` fallback is untouched.

The persisted arm is gated on **trust, not layout** (audit finding, 2026-09-16): the pointer
file is plain text that `init` writes unconditionally and the test suite has overwritten with
`/tmp` paths (#59), so "a `conversations/` dir exists there" proves nothing — anyone can create
one. A trusted root is absolute, resolves, is a directory owned by the current uid, and is not
group- or world-writable; the canonical path is what gets used. A trusted root whose `memory/`
was deleted behaves exactly as 4.4.0: commands say "run init first", hooks recreate the layout.

### Stale persisted root

A persisted root that is not trusted is **ignored, with one stderr line per process**, on both
the command and the hook path:

```
recall-echo: ignoring persisted entity root "<path>": it <reason>; re-run `recall-echo init`
```

The path is printed escaped (`{:?}`) because it comes from a file. Without this, honouring the
persisted root everywhere would turn #59 from "hooks broken" into "every command silently reads
a temp dir". The warning goes to stderr only when the fallback is actually taken.

### Provenance

`recall-echo status` (flagless) prints one dim stderr line naming the resolved root and the
arm that chose it (`root /root/.claude — persisted by \`recall-echo init\``), so a store that is
not the directory the user stands in is visible on screen.

### Persisted file permissions

`init` writes `~/.config/recall-echo/` as 0700 and `entity-root` as 0600. The file steers
every flagless command; nobody else gets to read where the store is.

### `--help` text

The positional `entity_root` help on `status`, `distill`, `consume`, `dashboard`, `config`,
`mcp`, `what-do-you-know`, `graph` says "defaults to current directory". Change to
"defaults to the initialised cwd, else the root `init` persisted". `search` gains no flag
(out of scope; it goes through `memory_dir()` and is fixed by the same change).

### Out of scope

- Directory walk-up (#51) and the registry (#53). No ancestor search is added.
- A claude-style root without `memory/` (`~/.claude/conversations` only): `status` still
  requires `memory/`; unchanged, same as with an explicit root today.
- `init`'s own root choice (`resolve_init_root`: env → `~/.claude` → cwd, no initialised-cwd
  check) and the #59 persist guard. `init` is a third chain; noted on #53 as a follow-up.
- `RECALL_ECHO_HOME` existence/ownership checks (explicit is consent, as today; #53 §1).

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
- AC5: Persisted root is not trusted (missing, relative, not a directory, owned by another
  uid, or writable by other users) → it is ignored, one stderr warning naming the path and the
  reason, and resolution falls back to cwd. The resulting `NotInitialized` error is the 4.4.0
  one. Hooks print the same warning before their `~/.claude` fallback notice.
- AC6: No persisted file, cwd not initialised → cwd, identical error text to 4.4.0.
- AC7: `consume`, `archive-session` and `checkpoint` (the hook path) resolve to the same root
  as `status` in every case above — one chain, one resolver.
- AC9: Flagless `status` names the resolved root and which arm chose it.
- AC10: `init` writes the persisted file 0600 in a 0700 directory.

Failure
- AC8: Persisted file unreadable or empty → treated as absent (existing behaviour of
  `persisted_entity_root`), no panic.

Tests
- Unit tests in `paths.rs` drive `resolve_root_source(env, cwd, persisted, initialised,
  trusted)` — pure, every input a parameter, the persisted root a lazy closure — through
  AC3–AC6/AC8, plus `trusted_root` against real temp dirs and the persisted-file modes (AC10).
- One test asserts the command and hook mappings agree on the root whenever the hook side is
  pinned (AC7).
- `tests/flagless_root.rs` runs the real binary from a directory that is not the root, with
  `XDG_CONFIG_HOME` and `HOME` pointed at a temp dir: bare/`status`/`config show` hit the
  persisted store (AC1, AC2, AC9); a missing and a world-writable persisted root are named and
  refused (AC5); `RECALL_ECHO_HOME` wins (AC4). The developer's real persisted file and
  `~/.claude` are never read or written by the suite.
