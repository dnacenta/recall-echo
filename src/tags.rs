//! Structured tag extraction from conversations.
//!
//! Extracts decisions, action items, project references, files touched,
//! and tools used from conversation entries.

use std::fmt::Write as _;

use crate::conversation::ConversationEntry;

/// Structured tags extracted from a conversation.
#[derive(Debug, Clone, Default)]
pub struct ConversationTags {
    pub decisions: Vec<String>,
    pub action_items: Vec<String>,
    pub project: Option<String>,
    pub files_touched: Vec<String>,
    pub tools_used: Vec<String>,
}

impl ConversationTags {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
            && self.action_items.is_empty()
            && self.project.is_none()
            && self.files_touched.is_empty()
            && self.tools_used.is_empty()
    }
}

const DECISION_MARKERS: &[&str] = &[
    "decided to",
    "decision:",
    "we'll go with",
    "going with",
    "let's use",
    "chose to",
    "choosing",
    "settled on",
    "agreed on",
    "switched to",
    "instead of",
    "rather than",
    "the approach is",
    "plan is to",
];

const ACTION_MARKERS: &[&str] = &[
    "todo:",
    "todo -",
    "action item:",
    "next step:",
    "need to",
    "needs to",
    "should be",
    "will need",
    "follow up",
    "follow-up",
    "remaining:",
    "still need",
    "don't forget",
    "remember to",
    "make sure to",
];

/// Extract structured tags from flattened conversation entries.
#[must_use]
pub fn extract_tags(entries: &[ConversationEntry]) -> ConversationTags {
    let mut tags = ConversationTags::default();
    let mut tool_set = std::collections::HashSet::new();
    let mut file_set = std::collections::HashSet::new();

    for entry in entries {
        match entry {
            ConversationEntry::UserMessage(text) | ConversationEntry::AssistantText(text) => {
                extract_decisions(text, &mut tags.decisions);
                extract_action_items(text, &mut tags.action_items);
                if tags.project.is_none() {
                    tags.project = detect_project(text);
                }
            }
            ConversationEntry::ToolUse {
                name,
                input_summary,
            } => {
                tool_set.insert(name.clone());
                let summary = input_summary.trim_matches('`');
                if !summary.is_empty() && (summary.contains('/') || summary.contains('.')) {
                    file_set.insert(summary.to_string());
                }
            }
            ConversationEntry::ToolResult { .. } => {}
        }
    }

    tags.tools_used = tool_set.into_iter().collect();
    tags.tools_used.sort();
    tags.files_touched = file_set.into_iter().collect();
    tags.files_touched.sort();

    tags.decisions.truncate(5);
    tags.action_items.truncate(5);
    tags.files_touched.truncate(10);

    tags
}

fn extract_decisions(text: &str, decisions: &mut Vec<String>) {
    let lower = text.to_lowercase();
    for marker in DECISION_MARKERS {
        if let Some(pos) = lower.find(marker) {
            let start = text[..pos].rfind(['.', '\n']).map(|p| p + 1).unwrap_or(pos);
            let end_offset = pos + marker.len();
            let end = text[end_offset..]
                .find(['.', '\n'])
                .map(|p| end_offset + p + 1)
                .unwrap_or(text.len().min(end_offset + 100));
            let sentence = text[start..end].trim();
            if sentence.len() >= 10 && sentence.len() <= 200 && decisions.len() < 5 {
                let sentence_lower = sentence.to_lowercase();
                if !decisions.iter().any(|d| d.to_lowercase() == sentence_lower) {
                    decisions.push(sentence.to_string());
                }
            }
        }
    }
}

fn extract_action_items(text: &str, actions: &mut Vec<String>) {
    let lower = text.to_lowercase();
    for marker in ACTION_MARKERS {
        if let Some(pos) = lower.find(marker) {
            let end_offset = pos + marker.len();
            let end = text[end_offset..]
                .find(['.', '\n'])
                .map(|p| end_offset + p + 1)
                .unwrap_or(text.len().min(end_offset + 100));
            let item = text[pos..end].trim();
            if item.len() >= 5 && item.len() <= 200 && actions.len() < 5 {
                let item_lower = item.to_lowercase();
                if !actions.iter().any(|a| a.to_lowercase() == item_lower) {
                    actions.push(item.to_string());
                }
            }
        }
    }
}

fn detect_project(text: &str) -> Option<String> {
    let patterns = ["project:", "repo:", "repository:", "working on", "in the"];
    let lower = text.to_lowercase();

    for pattern in &patterns {
        if let Some(pos) = lower.find(pattern) {
            let after = &text[pos + pattern.len()..];
            let word: String = after
                .trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if word.len() >= 2 {
                return Some(word);
            }
        }
    }

    None
}

/// Format tags as a markdown section for inclusion in conversation archives.
#[must_use]
pub fn format_tags_section(tags: &ConversationTags) -> String {
    if tags.is_empty() {
        return String::new();
    }

    let mut section = String::from("\n## Tags\n");

    if let Some(ref project) = tags.project {
        let _ = write!(section, "\n**Project**: {project}\n");
    }

    if !tags.decisions.is_empty() {
        section.push_str("\n**Decisions**:\n");
        for d in &tags.decisions {
            let _ = writeln!(section, "- {d}");
        }
    }

    if !tags.action_items.is_empty() {
        section.push_str("\n**Action Items**:\n");
        for a in &tags.action_items {
            let _ = writeln!(section, "- {a}");
        }
    }

    if !tags.files_touched.is_empty() {
        section.push_str("\n**Files**: ");
        section.push_str(&tags.files_touched.join(", "));
        section.push('\n');
    }

    if !tags.tools_used.is_empty() {
        section.push_str("\n**Tools**: ");
        section.push_str(&tags.tools_used.join(", "));
        section.push('\n');
    }

    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_decisions_basic() {
        let entries = vec![ConversationEntry::AssistantText(
            "After reviewing the options, I decided to use JWT tokens instead of session cookies."
                .to_string(),
        )];
        let tags = extract_tags(&entries);
        assert!(!tags.decisions.is_empty());
        assert!(tags.decisions[0].contains("JWT"));
    }

    #[test]
    fn extract_action_items_basic() {
        let entries = vec![ConversationEntry::AssistantText(
            "The auth module works now. Still need to add rate limiting to the API endpoints."
                .to_string(),
        )];
        let tags = extract_tags(&entries);
        assert!(!tags.action_items.is_empty());
        assert!(tags.action_items[0].contains("rate limiting"));
    }

    #[test]
    fn extract_tools_and_files() {
        let entries = vec![
            ConversationEntry::ToolUse {
                name: "Read".to_string(),
                input_summary: "/src/auth.rs".to_string(),
            },
            ConversationEntry::ToolUse {
                name: "Edit".to_string(),
                input_summary: "/src/config.rs".to_string(),
            },
            ConversationEntry::ToolUse {
                name: "Read".to_string(),
                input_summary: "/src/main.rs".to_string(),
            },
        ];
        let tags = extract_tags(&entries);
        assert_eq!(tags.tools_used, vec!["Edit", "Read"]);
        assert_eq!(tags.files_touched.len(), 3);
    }

    #[test]
    fn empty_entries_empty_tags() {
        let tags = extract_tags(&[]);
        assert!(tags.is_empty());
    }

    #[test]
    fn format_tags_section_basic() {
        let tags = ConversationTags {
            decisions: vec!["Use JWT instead of sessions".to_string()],
            action_items: vec!["Still need to add rate limiting".to_string()],
            project: Some("voice-echo".to_string()),
            files_touched: vec!["/src/auth.rs".to_string()],
            tools_used: vec!["Edit".to_string(), "Read".to_string()],
        };
        let section = format_tags_section(&tags);
        assert!(section.contains("## Tags"));
        assert!(section.contains("**Project**: voice-echo"));
        assert!(section.contains("**Decisions**:"));
        assert!(section.contains("JWT"));
    }

    #[test]
    fn format_empty_tags_returns_empty() {
        let tags = ConversationTags::default();
        assert!(format_tags_section(&tags).is_empty());
    }
}
