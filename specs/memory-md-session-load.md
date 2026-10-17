# Spec — Layer 1 reaches Claude Code's context: MEMORY.md loads every session

**Status:** drafted (RE-75)
**Target version:** 4.6.2 → 4.7.0 (`init` writes one more file and may change one Claude Code setting)
**Drafted:** 2026-10-17
**Baseline:** `main` @ `86c7590` (v4.6.2 + docs)
**Issue:** #75

---

## Goal

After `recall-echo init` in a Claude Code install, every session (startup, resume and `/clear`)
has the whole of `<root>/memory/MEMORY.md` and the current `<root>/memory/EPHEMERAL.md` in
context, once each. No layer depends on a hand-written rules file. Pulse-null pulses get
exactly what they get today.

## Why this matters

The docs (`docs/internals.md` layer table: MEMORY.md "always in context") and
`templates/recall-echo.md` ("Auto-loaded at session start") promise something nothing does:

- `consume`, the only SessionStart hook `init` installs, prints EPHEMERAL.md and nothing else
  (`src/consume.rs`, `src/main.rs` `Commands::Consume`).
- `init` never installs the rules template. Nothing references `templates/recall-echo.md`,
  neither `init` nor `install.sh`.
- Claude Code's own auto-memory reads `~/.claude/projects/<sanitized-cwd>/memory/`, not
  `<root>/memory/`.

So on a fresh install the curated layer is a file the model never sees, and `status`,
`distill` and `dashboard` manage it for nobody. With Claude Code's auto-memory on (the
default), the user also has a second curated store, and only that one is live.

The template is stale against the 4.x layout as well. It imports `~/.claude/EPHEMERAL.md`,
points archive search at `~/.claude/conversations/`, and tells the model to fill in
checkpoint files by hand. 4.x writes `<root>/memory/EPHEMERAL.md` and
`<root>/memory/conversations/`, and `checkpoint` writes the whole conversation file itself
(`src/checkpoint.rs`).

### Why not have `consume` print MEMORY.md

Verified on Claude Code 2.1.302 (2026-10-17) with headless probes: SessionStart
command-hook stdout is inlined only up to about 10,000 characters. A 9,833-char output
arrived whole. Outputs of 10,193, 14,009 and 19,977 chars arrived as a head preview plus
`Output too large (NKB). Full output saved to: <path>`. A real MEMORY.md is already past
that: 18.6 KB / 177 lines on the VPS, and the 200-line convention allows roughly 25 KB.

It would also regress pulses. Pulse-null writes its own `consume <pulse root>` SessionStart
hook into each pulse's `.claude/settings.json` (`src/cli_provider/adapters/claude_hooks.rs`
`render_hooks`), and its prompt assembler already puts MEMORY.md into the system prompt
under its byte and token caps (`src/server/prompt.rs`, Tier 1 block). It also deliberately
leaves MEMORY.md out of scheduled-task and intent prompts (`build_task_system_prompt`,
`scheduler/runner.rs` "no MEMORY.md"). If `consume` printed MEMORY.md, chat turns would
carry it twice, the second copy uncapped, and task runs would carry it against design.

### Why not point `autoMemoryDirectory` at `<root>/memory`

Claude Code would then load MEMORY.md natively. But its auto-memory also writes its own
topic files into that directory and runs its own scanning and consolidation over it,
and that directory holds EPHEMERAL.md, ARCHIVE.md, `conversations/` and `graph/`.
That behaviour is unverified, and it would tie recall-echo to an internal setting.

### What does work

Claude Code memory files (`~/.claude/rules/*.md`) are not under the hook cap. They are
loaded in every project and in `-p` mode, and their `@path` imports are inlined whole.
This was verified 2026-10-17: a user-level rules file importing an 18.6 KB MEMORY.md put
its last section (the "blue parrot" probe line) into a headless session's context.

---

## Design

### Layer 1: a managed rules file with one import

`init` writes `<claude_dir>/rules/recall-echo.md`, rendered from `templates/recall-echo.md`
(embedded with `include_str!`). Its first line is the managed marker:

```
<!-- recall-echo:managed — rewritten by `recall-echo init`; delete this line to keep your own edits -->
```

The body is the corrected memory protocol, and it holds exactly one import:
`@<canonical root>/memory/MEMORY.md`.

| State of `<claude_dir>/rules/recall-echo.md` | `init` does |
|---|---|
| absent | write it (creating `rules/`), report Created |
| present, carries the marker, same content | nothing, report Exists |
| present, carries the marker, different content (root moved, template changed) | rewrite atomically, report Created |
| present, no marker (hand-written) | leave it byte-identical, report Exists and print the import line to add |

Writes use the same no-truncation-window approach as `settings.json`: a temp file in the
same directory, then a rename. The rules file is skipped wherever hooks are skipped
(build-directory binary, no Claude Code). It is written only into `ConfigRoots`'
`claude_dir`, so sandboxed tests never reach `~/.claude` (#59).

The root must render to an import Claude Code can read. A canonical root containing
whitespace or control characters is refused: no file, one error line, everything else in
`init` unchanged.

### Layer 2: EPHEMERAL stays on the hook, now including `/clear`

EPHEMERAL.md is small (5 entries ≈ 1.8 KB on the VPS), well under the inline cap. So it
keeps arriving through `consume` and is not imported, because importing it would put it in
context twice on every startup. The SessionStart matcher `init` writes becomes
`startup|resume|clear`. The existing matcher sync in `upsert_hook` upgrades a group that
holds only recall-echo's hook. A group shared with other hooks keeps its matcher, as today.

`compact` stays excluded: the conversation continues, and memory files are re-read anyway.

### Pulses: byte-for-byte unchanged

- `consume` output is unchanged (EPHEMERAL only).
- `init` writes no rules file when the root holds `pulse-null.toml`. Pulse-null renders
  each pulse's own `.claude/rules/recall-echo.md` and loads MEMORY.md itself.
- Pulse hooks are written by pulse-null, not by `recall-echo init`, so the matcher change
  never reaches them.

### Claude Code's built-in auto-memory

`init` checks whether Claude Code's auto-memory is on: `autoMemoryEnabled` is not `false`
in `<claude_dir>/settings.json`, and `CLAUDE_CODE_DISABLE_AUTO_MEMORY` is not set to a true
value.

- **Interactive** (stderr is a terminal): ask once — `Claude Code's built-in auto-memory is
  on: a second curated store that recall-echo does not read. Turn it off? [Y/n]`. Yes →
  `autoMemoryEnabled: false` written with every other key preserved (same reader, refusal
  rules and atomic writer as the hooks). No → nothing changes.
- **Non-interactive**: never changes the setting; prints one notice line.
- **Already off**: no question, no notice.
- Pulse roots (`pulse-null.toml`): skipped.

The notice and the question both say that existing notes under
`~/.claude/projects/*/memory/` are not moved.

### Template rewrite (4.x layout)

`templates/recall-echo.md` becomes the source of the managed file:

- Layer 0: the graph, read through the MCP tools (`recall_query` first) or `recall-echo graph`.
- Layer 1: the `{{MEMORY_IMPORT}}` placeholder, the 200-line convention, topic files beside
  MEMORY.md, update rather than duplicate.
- Layer 2: EPHEMERAL arrives via the SessionStart hook; never edit it by hand.
- Layer 3: `<root>/memory/conversations/`, `<root>/memory/ARCHIVE.md`, `recall-echo search --ranked`.
- Lifecycle: the three hooks do the plumbing; no manual checkpoint or archive steps.

Paths in the body are rendered from the root, not hard-coded to `~/.claude`.

### Docs

- `docs/internals.md`: the layer table says how layers 1 and 2 reach Claude Code (rules
  import / SessionStart hook including `/clear`); the MCP section's "SessionStart prints
  EPHEMERAL.md" line stays accurate.
- `docs/commands.md`: `init` lists the rules file and the auto-memory question; `consume`
  notes the `/clear` matcher.
- `README.md`: the `init` sentence mentions the rules file.
- `CHANGELOG.md`: a 4.7.0 entry with the upgrade note (re-run `init` to get the rules file
  and the new matcher; a hand-written `~/.claude/rules/recall-echo.md` is left alone).

### Out of scope

- Moving existing Claude Code auto-memory notes into `<root>/memory/` (a separate import command).
- Layer 1 loading for Codex, Grok and Gemini (no verified memory-file channel; hooks are Claude Code only).
- Pulse-null's own stale rules text and its EPHEMERAL double injection (a pulse-null follow-up).
- Enforcing an EPHEMERAL size under the 10,000-char inline cap (documented only).
- A `status` warning about auto-memory.

---

## Acceptance criteria

### Happy path

- AC1: Given a Claude Code install with no rules file, when `recall-echo init <root>` runs,
  then `<claude_dir>/rules/recall-echo.md` exists, its first line is the managed marker, and
  it contains exactly one import, `@<canonical root>/memory/MEMORY.md`.
- AC2: Given that file and a MEMORY.md over 10,000 chars, when a headless Claude Code session
  starts, then the last line of MEMORY.md is in context. This is checked with a headless
  probe that plants a marker at the tail.
- AC3: Given `init` ran, then the SessionStart group holding `consume` has matcher
  `startup|resume|clear`. Given an existing install whose recall-echo-only group says
  `startup|resume`, re-running `init` upgrades it.
- AC4: The rendered rules file names only 4.x paths under the root (`memory/EPHEMERAL.md`,
  `memory/conversations/`, `memory/ARCHIVE.md`), points graph reads at the MCP tools, has no
  manual checkpoint instructions, and contains neither `~/.claude/EPHEMERAL.md` nor
  `~/.claude/conversations`.
- AC5: Given auto-memory on and an interactive `init`, when the user answers yes, then
  `<claude_dir>/settings.json` has `autoMemoryEnabled: false` and every other key unchanged.
  When the user answers no, the file is byte-identical.

### Edge cases

- AC6: Re-running `init` with the same root and template leaves the managed file untouched
  and reports Exists. With a different root, it rewrites the import.
- AC7: Given a rules file at that path without the marker, `init` leaves it byte-identical
  and prints the import line to add.
- AC8: Given a root holding `pulse-null.toml`, `init` writes no rules file and does not touch
  auto-memory. For any root, `consume` output is byte-identical to 4.6.2.
- AC9: A non-interactive `init` never changes `autoMemoryEnabled` and prints one notice when
  auto-memory is on. With `autoMemoryEnabled: false` or `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`
  there is no question and no notice.
- AC10: From a build-directory binary or with sandboxed `ConfigRoots`, nothing is written
  outside the sandbox: not the rules file, not the settings flag.

### Failure modes

- AC11: A canonical root containing whitespace or control characters leaves no rules file
  and prints one error line naming why. Hooks and MCP are handled exactly as before.
- AC12: An unreadable or invalid `settings.json` means the auto-memory step is refused with
  the hooks' message style, and the file is never overwritten.
- AC13: If the rules directory cannot be created or written, `init` prints an error line and
  carries on. `init` never fails because of an integration.

### Docs

- AC14: `docs/internals.md`, `docs/commands.md`, `README.md` and `CHANGELOG.md` describe how
  layers 1 and 2 reach Claude Code, and none of them claims that a hook loads MEMORY.md.
