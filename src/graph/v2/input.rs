// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Role-tagged session input.
//!
//! The markdown archive stays the human-facing record. A sidecar under
//! `turns/` keeps who said what, which tools ran between those turns, the
//! session date and the project, so a reader does not have to guess them
//! back out of merged headings and tool output. An older archive has no
//! sidecar; it is read from the markdown role blocks instead.
//!
//! `load_session` and `window` are that reader. The second pipeline reads
//! them when it extracts. This module still only loads and renders.

use std::fs;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use super::super::error::GraphError;
use crate::conversation::{date_from_timestamp, ConversationEntry};

const SIDECAR_VERSION: u32 = 1;

/// Who spoke a text turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TurnRole {
    User,
    Assistant,
}

/// One text turn, numbered from zero in session order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    pub index: usize,
    pub role: TurnRole,
    pub text: String,
}

/// Project named from the session's working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectIdentity {
    pub name: String,
    pub aliases: Vec<String>,
}

/// A session the second pipeline can read, whether it came from a sidecar
/// or from an older markdown archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInput {
    pub session_id: String,
    pub date: String,
    pub project: ProjectIdentity,
    pub turns: Vec<Turn>,
    pub tool_lines: Vec<(usize, Vec<String>)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TurnsFile {
    version: u32,
    session_id: String,
    date: String,
    cwd: Option<String>,
    project: ProjectIdentity,
    turns: Vec<SidecarTurn>,
    tools: Vec<ToolRun>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SidecarTurn {
    i: usize,
    role: TurnRole,
    text: String,
    ts: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolRun {
    before_turn: usize,
    names: Vec<String>,
}

/// Repo name from `origin`, then the directory name. No cwd yields an empty name.
///
/// The remote is read from `.git/config`. There is no subprocess: archiving
/// must not depend on `git` being installed or on a configured command.
#[must_use]
pub(crate) fn project_identity(cwd: Option<&Path>) -> ProjectIdentity {
    let Some(cwd) = cwd else {
        return empty_project();
    };
    let mut aliases = Vec::new();
    if let Some(repo) = origin_repo_name(cwd) {
        push_unique(&mut aliases, repo);
    }
    if let Some(base) = directory_name(cwd) {
        push_unique(&mut aliases, base);
    }
    let name = aliases.first().cloned().unwrap_or_default();
    ProjectIdentity { name, aliases }
}

/// Write `turns/conversation-NNN.json`. The caller logs a failure and still
/// keeps the archive: the sidecar is an extra record, not the archive itself.
pub(crate) fn write_turns_sidecar(
    memory_dir: &Path,
    log_number: u32,
    session_id: &str,
    date: &str,
    cwd: Option<&str>,
    entries: &[ConversationEntry],
) -> Result<(), GraphError> {
    let dir = memory_dir.join("turns");
    fs::create_dir_all(&dir)?;
    let cwd = cwd.map(str::trim).filter(|value| !value.is_empty());
    let (turns, tools) = turns_and_tools(entries);
    let sidecar = TurnsFile {
        version: SIDECAR_VERSION,
        session_id: session_id.to_string(),
        date: date.to_string(),
        cwd: cwd.map(str::to_string),
        project: project_identity(cwd.map(Path::new)),
        turns,
        tools,
    };
    let path = dir.join(format!("conversation-{log_number:03}.json"));
    let body = serde_json::to_string_pretty(&sidecar)?;
    fs::write(path, format!("{body}\n"))?;
    Ok(())
}

/// Sidecar when one was written, otherwise the markdown archive.
pub(crate) fn load_session(memory_dir: &Path, log_number: u32) -> Result<SessionInput, GraphError> {
    let stem = format!("conversation-{log_number:03}");
    let sidecar = memory_dir.join("turns").join(format!("{stem}.json"));
    if sidecar.is_file() {
        return read_sidecar(&sidecar);
    }
    let archive = memory_dir.join("conversations").join(format!("{stem}.md"));
    if archive.is_file() {
        return read_markdown(&archive);
    }
    Err(GraphError::NotFound(format!(
        "{stem} has no turns sidecar and no archive"
    )))
}

/// Lines the second pipeline reads.
///
/// A window is `[T<i>] USER:` / `[T<i>] ASSISTANT:` lines, with
/// `(assistant used tools: …)` before the turn those tools precede. The
/// caller's budget is `max_chars` (the pipeline default is 48,000). A longer
/// session is split on turn boundaries. The last two turns of a window open
/// the next one, so a claim that crosses the cut is still seen together.
#[must_use]
pub(crate) fn window(session: &SessionInput, max_chars: usize) -> Vec<String> {
    let count = session.turns.len();
    let full = render_window(session, 0, count);
    // No turn boundary to split on: one window, even when it is over budget.
    if full.chars().count() <= max_chars || count == 0 {
        return if full.is_empty() {
            Vec::new()
        } else {
            vec![full]
        };
    }

    let mut windows = Vec::new();
    let mut start = 0;
    while start < count {
        let mut end = start + 1;
        while end < count && fits(session, start, end + 1, max_chars) {
            end += 1;
        }
        windows.push(render_window(session, start, end));
        if end >= count {
            break;
        }
        // Repeating the last two turns only advances when the window holds
        // more than those two. Otherwise the next window would be the same
        // cut, so step forward by one turn.
        let repeated = end.saturating_sub(2);
        start = if repeated > start {
            repeated
        } else {
            start + 1
        };
    }
    windows
}

fn empty_project() -> ProjectIdentity {
    ProjectIdentity {
        name: String::new(),
        aliases: Vec::new(),
    }
}

/// A transcript cwd is untrusted. Read `.git/config` only when it cannot stall
/// the archive: an absolute path, a regular file, and a small one.
const GIT_CONFIG_MAX_BYTES: u64 = 64 * 1024;

fn origin_repo_name(cwd: &Path) -> Option<String> {
    if !cwd.is_absolute() {
        return None;
    }
    let path = cwd.join(".git").join("config");
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > GIT_CONFIG_MAX_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    repo_name(&origin_url(&text)?)
}

fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for raw in config.lines() {
        let line = strip_hash_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_origin = is_origin_remote(line);
            continue;
        }
        if in_origin {
            if let Some(url) = url_value(line) {
                return Some(url.to_string());
            }
        }
    }
    None
}

fn is_origin_remote(header: &str) -> bool {
    let Some(inner) = header
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
    else {
        return false;
    };
    let mut parts = inner.split_whitespace();
    if parts.next() != Some("remote") {
        return false;
    }
    let Some(name) = parts.next() else {
        return false;
    };
    parts.next().is_none() && name.trim_matches(|ch| ch == '"' || ch == '\'') == "origin"
}

fn url_value(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("url")?;
    // `url` is the whole key. `urlx = …` is a different key.
    if rest.starts_with(|ch: char| !ch.is_whitespace() && ch != '=') {
        return None;
    }
    let url = rest
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .trim_matches(|ch| ch == '"' || ch == '\'');
    (!url.is_empty()).then_some(url)
}

fn repo_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let segment = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    let name = segment.strip_suffix(".git").unwrap_or(segment);
    (!name.is_empty()).then(|| name.to_string())
}

fn directory_name(path: &Path) -> Option<String> {
    path.components()
        .rev()
        .find_map(|component| match component {
            Component::Normal(name) => name.to_str().map(str::to_string),
            _ => None,
        })
}

fn strip_hash_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted => return line[..index].trim_end(),
            _ => {}
        }
    }
    line
}

fn turns_and_tools(entries: &[ConversationEntry]) -> (Vec<SidecarTurn>, Vec<ToolRun>) {
    let mut turns = Vec::new();
    let mut tools = Vec::new();
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            ConversationEntry::UserMessage(text) | ConversationEntry::AssistantText(text) => {
                if text.is_empty() {
                    continue;
                }
                push_run(&mut tools, &mut names, turns.len());
                let role = match entry {
                    ConversationEntry::UserMessage(_) => TurnRole::User,
                    _ => TurnRole::Assistant,
                };
                push_turn(&mut turns, role, text);
            }
            ConversationEntry::ToolUse { name, .. } => push_unique(&mut names, name.clone()),
            ConversationEntry::ToolResult { .. } => {}
        }
    }
    push_run(&mut tools, &mut names, turns.len());
    (turns, tools)
}

fn push_turn(turns: &mut Vec<SidecarTurn>, role: TurnRole, text: &str) {
    let i = turns.len();
    turns.push(SidecarTurn {
        i,
        role,
        text: text.to_string(),
        ts: None,
    });
}

fn push_run(tools: &mut Vec<ToolRun>, names: &mut Vec<String>, before_turn: usize) {
    if names.is_empty() {
        return;
    }
    tools.push(ToolRun {
        before_turn,
        names: std::mem::take(names),
    });
}

fn push_unique(names: &mut Vec<String>, name: String) {
    if !names.iter().any(|existing| existing == &name) {
        names.push(name);
    }
}

fn read_sidecar(path: &Path) -> Result<SessionInput, GraphError> {
    let file: TurnsFile = serde_json::from_str(&fs::read_to_string(path)?)?;
    if file.version != SIDECAR_VERSION {
        return Err(GraphError::Parse(format!(
            "unsupported turns sidecar version {}",
            file.version
        )));
    }
    Ok(SessionInput {
        session_id: file.session_id,
        date: file.date,
        project: file.project,
        turns: file
            .turns
            .into_iter()
            .map(|turn| Turn {
                index: turn.i,
                role: turn.role,
                text: turn.text,
            })
            .collect(),
        tool_lines: file
            .tools
            .into_iter()
            .map(|run| (run.before_turn, run.names))
            .collect(),
    })
}

fn read_markdown(path: &Path) -> Result<SessionInput, GraphError> {
    let text = fs::read_to_string(path)?;
    let frontmatter = crate::frontmatter::parse(&text)
        .ok_or_else(|| GraphError::Parse(format!("{} has no frontmatter", path.display())))?;
    let mut parser = MarkdownTurns::default();
    for line in text.lines() {
        parser.push_line(line);
    }
    parser.finish();
    Ok(SessionInput {
        session_id: frontmatter.session_id,
        date: date_from_timestamp(&frontmatter.date),
        project: empty_project(),
        turns: parser.turns,
        tool_lines: parser.tool_lines,
    })
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DetailsSkip {
    #[default]
    Off,
    /// Writer form is `<details>…` then a blank line, then an opening fence.
    AwaitFence,
    InFence,
    /// `</details>` inside the fenced output must not end the skip.
    AwaitClose,
}

#[derive(Default)]
struct MarkdownTurns {
    turns: Vec<Turn>,
    tool_lines: Vec<(usize, Vec<String>)>,
    role: Option<TurnRole>,
    text: String,
    pending: Vec<String>,
    details: DetailsSkip,
}

impl MarkdownTurns {
    fn push_line(&mut self, raw: &str) {
        let line = raw.trim_end_matches('\r');
        if self.details != DetailsSkip::Off {
            self.step_details(line);
            return;
        }
        if is_writer_details(line) {
            self.details = DetailsSkip::AwaitFence;
            return;
        }
        if let Some(role) = heading_role(line) {
            self.start_turn(role);
            return;
        }
        if ends_writer_section(line) {
            self.finish_text();
            return;
        }
        if line.trim() == "---" {
            return;
        }
        if let Some(name) = quoted_tool(line) {
            push_unique(&mut self.pending, name);
            return;
        }
        if self.role.is_some() {
            self.text.push_str(line);
            self.text.push('\n');
        }
    }

    /// The writer wraps tool output in one fence, then a line that is
    /// exactly `</details>`. A close tag inside that fence stays output.
    fn step_details(&mut self, line: &str) {
        let trimmed = line.trim();
        self.details = match self.details {
            DetailsSkip::AwaitFence if trimmed == "```" => DetailsSkip::InFence,
            DetailsSkip::InFence if trimmed == "```" => DetailsSkip::AwaitClose,
            DetailsSkip::AwaitClose if trimmed == "</details>" => DetailsSkip::Off,
            other => other,
        };
    }

    fn start_turn(&mut self, role: TurnRole) {
        self.finish_text();
        self.attach_pending(self.turns.len());
        self.role = Some(role);
    }

    fn finish_text(&mut self) {
        let Some(role) = self.role.take() else {
            return;
        };
        let text = std::mem::take(&mut self.text);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let index = self.turns.len();
        self.turns.push(Turn { index, role, text });
    }

    fn attach_pending(&mut self, before_turn: usize) {
        if self.pending.is_empty() {
            return;
        }
        self.tool_lines
            .push((before_turn, std::mem::take(&mut self.pending)));
    }

    fn finish(&mut self) {
        self.finish_text();
        self.attach_pending(self.turns.len());
    }
}

fn heading_role(line: &str) -> Option<TurnRole> {
    match line.trim() {
        "### User" => Some(TurnRole::User),
        "### Assistant" => Some(TurnRole::Assistant),
        _ => None,
    }
}

/// The writer emits `> **Name**: \`input\``. A blockquote without that
/// backtick span (`> **Note**: …`) is text, not a tool name.
fn quoted_tool(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("> **")?;
    let (name, value) = rest.split_once("**:")?;
    let value = value.trim();
    let span = value.len() >= 2 && value.starts_with('`') && value.ends_with('`');
    (span && !name.is_empty()).then(|| name.to_string())
}

/// The archive writer closes the turns with these two headings. Any other
/// `#` line (`#[derive]`, a shebang, a heading in the prompt) is text.
fn ends_writer_section(line: &str) -> bool {
    let line = line.trim();
    line == "## Tags" || is_conversation_heading(line)
}

fn is_conversation_heading(line: &str) -> bool {
    let Some(number) = line.strip_prefix("# Conversation ") else {
        return false;
    };
    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_writer_details(line: &str) -> bool {
    matches!(
        line.trim(),
        "<details><summary>Result</summary>" | "<details><summary>Error</summary>"
    )
}

fn render_window(session: &SessionInput, start: usize, end: usize) -> String {
    let mut lines = Vec::new();
    for turn in session.turns.iter().take(end).skip(start) {
        push_tools(&mut lines, session, turn.index);
        lines.push(format!(
            "[T{}] {}: {}",
            turn.index,
            role_label(turn.role),
            turn.text
        ));
    }
    if end == session.turns.len() {
        push_tools(&mut lines, session, session.turns.len());
    }
    lines.join("\n")
}

fn push_tools(lines: &mut Vec<String>, session: &SessionInput, before_turn: usize) {
    for (index, names) in &session.tool_lines {
        if *index == before_turn {
            lines.push(format!("(assistant used tools: {})", names.join(", ")));
        }
    }
}

fn role_label(role: TurnRole) -> &'static str {
    match role {
        TurnRole::User => "USER",
        TurnRole::Assistant => "ASSISTANT",
    }
}

fn fits(session: &SessionInput, start: usize, end: usize, max_chars: usize) -> bool {
    render_window(session, start, end).chars().count() <= max_chars
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_at(config: &str, dirname: &str) -> ProjectIdentity {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join(dirname);
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/config"), config).unwrap();
        project_identity(Some(&repo))
    }

    #[test]
    fn project_identity_from_git_remote_and_basename() {
        let empty = project_identity(None);
        assert_eq!(empty.name, "");
        assert!(empty.aliases.is_empty());

        let bare = tempfile::tempdir().unwrap();
        let repo = bare.path().join("linkshelf");
        fs::create_dir(&repo).unwrap();
        let base_only = project_identity(Some(&repo));
        assert_eq!(base_only.name, "linkshelf");
        assert_eq!(base_only.aliases, vec!["linkshelf".to_string()]);

        let https = identity_at(
            "[remote \"upstream\"]\n\turl = https://example.com/other/nope.git\n\
             [remote \"origin\"]\n\turl = https://example.com/acme/shelf.git\n",
            "linkshelf",
        );
        assert_eq!(https.name, "shelf");
        assert_eq!(
            https.aliases,
            vec!["shelf".to_string(), "linkshelf".to_string()]
        );

        let scp = identity_at(
            "[remote \"origin\"]\n\turl = git@example.com:acme/linkshelf.git\n",
            "linkshelf",
        );
        assert_eq!(scp.name, "linkshelf");
        assert_eq!(scp.aliases, vec!["linkshelf".to_string()]);

        let plain = identity_at(
            "[remote \"origin\"]\n\turl = \"https://example.com/acme/widget\"\n",
            "checkout",
        );
        assert_eq!(plain.name, "widget");
        assert_eq!(
            plain.aliases,
            vec!["widget".to_string(), "checkout".to_string()]
        );

        let slashed = identity_at(
            "[remote \"origin\"]\n\turl = https://example.com/acme/widget/\n",
            "checkout",
        );
        assert_eq!(slashed.name, "widget");
    }

    #[test]
    fn project_identity_skips_non_regular_or_oversize_git_config() {
        let tmp = tempfile::tempdir().unwrap();

        let fifo_repo = tmp.path().join("fifo-repo");
        fs::create_dir_all(fifo_repo.join(".git")).unwrap();
        let fifo = fifo_repo.join(".git/config");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path inside this temp dir, and
        // mkfifo only creates a fifo there. The read guard must not open it.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0);
        let fifo_id = project_identity(Some(&fifo_repo));
        assert_eq!(fifo_id.name, "fifo-repo");
        assert_eq!(fifo_id.aliases, vec!["fifo-repo".to_string()]);

        let dir_repo = tmp.path().join("dir-repo");
        fs::create_dir_all(dir_repo.join(".git/config")).unwrap();
        let dir_id = project_identity(Some(&dir_repo));
        assert_eq!(dir_id.name, "dir-repo");
        assert_eq!(dir_id.aliases, vec!["dir-repo".to_string()]);

        let huge_repo = tmp.path().join("huge-repo");
        fs::create_dir_all(huge_repo.join(".git")).unwrap();
        let mut body = String::from(
            "[remote \"origin\"]\n\turl = https://example.com/acme/should-not-read.git\n",
        );
        body.push_str(&"x".repeat(64 * 1024));
        fs::write(huge_repo.join(".git/config"), body).unwrap();
        let huge_id = project_identity(Some(&huge_repo));
        assert_eq!(huge_id.name, "huge-repo");
        assert_eq!(huge_id.aliases, vec!["huge-repo".to_string()]);
    }

    #[test]
    fn markdown_fallback_skips_tool_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        fs::create_dir_all(memory.join("conversations")).unwrap();
        let archive = "\
---
log: 7
date: \"2026-09-28T09:10:37Z\"
session_id: \"sess-7\"
message_count: 3
duration: \"6m\"
source: \"jsonl\"
topics: [\"linkshelf\"]
---

## Summary

A summary that must not become a turn.

# Conversation 007

---

### User

Ship the shelf.

---

### Assistant

Writing the store.

> **Write**: `src/store.py`

> **Bash**: `uv run pytest`

<details><summary>Result</summary>

```
SECRET_TOOL_OUTPUT should not appear
```

</details>

---

### User

Looks good.

> **Read**: `README.md`
";
        fs::write(memory.join("conversations/conversation-007.md"), archive).unwrap();

        let session = load_session(memory, 7).unwrap();
        assert_eq!(session.session_id, "sess-7");
        assert_eq!(session.date, "2026-09-28");
        assert_eq!(session.project.name, "");
        assert!(session.project.aliases.is_empty());
        assert_eq!(
            session.turns,
            vec![
                Turn {
                    index: 0,
                    role: TurnRole::User,
                    text: "Ship the shelf.".into(),
                },
                Turn {
                    index: 1,
                    role: TurnRole::Assistant,
                    text: "Writing the store.".into(),
                },
                Turn {
                    index: 2,
                    role: TurnRole::User,
                    text: "Looks good.".into(),
                },
            ]
        );
        assert_eq!(
            session.tool_lines,
            vec![
                (2, vec!["Write".into(), "Bash".into()]),
                (3, vec!["Read".into()]),
            ]
        );
        let rendered = format!("{session:?}");
        assert!(!rendered.contains("SECRET_TOOL_OUTPUT"));
        assert!(!rendered.contains("src/store.py"));
        assert!(!rendered.contains("must not become a turn"));
    }

    fn load_markdown(body: &str) -> SessionInput {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path();
        fs::create_dir_all(memory.join("conversations")).unwrap();
        let archive = format!(
            "---\nlog: 1\ndate: \"2026-09-28T09:10:37Z\"\nsession_id: \"sess\"\n\
             message_count: 1\nduration: \"1m\"\nsource: \"jsonl\"\ntopics: []\n---\n\n{body}"
        );
        fs::write(memory.join("conversations/conversation-001.md"), archive).unwrap();
        load_session(memory, 1).unwrap()
    }

    #[test]
    fn markdown_fallback_keeps_tool_output_that_contains_details_close() {
        let session = load_markdown(
            "\
### Assistant

ok

<details><summary>Result</summary>

```
<div></details>
</details>
LEAKED_TOOL_OUTPUT
```

</details>

still the assistant

### User

see <details> in the prompt
and </details> too
",
        );
        assert_eq!(
            session.turns,
            vec![
                Turn {
                    index: 0,
                    role: TurnRole::Assistant,
                    text: "ok\n\n\nstill the assistant".into(),
                },
                Turn {
                    index: 1,
                    role: TurnRole::User,
                    text: "see <details> in the prompt\nand </details> too".into(),
                },
            ]
        );
        assert!(session.tool_lines.is_empty());
        assert!(!format!("{session:?}").contains("LEAKED_TOOL_OUTPUT"));
    }

    #[test]
    fn markdown_fallback_keeps_hash_lines_in_the_turn() {
        let session = load_markdown(
            "\
# Conversation 001

### User

Fix this:
```rust
#[derive(Debug)]
struct KeepMe;
```
AFTER_HASH_LINE

#!/bin/bash
echo kept

## Heading inside the prompt

## Tags

**Project**: should-not-be-turn
",
        );
        assert_eq!(session.turns.len(), 1);
        assert_eq!(
            session.turns[0].text,
            "Fix this:\n```rust\n#[derive(Debug)]\nstruct KeepMe;\n```\n\
             AFTER_HASH_LINE\n\n#!/bin/bash\necho kept\n\n## Heading inside the prompt"
        );
        assert!(!session.turns[0].text.contains("should-not-be-turn"));
    }

    #[test]
    fn markdown_fallback_keeps_note_blockquote_as_text() {
        let session = load_markdown(
            "\
### Assistant

> **Note**: this stays text

> **Write**: `src/store.py`

kept
",
        );
        assert_eq!(
            session.turns,
            vec![Turn {
                index: 0,
                role: TurnRole::Assistant,
                text: "> **Note**: this stays text\n\n\nkept".into(),
            }]
        );
        assert_eq!(session.tool_lines, vec![(1, vec!["Write".into()])]);
    }

    fn user_turn(index: usize) -> Turn {
        Turn {
            index,
            role: TurnRole::User,
            text: "x".repeat(20),
        }
    }

    fn line(index: usize) -> String {
        format!("[T{index}] USER: {}", "x".repeat(20))
    }

    #[test]
    fn window_splits_on_turn_boundaries_with_overlap() {
        let with_tools = SessionInput {
            session_id: "s".into(),
            date: "2026-09-28".into(),
            project: empty_project(),
            turns: vec![
                Turn {
                    index: 0,
                    role: TurnRole::User,
                    text: "hi".into(),
                },
                Turn {
                    index: 1,
                    role: TurnRole::Assistant,
                    text: "done".into(),
                },
            ],
            tool_lines: vec![
                (1, vec!["Write".into(), "Bash".into()]),
                (2, vec!["Read".into()]),
            ],
        };
        assert_eq!(
            window(&with_tools, 48_000),
            vec![
                "[T0] USER: hi\n(assistant used tools: Write, Bash)\n[T1] ASSISTANT: done\n(assistant used tools: Read)"
                    .to_string()
            ]
        );

        let turns: Vec<Turn> = (0..6).map(user_turn).collect();
        let session = SessionInput {
            session_id: "s".into(),
            date: "2026-09-28".into(),
            project: empty_project(),
            turns,
            tool_lines: Vec::new(),
        };
        let one_line = line(0).chars().count();
        let three = one_line * 3 + 2;
        let full = one_line * 6 + 5;
        assert_eq!(
            window(&session, full),
            vec![(0..6).map(line).collect::<Vec<_>>().join("\n")]
        );
        assert!(window(&session, three)
            .iter()
            .all(|item| item.chars().count() <= three));

        let windows = window(&session, three);
        assert_eq!(
            windows,
            vec![
                [line(0), line(1), line(2)].join("\n"),
                [line(1), line(2), line(3)].join("\n"),
                [line(2), line(3), line(4)].join("\n"),
                [line(3), line(4), line(5)].join("\n"),
            ]
        );
    }

    fn archive_fixture(memory: &Path, path: &Path, session_id: &str) {
        let conversation = crate::jsonl::parse_transcript(&path.to_string_lossy(), session_id)
            .unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));
        let cwd = crate::archive::resolve_cwd(None, Some(path), None);
        crate::archive::archive_conversation_with_cwd(
            memory,
            &conversation,
            &crate::summarize::ConversationSummary::default(),
            "jsonl",
            cwd.as_deref(),
        )
        .unwrap_or_else(|err| panic!("archive {}: {err}", path.display()));
    }

    fn prepare_memory(root: &Path) -> std::path::PathBuf {
        let memory = root.join("memory");
        fs::create_dir_all(memory.join("conversations")).unwrap();
        memory
    }

    #[test]
    fn block_and_string_forms_give_identical_windows() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/writes/linkshelf");
        let block_root = tempfile::tempdir().unwrap();
        let string_root = tempfile::tempdir().unwrap();
        let block_memory = prepare_memory(block_root.path());
        let string_memory = prepare_memory(string_root.path());

        let mut block_files = fs::read_dir(root.join("sessions-blocks"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect::<Vec<_>>();
        let mut string_files = fs::read_dir(root.join("sessions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect::<Vec<_>>();
        block_files.sort();
        string_files.sort();
        assert_eq!(block_files.len(), 14);
        assert_eq!(string_files.len(), 14);

        for (index, (block, string)) in block_files.iter().zip(&string_files).enumerate() {
            let number = index + 1;
            archive_fixture(&block_memory, block, &format!("block-{number:02}"));
            archive_fixture(&string_memory, string, &format!("string-{number:02}"));
            let from_blocks = load_session(&block_memory, number as u32).unwrap();
            let from_strings = load_session(&string_memory, number as u32).unwrap();
            assert_eq!(
                window(&from_blocks, 48_000),
                window(&from_strings, 48_000),
                "session {number}"
            );
        }

        let expected_dates = [
            "2026-09-28",
            "2026-09-29",
            "2026-09-30",
            "2026-10-02",
            "2026-10-03",
            "2026-10-05",
            "2026-10-06",
            "2026-10-08",
            "2026-10-09",
            "2026-10-11",
            "2026-10-12",
            "2026-10-13",
            "2026-10-15",
            "2026-10-16",
        ];
        let dates = (1..=14)
            .map(|number| load_session(&block_memory, number).unwrap().date)
            .collect::<Vec<_>>();
        assert_eq!(dates, expected_dates);

        let archives = fs::read_dir(block_memory.join("conversations"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(archives.len(), 14);
        assert!(archives.iter().all(|name| name.ends_with(".md")));
        let sidecars = fs::read_dir(block_memory.join("turns"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(sidecars.len(), 14);
        assert!(sidecars.iter().all(|name| name.ends_with(".json")));

        let first =
            fs::read_to_string(block_memory.join("conversations/conversation-001.md")).unwrap();
        assert!(first.contains("Python 3.12, managed with uv"), "{first}");
        let sidecar: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(block_memory.join("turns/conversation-001.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar["cwd"], "/work/linkshelf");
        assert_eq!(sidecar["project"]["name"], "linkshelf");
    }
}
