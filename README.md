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
│ (auto-loaded)   │  │ promoted at  │  │                     │
└─────────────────┘  │ next start   │  │ Created by CLI:     │
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
 │ recall-echo │        │ Update      │         │ PreCompact  │         │ Write        │
 │ promote     │───────▶│ MEMORY.md   │────────▶│ hook fires  │────────▶│ EPHEMERAL.md │
 │             │        │ with stable │         │             │         │ with session │
 │ Archives    │        │ facts       │         └──────┬──────┘         │ summary      │
 │ EPHEMERAL → │        └─────────────┘                │                └──────────────┘
 │ archive log │                                       ▼                 (that's it —
 │ + clears it │                              ┌──────────────┐          promoted next
 └─────────────┘                              │ recall-echo  │          session)
                                              │ checkpoint   │
                                              │ --trigger    │
                                              │ precompact   │
                                              └──────┬───────┘
                                                     │
                                                     ▼
                                              ┌──────────────┐
                                              │ CLI creates  │
                                              │ scaffolded   │
                                              │ archive log  │
                                              │ + updates    │
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

The CLI handles all mechanical bookkeeping — numbering, file creation, timestamps, index updates. The agent writes one summary to EPHEMERAL.md at session end, and the CLI promotes it at the next session start.

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

Initialize or upgrade the memory system. Creates directories, writes protocol rules, and configures the PreCompact hook. Idempotent — running it again won't overwrite existing memory files. Upgrades legacy echo hooks to the new checkpoint hook.

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

### `recall-echo status`

Memory system health check. Shows MEMORY.md line count, EPHEMERAL.md state, archive log count, protocol status, and hook configuration. Warns about approaching limits or legacy hooks.

```
recall-echo — memory system status

  MEMORY.md:    142/200 lines (71%)
  EPHEMERAL.md: has content (pending promotion)
  Archive logs: 23 logs, latest: 2026-02-24
  Protocol:     installed
  Hook:         checkpoint (active)

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
└── settings.json ················· PreCompact hook: recall-echo checkpoint
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

The only thing it touches in `settings.json` is adding a `PreCompact` hook that runs `recall-echo checkpoint` before context compaction. If you already have hooks configured, they're preserved. If upgrading from v0.2.0, the legacy echo hook is automatically replaced.

## How the Agent Uses It

Once installed, the agent follows the protocol automatically:

- **Runs `recall-echo promote`** at session start to archive the previous session's EPHEMERAL.md
- **Updates `MEMORY.md`** when it learns stable facts (never speculative or session-specific info)
- **Writes `EPHEMERAL.md`** at session end with a rich session summary
- **Fills in archive log sections** when a precompact checkpoint fires
- **Searches archives** with `Grep` when it needs historical context
- **Distills `MEMORY.md`** proactively when it approaches 200 lines, moving details to topic files

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

You may also want to remove the `PreCompact` hook from `~/.claude/settings.json`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[MIT](LICENSE)
