/// YAML frontmatter for archive logs.
///
/// Parses and renders a minimal subset: log number, date, trigger, context, topics.
/// No external YAML dependency — hand-rolled for the fixed schema.

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub log: u32,
    pub date: String,
    pub trigger: String,
    pub context: String,
    pub topics: Vec<String>,
}

impl Frontmatter {
    pub fn render(&self) -> String {
        let topics = if self.topics.is_empty() {
            "[]".to_string()
        } else {
            let items: Vec<String> = self.topics.iter().map(|t| format!("\"{t}\"")).collect();
            format!("[{}]", items.join(", "))
        };

        let ctx = &self.context;
        format!(
            "---\nlog: {}\ndate: \"{}\"\ntrigger: {}\ncontext: \"{ctx}\"\ntopics: {topics}\n---",
            self.log, self.date, self.trigger,
        )
    }
}

/// Parse frontmatter from file content. Returns None if no valid frontmatter found.
pub fn parse(content: &str) -> Option<Frontmatter> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }

    let after_open = &content[3..];
    let end = after_open.find("\n---")?;
    let block = &after_open[..end];

    let mut log: Option<u32> = None;
    let mut date = String::new();
    let mut trigger = String::new();
    let mut context = String::new();
    let mut topics: Vec<String> = Vec::new();

    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "log" => log = val.parse().ok(),
                "date" => date = val.trim_matches('"').to_string(),
                "trigger" => trigger = val.trim_matches('"').to_string(),
                "context" => context = val.trim_matches('"').to_string(),
                "topics" => {
                    let inner = val.trim_start_matches('[').trim_end_matches(']');
                    if !inner.is_empty() {
                        topics = inner
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
                _ => {}
            }
        }
    }

    Some(Frontmatter {
        log: log?,
        date,
        trigger,
        context,
        topics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_parse_roundtrip() {
        let fm = Frontmatter {
            log: 5,
            date: "2026-02-24T21:30:00Z".to_string(),
            trigger: "precompact".to_string(),
            context: "working on recall-echo".to_string(),
            topics: vec!["rust".to_string(), "memory".to_string()],
        };

        let rendered = fm.render();
        let parsed = parse(&rendered).expect("should parse");
        assert_eq!(parsed.log, 5);
        assert_eq!(parsed.date, "2026-02-24T21:30:00Z");
        assert_eq!(parsed.trigger, "precompact");
        assert_eq!(parsed.context, "working on recall-echo");
        assert_eq!(parsed.topics, vec!["rust", "memory"]);
    }

    #[test]
    fn parse_empty_topics() {
        let input = "---\nlog: 1\ndate: \"2026-01-01\"\ntrigger: session-end\ncontext: \"\"\ntopics: []\n---";
        let fm = parse(input).expect("should parse");
        assert!(fm.topics.is_empty());
    }

    #[test]
    fn parse_missing_frontmatter() {
        let input = "# Just a markdown file\n\nNo frontmatter here.";
        assert!(parse(input).is_none());
    }

    #[test]
    fn parse_malformed_frontmatter() {
        let input = "---\nthis is not yaml at all\n---";
        // Should return None because log field is missing (required)
        assert!(parse(input).is_none());
    }

    #[test]
    fn render_empty_topics() {
        let fm = Frontmatter {
            log: 1,
            date: "2026-01-01".to_string(),
            trigger: "session-end".to_string(),
            context: "".to_string(),
            topics: vec![],
        };
        let rendered = fm.render();
        assert!(rendered.contains("topics: []"));
    }
}
