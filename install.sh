#!/usr/bin/env bash
set -euo pipefail

# recall-echo — Persistent memory for AI coding agents
# https://github.com/dnacenta/recall-echo

CLAUDE_DIR="${HOME}/.claude"
RULES_DIR="${CLAUDE_DIR}/rules"
MEMORY_DIR="${CLAUDE_DIR}/memory"
MEMORIES_DIR="${CLAUDE_DIR}/memories"
EPHEMERAL_FILE="${CLAUDE_DIR}/EPHEMERAL.md"
ARCHIVE_FILE="${CLAUDE_DIR}/ARCHIVE.md"
MEMORY_FILE="${MEMORY_DIR}/MEMORY.md"
RULES_FILE="${RULES_DIR}/recall-echo.md"
SETTINGS_FILE="${CLAUDE_DIR}/settings.json"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color
BOLD='\033[1m'

info() { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}~${NC} $1"; }
fail() { echo -e "  ${RED}✗${NC} $1"; }

echo ""
echo -e "${BOLD}recall-echo${NC} — initializing memory system"
echo ""

# Pre-flight: check for ~/.claude
if [ ! -d "$CLAUDE_DIR" ]; then
  fail "~/.claude directory not found. Is Claude Code installed?"
  echo "  Install Claude Code first, then run this again."
  exit 1
fi

# 1. Create directories
mkdir -p "$RULES_DIR" "$MEMORY_DIR" "$MEMORIES_DIR"

# 2. Write the memory protocol rules file
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE_FILE="${SCRIPT_DIR}/templates/recall-echo.md"

if [ -f "$RULES_FILE" ]; then
  if [ -f "$TEMPLATE_FILE" ] && ! diff -q "$RULES_FILE" "$TEMPLATE_FILE" > /dev/null 2>&1; then
    echo -n "  Memory protocol already exists but differs from latest. Overwrite? [y/N] "
    read -r answer
    if [[ "$answer" =~ ^[Yy] ]]; then
      cp "$TEMPLATE_FILE" "$RULES_FILE"
      info "Updated memory protocol (${RULES_FILE})"
    else
      warn "Kept existing memory protocol"
    fi
  else
    warn "Memory protocol already up to date"
  fi
else
  if [ -f "$TEMPLATE_FILE" ]; then
    cp "$TEMPLATE_FILE" "$RULES_FILE"
  else
    # Inline fallback if template not found (e.g., curl install)
    cat > "$RULES_FILE" << 'PROTOCOL'
# recall-echo — Memory Protocol

You have a persistent three-layer memory system. Use it to maintain continuity across sessions.

## Memory Layers

### Layer 1 — Curated Memory (MEMORY.md)
- Location: `~/.claude/memory/MEMORY.md` (and optional topic files alongside it)
- This is your source of truth. Distilled facts, user preferences, project patterns, key decisions.
- It is auto-loaded at session start (first 200 lines).
- Keep it under 200 lines. If it grows beyond that, distill aggressively or move details to topic files (e.g., `debugging.md`, `architecture.md`) in the same directory.
- Only write stable, confirmed facts. Never write speculative or session-specific information here.
- Before adding a new entry, check if an existing entry should be updated instead. No duplicates.

### Layer 2 — Short-Term Memory (EPHEMERAL.md)
@~/.claude/EPHEMERAL.md
- This contains a summary of the last session. It gives you immediate context about what happened recently.
- At the start of a session, read it to orient yourself, then clear the file (empty it). It has been consumed.
- At the end of a session, write a fresh summary of the current session to this file.
- Contents should include: date, key topics discussed, decisions made, action items, unresolved threads.

### Layer 3 — Long-Term Memory (archive logs)
- Index: `~/.claude/ARCHIVE.md`
- Logs: `~/.claude/memories/archive-log-XXX.md` (sequentially numbered: 001, 002, 003...)
- This is your full history. NOT loaded into context — search it on demand using Grep.
- Each archive log is a checkpoint of a conversation or portion of a conversation.
- ARCHIVE.md is a lightweight index: sequence number, date, and key topics per entry.
- To search history, use: `Grep pattern="search term" path="~/.claude/memories/"`

## Session Lifecycle

### On session start:
1. MEMORY.md is already in your context (auto-loaded).
2. Read EPHEMERAL.md (imported above). Use it to orient — what happened last time?
3. Clear EPHEMERAL.md by writing an empty string to it. It's been consumed.

### During the session:
- Update MEMORY.md when you learn stable facts (user preferences, project decisions, confirmed patterns).
- Do NOT update MEMORY.md with session-specific or speculative information.
- If you need historical context, search the archive: `Grep pattern="topic" path="~/.claude/memories/"`

### On PreCompact (context about to be compressed):
If you see a system message indicating context compaction is imminent:
1. Determine the next archive log number by checking `~/.claude/memories/` for the highest existing number.
2. Write a checkpoint to `~/.claude/memories/archive-log-XXX.md` with:
   - Date and timestamp
   - Trigger: "precompact"
   - Summary of the conversation so far
   - Key decisions, code changes, and unresolved items
3. Add an entry to `~/.claude/ARCHIVE.md` with the log number, date, and key topics.

### On session end:
When the conversation is wrapping up (user says goodbye, task is complete, or you sense the session is ending):
1. Write EPHEMERAL.md with a rich summary of this session.
2. Save a final archive log to `~/.claude/memories/archive-log-XXX.md` with:
   - Date and timestamp
   - Trigger: "session-end"
   - Full session summary
   - Action items and unresolved threads
3. Update ARCHIVE.md index.

## Archive Log Format

Each archive log file should follow this structure:

```
# Archive Log XXX

- **Date**: YYYY-MM-DD HH:MM
- **Trigger**: precompact | session-end
- **Session context**: [brief description of what this session was about]

## Summary
[What was discussed, decided, and accomplished]

## Key Details
[Important specifics — code changes, configurations, decisions with rationale]

## Action Items
[What needs to happen next]

## Unresolved
[Open questions or threads to pick up later]
```

## Rules

- Never write duplicate information to MEMORY.md. Check first, update if exists.
- EPHEMERAL.md is always either empty (mid-session) or contains the last session's summary (between sessions). Never both.
- Archive logs are immutable once written. Never modify an existing archive log.
- When MEMORY.md approaches 200 lines, proactively distill it. Move detailed notes to topic files.
- The memory system is yours. Use it actively — don't wait to be asked.
PROTOCOL
  fi
  info "Created memory protocol (${RULES_FILE})"
fi

# 3. Write MEMORY.md (never overwrite)
if [ -f "$MEMORY_FILE" ]; then
  warn "MEMORY.md already exists — preserved"
else
  cat > "$MEMORY_FILE" << 'EOF'
# Memory

<!-- recall-echo: Curated memory. Distilled facts, preferences, patterns. -->
<!-- Keep under 200 lines. Only write confirmed, stable information. -->
EOF
  info "Created MEMORY.md (${MEMORY_FILE})"
fi

# 4. Write EPHEMERAL.md (never overwrite — may have active session data)
if [ -f "$EPHEMERAL_FILE" ]; then
  warn "EPHEMERAL.md already exists — preserved"
else
  touch "$EPHEMERAL_FILE"
  info "Created EPHEMERAL.md (${EPHEMERAL_FILE})"
fi

# 5. Write ARCHIVE.md (never overwrite)
if [ -f "$ARCHIVE_FILE" ]; then
  warn "ARCHIVE.md already exists — preserved"
else
  cat > "$ARCHIVE_FILE" << 'EOF'
# Archive Index

<!-- recall-echo: Lightweight index of archive logs. -->
<!-- Format: | log number | date | key topics | -->
EOF
  info "Created ARCHIVE.md (${ARCHIVE_FILE})"
fi

# 6. Merge PreCompact hook into settings.json
if [ -f "$SETTINGS_FILE" ]; then
  # Check if recall-echo hook already exists
  if grep -q "RECALL-ECHO" "$SETTINGS_FILE" 2>/dev/null; then
    warn "PreCompact hook already configured"
  else
    # Use a temp file to safely merge
    TEMP_FILE=$(mktemp)

    # Check if hooks.PreCompact already exists
    if python3 -c "
import json, sys
with open('$SETTINGS_FILE') as f:
    s = json.load(f)
hooks = s.setdefault('hooks', {})
pre = hooks.setdefault('PreCompact', [])
pre.append({
    'hooks': [{
        'type': 'command',
        'command': \"echo 'RECALL-ECHO: Context compaction imminent. Save a memory checkpoint to ~/.claude/memories/ before context is lost. Check the highest archive-log-XXX.md number and create the next one.'\"
    }]
})
with open('$TEMP_FILE', 'w') as f:
    json.dump(s, f, indent=2)
    f.write('\n')
" 2>/dev/null; then
      mv "$TEMP_FILE" "$SETTINGS_FILE"
      info "Added PreCompact hook to settings.json"
    else
      rm -f "$TEMP_FILE"
      fail "Could not merge PreCompact hook — add it manually to ${SETTINGS_FILE}"
    fi
  fi
else
  # Create fresh settings.json with just the hook
  cat > "$SETTINGS_FILE" << 'EOF'
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo 'RECALL-ECHO: Context compaction imminent. Save a memory checkpoint to ~/.claude/memories/ before context is lost. Check the highest archive-log-XXX.md number and create the next one.'"
          }
        ]
      }
    ]
  }
}
EOF
  info "Created settings.json with PreCompact hook"
fi

# Done
echo ""
echo -e "${BOLD}Setup complete.${NC} Your memory system is ready."
echo ""
echo "  Layer 1 (MEMORY.md)     — Curated facts, always in context"
echo "  Layer 2 (EPHEMERAL.md)  — Last session summary, read then cleared"
echo "  Layer 3 (Archive)       — Searchable history in ~/.claude/memories/"
echo ""
echo "  The memory protocol loads automatically via ~/.claude/rules/recall-echo.md"
echo "  Start a new Claude Code session and your agent will have persistent memory."
echo ""
