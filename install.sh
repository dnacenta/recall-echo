#!/usr/bin/env bash
set -euo pipefail

# recall-echo — Persistent memory for AI coding agents
# https://github.com/dnacenta/recall-echo

REPO="dnacenta/recall-echo"
CLAUDE_DIR="${HOME}/.claude"
INSTALL_DIR="${HOME}/.local/bin"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'
BOLD='\033[1m'

info() { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}~${NC} $1"; }
fail() { echo -e "  ${RED}✗${NC} $1"; }

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)  os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *)      echo ""; return ;;
  esac

  case "$arch" in
    x86_64)  arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *)       echo ""; return ;;
  esac

  echo "${arch}-${os}"
}

download_binary() {
  local target="$1"
  local tag url tmpdir

  # Get latest release tag
  tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)" || return 1

  [ -z "$tag" ] && return 1

  url="https://github.com/${REPO}/releases/download/${tag}/recall-echo-${target}.tar.gz"

  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT

  if curl -fsSL "$url" -o "${tmpdir}/recall-echo.tar.gz" 2>/dev/null; then
    tar xzf "${tmpdir}/recall-echo.tar.gz" -C "$tmpdir"
    mkdir -p "$INSTALL_DIR"
    mv "${tmpdir}/recall-echo" "${INSTALL_DIR}/recall-echo"
    chmod +x "${INSTALL_DIR}/recall-echo"
    return 0
  fi

  return 1
}

echo ""
echo -e "${BOLD}recall-echo${NC} — installer"
echo ""

# Try to download prebuilt binary
TARGET="$(detect_target)"
BINARY=""

if [ -n "$TARGET" ]; then
  echo "  Detected platform: ${TARGET}"
  if download_binary "$TARGET"; then
    BINARY="${INSTALL_DIR}/recall-echo"
    info "Downloaded binary to ${BINARY}"

    # Check if ~/.local/bin is in PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
      warn "${INSTALL_DIR} is not in your PATH — add it to your shell profile"
    fi
  else
    warn "Could not download prebuilt binary — falling back to bash installer"
  fi
else
  warn "Could not detect platform — falling back to bash installer"
fi

# Run init: prefer binary if downloaded, otherwise inline bash
if [ -n "$BINARY" ]; then
  "$BINARY" init
  exit 0
fi

# ─── Bash fallback ─────────────────────────────────────────────────────

RULES_DIR="${CLAUDE_DIR}/rules"
MEMORY_DIR="${CLAUDE_DIR}/memory"
MEMORIES_DIR="${CLAUDE_DIR}/memories"
EPHEMERAL_FILE="${CLAUDE_DIR}/EPHEMERAL.md"
ARCHIVE_FILE="${CLAUDE_DIR}/ARCHIVE.md"
MEMORY_FILE="${MEMORY_DIR}/MEMORY.md"
RULES_FILE="${RULES_DIR}/recall-echo.md"
SETTINGS_FILE="${CLAUDE_DIR}/settings.json"

# Determine hook commands: use full binary path to avoid PATH issues in hook subprocesses
RECALL_BIN="${BINARY:-$(command -v recall-echo 2>/dev/null || echo "")}"
if [ -n "$RECALL_BIN" ]; then
  HOOK_COMMAND="${RECALL_BIN} checkpoint --trigger precompact"
  SESSION_END_COMMAND="${RECALL_BIN} promote"
else
  HOOK_COMMAND="echo 'RECALL-ECHO: Context compaction imminent. Save a memory checkpoint to ~/.claude/memories/ before context is lost. Check the highest archive-log-XXX.md number and create the next one.'"
  SESSION_END_COMMAND=""  # Can't promote without binary
fi

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
- This is a staging area for session summaries. Between sessions, it holds the previous session's summary waiting to be promoted to an archive log.
- At the end of a session, write a fresh summary of the current session to this file.
- Contents should include: date, key topics discussed, decisions made, code changes, action items, unresolved threads.
- At the start of the next session, `recall-echo promote` archives it automatically.

### Layer 3 — Long-Term Memory (archive logs)
- Index: `~/.claude/ARCHIVE.md`
- Logs: `~/.claude/memories/archive-log-XXX.md` (sequentially numbered: 001, 002, 003...)
- This is your full history. NOT loaded into context — search it on demand using Grep.
- Each archive log is a checkpoint of a conversation or portion of a conversation.
- ARCHIVE.md is a lightweight index: sequence number, date, and key topics per entry.
- To search history, use: `Grep pattern="search term" path="~/.claude/memories/"`

## Session Lifecycle

### On session start:
1. Run `recall-echo promote` (via Bash tool) to archive the previous session's EPHEMERAL.md into an archive log.
2. MEMORY.md is already in your context (auto-loaded).
3. If you need context from the last session, read the archive log that was just promoted.

### During the session:
- Update MEMORY.md when you learn stable facts (user preferences, project decisions, confirmed patterns).
- Do NOT update MEMORY.md with session-specific or speculative information.
- If you need historical context, search the archive: `Grep pattern="topic" path="~/.claude/memories/"`

### On PreCompact (context about to be compressed):
The PreCompact hook automatically runs `recall-echo checkpoint --trigger precompact`.
The output tells you the file path and log number. Open that file and fill in the
Summary, Key Details, Action Items, and Unresolved sections with context from the
current conversation.

### On session end:
When the conversation is wrapping up (user says goodbye, task is complete, or you sense the session is ending):
1. Write EPHEMERAL.md with a rich session summary.
   Include: what was discussed, key decisions, code changes, action items, unresolved threads.
2. That's it. The SessionEnd hook runs `recall-echo promote` automatically on exit,
   archiving EPHEMERAL.md into an archive log and clearing it.
   (The session-start promote is a safety net in case the hook didn't fire.)

## Archive Log Format

Archive logs are created by `recall-echo checkpoint` (precompact) or `recall-echo promote` (session end) with YAML frontmatter.

For precompact checkpoints, you fill in the section templates. For promoted logs, the content comes from EPHEMERAL.md automatically.

```yaml
---
log: 5
date: "2026-02-24T21:30:00Z"
trigger: precompact
context: ""
topics: []
---
```

Sections to fill in (precompact only):
- **Summary** — What was discussed, decided, and accomplished
- **Key Details** — Important specifics: code changes, configurations, decisions with rationale
- **Action Items** — What needs to happen next
- **Unresolved** — Open questions or threads to pick up later

Old logs without frontmatter continue to work — numbering is by filename, not content.

## Commands

- `recall-echo init` — Initialize or upgrade the memory system (installs PreCompact + SessionEnd hooks)
- `recall-echo checkpoint --trigger precompact [--context "..."]` — Create a precompact archive checkpoint
- `recall-echo promote [--context "..."]` — Promote EPHEMERAL.md into an archive log
- `recall-echo status` — Check memory system health

## Rules

- Never write duplicate information to MEMORY.md. Check first, update if exists.
- EPHEMERAL.md holds the session summary between sessions. It is promoted to an archive log at the start of the next session.
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

# 6. Merge hooks (PreCompact + SessionEnd) into settings.json
if [ -f "$SETTINGS_FILE" ]; then
  TEMP_FILE=$(mktemp)

  if python3 -c "
import json, sys
with open('$SETTINGS_FILE') as f:
    s = json.load(f)
hooks = s.setdefault('hooks', {})
changed = False
messages = []

# --- PreCompact hook ---
pre = hooks.get('PreCompact', [])
has_checkpoint = any(
    'recall-echo checkpoint' in h.get('command', '')
    for e in pre for h in e.get('hooks', [])
)
has_legacy = any(
    'RECALL-ECHO' in h.get('command', '')
    for e in pre for h in e.get('hooks', [])
)

if has_checkpoint:
    messages.append('~|PreCompact hook already up to date')
elif has_legacy:
    pre = [e for e in pre if not any(
        'RECALL-ECHO' in h.get('command', '')
        for h in e.get('hooks', [])
    )]
    pre.append({'hooks': [{'type': 'command', 'command': '$HOOK_COMMAND'}]})
    hooks['PreCompact'] = pre
    changed = True
    messages.append('+|Migrated PreCompact hook: echo → checkpoint')
else:
    pre = hooks.setdefault('PreCompact', [])
    pre.append({'hooks': [{'type': 'command', 'command': '$HOOK_COMMAND'}]})
    changed = True
    messages.append('+|Added PreCompact hook')

# --- SessionEnd hook ---
session_end_cmd = '$SESSION_END_COMMAND'
if session_end_cmd:
    se = hooks.get('SessionEnd', [])
    has_promote = any(
        'recall-echo promote' in h.get('command', '')
        for e in se for h in e.get('hooks', [])
    )
    if has_promote:
        messages.append('~|SessionEnd hook already up to date')
    else:
        se = hooks.setdefault('SessionEnd', [])
        se.append({'hooks': [{'type': 'command', 'command': session_end_cmd}]})
        changed = True
        messages.append('+|Added SessionEnd hook')

s['hooks'] = hooks
if changed:
    with open('$TEMP_FILE', 'w') as f:
        json.dump(s, f, indent=2)
        f.write('\n')

for m in messages:
    print(m)
sys.exit(0 if not changed else 0)
" 2>/dev/null > /tmp/recall-echo-hook-output; then
    # Parse output for status messages
    while IFS='|' read -r kind msg; do
      case "$kind" in
        '+') info "$msg" ;;
        '~') warn "$msg" ;;
        *) echo "  $msg" ;;
      esac
    done < /tmp/recall-echo-hook-output
    rm -f /tmp/recall-echo-hook-output

    if [ -f "$TEMP_FILE" ] && [ -s "$TEMP_FILE" ]; then
      mv "$TEMP_FILE" "$SETTINGS_FILE"
    else
      rm -f "$TEMP_FILE"
    fi
  else
    rm -f "$TEMP_FILE"
    fail "Could not merge hooks — add them manually to ${SETTINGS_FILE}"
  fi
else
  # Create fresh settings.json with hooks
  if [ -n "$SESSION_END_COMMAND" ]; then
    cat > "$SETTINGS_FILE" << EOF
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${HOOK_COMMAND}"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${SESSION_END_COMMAND}"
          }
        ]
      }
    ]
  }
}
EOF
  else
    cat > "$SETTINGS_FILE" << EOF
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${HOOK_COMMAND}"
          }
        ]
      }
    ]
  }
}
EOF
  fi
  info "Created settings.json with hooks"
fi

# Done
echo ""
echo -e "${BOLD}Setup complete.${NC} Your memory system is ready."
echo ""
echo "  Layer 1 (MEMORY.md)     — Curated facts, always in context"
echo "  Layer 2 (EPHEMERAL.md)  — Last session summary, promoted on exit"
echo "  Layer 3 (Archive)       — Searchable history in ~/.claude/memories/"
echo ""
echo "  The memory protocol loads automatically via ~/.claude/rules/recall-echo.md"
echo "  Start a new Claude Code session and your agent will have persistent memory."
echo ""
