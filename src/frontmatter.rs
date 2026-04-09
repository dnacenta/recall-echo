/// YAML frontmatter for conversation archives.
///
/// Parses and renders a minimal subset: log number, date, session_id,
/// message_count, duration, source, topics.
/// No external YAML dependency — hand-rolled for the fixed schema.

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub log: u32,
    pub date: String,
    pub session_id: String,
    pub message_count: u32,
    pub duration: String,
    pub source: String,
    pub topics: Vec<String>,
}

impl Frontmatter {
    #[must_use]
    pub fn render(&self) -> String {
        let topics = if self.topics.is_empty() {
            "[]".to_string()
        } else {
            let items: Vec<String> = self.topics.iter().map(|t| format!("\"{t}\"")).collect();
            format!("[{}]", items.join(", "))
        };

        format!(
            "---\nlog: {}\ndate: \"{}\"\nsession_id: \"{}\"\nmessage_count: {}\nduration: \"{}\"\nsource: \"{}\"\ntopics: {}\n---",
            self.log, self.date, self.session_id, self.message_count, self.duration, self.source, topics
        )
    }
}

/// Parse frontmatter from file content. Returns None if no valid frontmatter found.
#[must_use]
pub fn parse(content: &str) -> Option<Frontmatter> {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return None;
    }

    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    let block = &after_first[..end];

    let mut log = None;
    let mut date = None;
    let mut session_id = None;
    let mut message_count = None;
    let mut duration = None;
    let mut source = None;
    let mut topics = Vec::new();

    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, val) = line.split_once(':')?;
        let key = key.trim();
        let val = val.trim().trim_matches('"');

        match key {
            "log" => log = val.parse().ok(),
            "date" => date = Some(val.to_string()),
            "session_id" => session_id = Some(val.to_string()),
            "message_count" => message_count = val.parse().ok(),
            "duration" => duration = Some(val.to_string()),
            "source" => source = Some(val.to_string()),
            "topics" => {
                let inner = val.trim_matches(|c| c == '[' || c == ']');
                if !inner.is_empty() {
                    topics = inner
                        .split(',')
                        .map(|t| t.trim().trim_matches('"').to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
            }
            _ => {}
        }
    }

    Some(Frontmatter {
        log: log?,
        date: date?,
        session_id: session_id.unwrap_or_default(),
        message_count: message_count.unwrap_or(0),
        duration: duration.unwrap_or_default(),
        source: source.unwrap_or_default(),
        topics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_parse_roundtrip() {
        let fm = Frontmatter {
            log: 42,
            date: "2026-03-05T14:30:00Z".to_string(),
            session_id: "abc123".to_string(),
            message_count: 34,
            duration: "45m".to_string(),
            source: "jsonl".to_string(),
            topics: vec!["auth".to_string(), "JWT".to_string()],
        };
        let rendered = fm.render();
        let parsed = parse(&rendered).unwrap();
        assert_eq!(fm, parsed);
    }

    #[test]
    fn render_empty_topics() {
        let fm = Frontmatter {
            log: 1,
            date: "2026-03-05T00:00:00Z".to_string(),
            session_id: "xyz".to_string(),
            message_count: 0,
            duration: "< 1m".to_string(),
            source: "jsonl".to_string(),
            topics: vec![],
        };
        let rendered = fm.render();
        assert!(rendered.contains("topics: []"));
        let parsed = parse(&rendered).unwrap();
        assert_eq!(parsed.topics, Vec::<String>::new());
    }

    #[test]
    fn parse_missing_frontmatter() {
        assert!(parse("no frontmatter here").is_none());
    }

    #[test]
    fn parse_malformed_frontmatter() {
        assert!(parse("---\nlog: abc\n---").is_none());
    }
}
