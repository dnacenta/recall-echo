# Spec — Gruvbox Dark palette for all CLI output

**Status:** draft (awaiting D's approval)
**Target version:** 4.3.0 → 4.4.0 (visible output change, no CLI surface change)
**Drafted:** 2026-09-15
**Baseline:** `main` @ `26e4407` (v4.3.0)

---

## Goal

Every colored line recall-echo prints — dashboard, status, init, search, distill, inspect,
config, graph — renders in Gruvbox Dark, from one theme module, with correct behaviour when
color is unwanted (piped, `NO_COLOR`, dumb terminal).

## Why this matters

recall-echo has no TUI framework. Its "TUI" is seven copies of the same six ANSI constants
(`GREEN`, `YELLOW`, `RED`, `CYAN`, `DIM`, `BOLD`, `RESET`) pasted into eight source files,
plus three inline `\x1b[31m` literals. Those are the terminal's *basic* 16-color slots, so
the output looks like whatever the user's terminal theme maps red/green/cyan to — never a
deliberate palette. Color is also emitted unconditionally: `recall-echo status | less` and
every hook log line carry escape codes today.

pulse-null's TUI (PN-102) already commits to Gruvbox Dark as its native palette and
falls back to it for everything. Its ears-and-memory sibling should look like the same
family on the same screen.

---

## Design

### Palette (foreground only)

recall-echo is line-oriented output inside the user's shell, so it paints **foreground
spans only**. It never sets a background and never repaints default text; the terminal
owns `bg`/`fg`. Six semantic tokens replace the seven raw constants:

| Token    | Role today (replaces)                                   | Gruvbox Dark | 256-color |
| -------- | ------------------------------------------------------- | ------------ | --------- |
| `GOOD`   | `GREEN` — ✓ marks, HEALTHY, running/on, extracted →     | `#b8bb26`    | 142       |
| `WARN`   | `YELLOW` — ! marks, WATCH, ~ exists, not running/off    | `#fabd2f`    | 214       |
| `BAD`    | `RED` — ✗ marks, ALERT, errors, decay ↓                 | `#fb4934`    | 167       |
| `ACCENT` | `CYAN` — filenames, entity names, relation edges, logo  | `#8ec07c`    | 108       |
| `DIM`    | `DIM` (faint attr) — metadata, scores, hints, separators| `#928374`    | 245       |
| `BOLD`   | `BOLD` — section headers, emphasised names              | bold attr    | bold attr |

`RESET` stays. `DIM` becomes an explicit gray instead of the SGR "faint" attribute, whose
rendering varies by terminal. The hex values and 256-color indices are Gruvbox's own
canonical mapping (the same numbers the upstream vim theme ships), and the hex values are
byte-identical to pulse-null's `GRUVBOX_DARK` tokens.

### Color mode resolution

Resolved **once per process** from environment + stream, by a pure function that is unit
tested with injected inputs:

```
resolve(env, stdout_is_tty) -> Mode
  CLICOLOR_FORCE set and not "0"        → Truecolor if COLORTERM∈{truecolor,24bit} else Ansi256
  NO_COLOR set (any value, incl. "")    → Plain
  TERM == "dumb" or TERM unset          → Plain
  !stdout_is_tty                        → Plain
  COLORTERM ∈ {truecolor, 24bit}        → Truecolor   (\x1b[38;2;r;g;bm)
  otherwise                             → Ansi256     (\x1b[38;5;Nm)
```

`Plain` emits empty strings for every token including `BOLD` and `RESET`, so the text is
byte-clean. The mode is keyed on **stdout** only: the 29 stderr call sites (init prompts,
error line in `main`) are all interactive flows where stdout is the same terminal, and one
global decision keeps the module trivial. No `--color` flag in this spec; the front-door
work (RE-54) owns CLI surface and can add one on top of the same `Mode` later.

### Module

New `src/theme.rs`:

```rust
pub enum Mode { Plain, Ansi256, Truecolor }
pub fn resolve(env: impl Fn(&str) -> Option<String>, stdout_is_tty: bool) -> Mode
pub fn mode() -> Mode            // OnceLock, initialised from the real env on first use

pub struct Paint(Token);          // impl Display: writes the escape for mode(), or ""
pub static GOOD: Paint;  pub static WARN: Paint;  pub static BAD: Paint;
pub static ACCENT: Paint; pub static DIM: Paint; pub static BOLD: Paint; pub static RESET: Paint;
```

`static` + `Display` is the point: Rust's inline format args capture statics, so the
existing `format!("{GREEN}HEALTHY{RESET}")` sites become `format!("{GOOD}HEALTHY{RESET}")`
with a mechanical rename and one `use crate::theme::{…}` per file. No call site is
restructured. `Display` reads the `OnceLock` at format time (one atomic load), so cost is
nil and no explicit init call is needed in `main`.

### Migration

1. Delete the per-file `const` blocks in `dashboard`, `init`, `status`, `distill`,
   `inspect_cli`, `graph_cli`, `search`, `config_cli`.
2. Rename usages: `GREEN→GOOD`, `YELLOW→WARN`, `RED→BAD`, `CYAN→ACCENT`; `DIM`, `BOLD`,
   `RESET` keep their names.
3. Fold the three inline literals (`graph_cli.rs:1331`, `graph_cli.rs:1782`, `main.rs:820`)
   into `BAD`.
4. Paint the dashboard `SEPARATOR` and the section rules `DIM`.
5. A `scripts/`-free grep gate in tests: no `\x1b[` literal outside `src/theme.rs`.

Out of scope: any layout, wording, or structural change to what is printed; background
painting; per-stream (stderr) mode; a `--color` flag; theme selection or config — Gruvbox
Dark is the palette, not an option.

### Sequencing

RE-52 (rename to pulse-recall, 83 files) is in flight in a worktree and touches every file
this spec touches. This work is a mechanical rename, so it goes second: branch off `main`
now, land **after** RE-52 merges, rebase once. Conflicts, if any, are resolved by re-running
the rename.

---

## Acceptance Criteria

### Happy
- AC1: `src/theme.rs` is the only file in `src/` containing the byte sequence `\x1b[`; a test enforces it.
- AC2: With `COLORTERM=truecolor` on a tty, every colored span uses a 24-bit escape whose RGB is one of the six Gruvbox values above; no `\x1b[3Xm` basic-color escapes remain.
- AC3: With `COLORTERM` unset on a tty, every colored span uses `\x1b[38;5;Nm` with N ∈ {142, 214, 167, 108, 245}.
- AC4: `recall-echo status`, `dashboard`, `search`, `graph status`, `config show`, `inspect`, `distill --dry-run`, `init` (non-interactive) print byte-for-byte the same text as before once escapes are stripped — no wording or layout drift.
- AC5: `DIM` spans render in gray `#928374` / 245, not the SGR faint attribute.

### Edge
- AC6: `NO_COLOR=` (empty) and `NO_COLOR=1` both yield plain output on a tty.
- AC7: `TERM=dumb` yields plain output regardless of `COLORTERM`.
- AC8: `CLICOLOR_FORCE=1` yields colored output when stdout is a pipe.
- AC9: `resolve()` is pure and covered by unit tests for each row of the resolution table, without touching process env or a real tty.

### Failure
- AC10: Any command with stdout piped (`| cat`) emits zero escape bytes on stdout and stderr, unless `CLICOLOR_FORCE` is set.
- AC11: Existing test suite passes unchanged in count (tests run non-tty → `Plain`, so no test may depend on escapes being present).
