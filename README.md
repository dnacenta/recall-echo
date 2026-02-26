# recall-echo

[![License: MIT](https://img.shields.io/github/license/dnacenta/recall-echo)](LICENSE)
[![Version](https://img.shields.io/github/v/tag/dnacenta/recall-echo?label=version&color=green)](https://github.com/dnacenta/recall-echo/tags)
[![Crates.io](https://img.shields.io/crates/v/recall-echo)](https://crates.io/crates/recall-echo)

Persistent three-layer memory system for AI coding agents. Gives Claude Code (and similar tools) long-term recall across sessions.

## The Problem

Claude Code's built-in memory (`MEMORY.md`) is a single flat file. It works for small notes, but breaks down as conversations accumulate — no session continuity, no archival history, no lifecycle management. You lose context every time a session ends or compaction kicks in.

## How It Works

recall-echo adds a structured memory protocol via Claude Code's auto-loaded rules system (`~/.claude/rules/`). No patches, no forks — just a rules file that teaches the agent how to manage its own memory.

```
                         ┌──────────────────────────┐
                         │      Claude Code agent    │
                         │    (reads / writes all    │
                         │     layers via protocol)  │
                         └────┬─────────┬───────┬───┘
                              │         │       │
                    read+write│   write │       │ search
                              │         │       │ on demand
                              ▼         ▼       ▼
┌─────────────────┐  ┌──────────────┐  ┌─────────────────────┐
│   Layer 1       │  │   Layer 2    │  │      Layer 3        │
│   MEMORY.md     │  │ EPHEMERAL.md │  │   Archive Logs      │
│                 │  │              │  │                     │
│ Curated facts,  │  │ Session      │  │ archive-log-001.md  │
│ preferences,    │  │ summary,     │  │ archive-log-002.md  │
│ stable patterns │  │ staging area │  │ ...                 │
│                 │  │              │  │                     │
│ Always in       │  │ Written at   │  │ YAML frontmatter    │
│ context         │  │ session end, │  │ + content body      │
│ (auto-loaded)   │  │ promoted     │  │                     │
└─────────────────┘  │ automatically│  │ Created by CLI:     │
                     └──────────────┘  │ recall-echo         │
                                       │   promote/checkpoint│
                                       │                     │
                                       │ Indexed in          │
                                       │ ARCHIVE.md          │
                                       └─────────────────────┘
```

### Session Lifecycle

```
 Session Start          During Session           On Compaction            Session End
 ─────────────          ──────────────           ──────────────           ───────────

 ┌─────────────┐        ┌─────────────┐         ┌─────────────┐         ┌──────────────┐
 │ PreToolUse  │        │ Update      │         │ PreCompact  │         │ Write        │
 │ hook fires  │───────▶│ MEMORY.md   │────────▶│ hook fires  │────────▶│ EPHEMERAL.md │
 │             │        │ with stable │         │             │         │ with session │
 │ recall-echo │        │ facts       │         └──────┬──────┘         │ summary      │
 │ consume     │        └─────────────┘                │                └──────┬───────┘
 │             │                                       ▼                       │
 │ Reads       │                              ┌──────────────┐                 ▼
 │ EPHEMERAL → │                              │ recall-echo  │         ┌──────────────┐
 │ stdout,     │                              │ checkpoint   │         │ SessionEnd   │
 │ clears file │                              │ --trigger    │         │ hook fires   │
 └─────────────┘                              │ precompact   │         │              │
                                              └──────┬───────┘         │ recall-echo  │
                                                     │                 │ promote      │
                                                     ▼                 │              │
                                              ┌──────────────┐         │ Archives     │
                                              │ CLI creates  │         │ EPHEMERAL →  │
                                              │ scaffolded   │         │ archive log  │
                                              │ archive log  │         │ + clears it  │
                                              │ + updates    │         └──────────────┘
                                              │ ARCHIVE.md   │
                                              └──────┬───────┘
                                                     │
                                                     ▼
                                              ┌──────────────┐
                                              │ Agent fills  │
                                              │ in Summary,  │
                                              │ Key Details, │
                                              │ Action Items,│
                                              │ Unresolved   │
                                              └──────────────┘
```

The entire lifecycle is now mechanical — no voluntary steps. The PreToolUse hook consumes last session's context at start, the PreCompact hook checkpoints before compaction, and the SessionEnd hook archives on exit. The agent only writes one summary to EPHEMERAL.md at session end.

## Installation

### cargo install (recommended)

```bash
cargo install recall-echo
recall-echo init
```

### Install script

Downloads a prebuilt binary for your platform. Falls back to a bash-only installer if no binary is available.

```bash
curl -fsSL https://raw.githubusercontent.com/dnacenta/recall-echo/main/install.sh | bash
```

### Prebuilt binaries

Download from [GitHub Releases](https://github.com/dnacenta/recall-echo/releases/latest) for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin` (Apple Silicon)

Extract and run:

```bash
tar xzf recall-echo-<target>.tar.gz
./recall-echo init
```

### Manual (from source)

```bash
git clone https://github.com/dnacenta/recall-echo.git
cd recall-echo
cargo build --release
./target/release/recall-echo init
```

## Commands

### `recall-echo init`

Initialize or upgrade the memory system. Creates directories, writes protocol rules, and configures all three hooks (PreToolUse for ephemeral consumption, PreCompact for checkpointing, SessionEnd for automatic promotion). Idempotent — running it again won't overwrite existing memory files. Upgrades legacy echo hooks to the new checkpoint hook.

### `recall-echo checkpoint`

Create a precompact archive checkpoint. Scaffolds a new archive log with YAML frontmatter and section templates, updates the ARCHIVE.md index, and prints the file path for the agent to fill in.

```bash
recall-echo checkpoint                              # default: precompact trigger
recall-echo checkpoint --context "working on auth system"
```

```
  recall-echo checkpoint
        │
        ├──▶ Scan ~/.claude/memories/ for highest archive-log-XXX.md
        │
        ├──▶ Create archive-log-{next}.md with YAML frontmatter
        │    + empty Summary / Key Details / Action Items / Unresolved
        │
        ├──▶ Append entry to ARCHIVE.md index
        │
        └──▶ Print path + instructions for the agent
             "RECALL-ECHO checkpoint: ~/.claude/memories/archive-log-005.md"
```

### `recall-echo promote`

Promote EPHEMERAL.md into an archive log. Reads EPHEMERAL.md content, creates a new archive log with the content as the body, updates the ARCHIVE.md index, and clears EPHEMERAL.md. If EPHEMERAL.md is empty or missing, exits cleanly with no action.

```bash
recall-echo promote                                 # auto-extract context from content
recall-echo promote --context "session about auth"   # override context field
```

```
  recall-echo promote
        │
        ├──▶ Read EPHEMERAL.md (exit if empty)
        │
        ├──▶ Scan ~/.claude/memories/ for next log number
        │
        ├──▶ Create archive-log-{next}.md with YAML frontmatter
        │    + EPHEMERAL.md content as the body
        │
        ├──▶ Append entry to ARCHIVE.md index
        │
        ├──▶ Clear EPHEMERAL.md
        │
        └──▶ Print confirmation
             "Promoted EPHEMERAL.md → Log 005 | Date: 2026-02-24"
```

### `recall-echo consume`

Consume EPHEMERAL.md at session start. Reads the file contents, outputs them to stdout wrapped in memory markers (so Claude Code captures it via hook), then clears the file. Silent if empty or missing. Idempotent — safe to call multiple times; only the first call with content produces output.

```
  recall-echo consume
        │
        ├──▶ Read EPHEMERAL.md (exit silently if empty/missing)
        │
        ├──▶ Output content wrapped in memory markers to stdout
        │    [MEMORY — Last Session Summary ...]
        │    <content>
        │    [END MEMORY — EPHEMERAL.md has been cleared ...]
        │
        └──▶ Clear EPHEMERAL.md
```

### `recall-echo status`

Memory system health check. Shows MEMORY.md line count, EPHEMERAL.md state, archive log count, protocol status, and hook configuration. Warns about approaching limits or legacy hooks.

```
recall-echo — memory system status

  MEMORY.md:    142/200 lines (71%)
  EPHEMERAL.md: has content (pending consumption)
  Archive logs: 23 logs, latest: 2026-02-24
  Protocol:     installed
  Hooks:        PreToolUse ✓  PreCompact ✓  SessionEnd ✓

  No issues detected.
```

## What It Creates

```
~/.claude/
│
├── rules/
│   └── recall-echo.md ·········· Protocol rules (auto-loaded into every session)
│
├── memory/
│   └── MEMORY.md ················ Layer 1 — curated facts (≤200 lines)
│
├── memories/
│   ├── archive-log-001.md ······· Layer 3 — checkpoint (YAML frontmatter)
│   ├── archive-log-002.md
│   └── ...
│
├── EPHEMERAL.md ·················· Layer 2 — session summary staging area
├── ARCHIVE.md ···················· Index — log number, date, trigger per entry
└── settings.json ················· Hooks: PreToolUse + PreCompact + SessionEnd
```

## Archive Log Format

Logs created by `recall-echo checkpoint` and `recall-echo promote` include YAML frontmatter for structured metadata:

```yaml
---
log: 5
date: "2026-02-24T21:30:00Z"
trigger: precompact
context: "working on auth system"
topics: []
---

# Archive Log 005

## Summary
<!-- Fill in: What was discussed, decided, and accomplished -->

## Key Details
<!-- Fill in: Important specifics — code changes, configurations, decisions -->

## Action Items
<!-- Fill in: What needs to happen next -->

## Unresolved
<!-- Fill in: Open questions or threads to pick up later -->
```

Old logs without frontmatter continue to work — numbering is by filename, not content.

## Configuration

recall-echo requires no configuration. It works out of the box with Claude Code's existing infrastructure.

It adds three hooks to `settings.json`:
- **PreToolUse** — runs `recall-echo consume` on first tool use, injecting last session's context and clearing EPHEMERAL.md
- **PreCompact** — runs `recall-echo checkpoint` before context compaction, creating a scaffolded archive log
- **SessionEnd** — runs `recall-echo promote` on exit, archiving EPHEMERAL.md automatically

If you already have hooks configured, they're preserved. If upgrading from v0.2.0, the legacy echo hook is automatically replaced.

## How the Agent Uses It

Once installed, the entire memory lifecycle is mechanical — managed by hooks, not voluntary behavior:

- **Session start** — PreToolUse hook runs `recall-echo consume`, injecting last session's context automatically
- **During session** — agent updates `MEMORY.md` with stable facts and searches archives with `Grep` as needed
- **On compaction** — PreCompact hook runs `recall-echo checkpoint`, creating a scaffolded archive log
- **Session end** — agent writes `EPHEMERAL.md`, then SessionEnd hook runs `recall-echo promote` to archive it

You can also explicitly tell the agent to remember something, search its history, or review what it knows. The memory is transparent — it's all plain markdown files you can read and edit yourself.

## Upgrading from v0.2.0

Run `recall-echo init` — it will detect the legacy echo hook and migrate it to the new checkpoint hook. Your existing memory files, archive logs, and settings are preserved.

## Uninstall

Remove the rules file and optionally delete the memory data:

```bash
# Remove the protocol (agent stops following it)
rm ~/.claude/rules/recall-echo.md

# Optionally remove all memory data
rm -rf ~/.claude/memory ~/.claude/memories ~/.claude/EPHEMERAL.md ~/.claude/ARCHIVE.md
```

You may also want to remove the `PreToolUse`, `PreCompact`, and `SessionEnd` hooks from `~/.claude/settings.json`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[MIT](LICENSE)
