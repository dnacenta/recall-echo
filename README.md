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
┌─────────────────────────────────────────────────┐
│                  recall-echo                     │
│                                                  │
│  Layer 1: MEMORY.md        ← always in context   │
│  Curated facts, preferences, stable patterns     │
│                                                  │
│  Layer 2: EPHEMERAL.md     ← session bridge      │
│  Last session summary, read on start, cleared    │
│                                                  │
│  Layer 3: archive logs     ← searchable history  │
│  ~/.claude/memories/archive-log-001.md ...       │
│  Checkpointed on compaction and session end      │
│                                                  │
│  ARCHIVE.md                ← lightweight index   │
│  Log number, date, key topics per entry          │
└─────────────────────────────────────────────────┘
```

### Session Lifecycle

1. **Session start** — Agent reads `EPHEMERAL.md` for last session context, then clears it.
2. **During session** — Agent updates `MEMORY.md` with stable facts as they're confirmed.
3. **On compaction** — `PreCompact` hook fires, agent saves a checkpoint to `archive-log-XXX.md`.
4. **Session end** — Agent writes a fresh `EPHEMERAL.md` summary and a final archive log.

The agent manages all of this autonomously. You don't need to tell it to remember things — the protocol is in its rules.

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

## What It Creates

```
~/.claude/
├── rules/
│   └── recall-echo.md       # Memory protocol (auto-loaded by Claude Code)
├── memory/
│   └── MEMORY.md             # Layer 1: curated facts
├── memories/
│   └── archive-log-001.md    # Layer 3: archive logs (created by agent)
├── EPHEMERAL.md              # Layer 2: session bridge
├── ARCHIVE.md                # Archive index
└── settings.json             # PreCompact hook merged in
```

The installer is idempotent — running it again won't overwrite your existing memory files. It only creates what's missing and updates the protocol rules file to the latest version.

## Configuration

recall-echo requires no configuration. It works out of the box with Claude Code's existing infrastructure.

The only thing it touches in `settings.json` is adding a `PreCompact` hook that reminds the agent to checkpoint before context compaction. If you already have hooks configured, they're preserved.

## How the Agent Uses It

Once installed, the agent follows the protocol automatically:

- **Reads `EPHEMERAL.md`** at session start to pick up where it left off
- **Updates `MEMORY.md`** when it learns stable facts (never speculative or session-specific info)
- **Creates archive logs** on compaction events and at session end
- **Searches archives** with `Grep` when it needs historical context
- **Distills `MEMORY.md`** proactively when it approaches 200 lines, moving details to topic files

You can also explicitly tell the agent to remember something, search its history, or review what it knows. The memory is transparent — it's all plain markdown files you can read and edit yourself.

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
