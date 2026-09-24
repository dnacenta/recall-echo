// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// YAML frontmatter for conversation archives.
///
/// Parses and renders a minimal subset: log number, date, session_id,
/// message_count, duration, source, pulse, topics.
/// No external YAML dependency — hand-rolled for the fixed schema.
///
/// `pulse` names the pulse-null pulse whose session this is. It is written as
/// `pulse:`; the pre-4.6.0 key `entity:` — which pulse-null's own archive and
/// quarantine writers used — is still read, so older logs parse unchanged.

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub log: u32,
    pub date: String,
    pub session_id: String,
    pub message_count: u32,
    pub duration: String,
    pub source: String,
    /// The pulse the session belongs to, when the writer knows it.
    pub pulse: Option<String>,
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

        let pulse = self
            .pulse
            .as_deref()
            .map(|name| format!("pulse: \"{}\"\n", sanitize_value(name)))
            .unwrap_or_default();

        format!(
            "---\nlog: {}\ndate: \"{}\"\nsession_id: \"{}\"\nmessage_count: {}\nduration: \"{}\"\nsource: \"{}\"\n{pulse}topics: {}\n---",
            self.log, self.date, self.session_id, self.message_count, self.duration, self.source, topics
        )
    }
}

/// A pulse name as a single-line, quote-free frontmatter value: the parser
/// splits on lines and trims quotes, so neither may appear inside it.
fn sanitize_value(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .collect()
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
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
    let mut pulse = None;
    let mut legacy_pulse = None;
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
            "pulse" => pulse = non_empty(val),
            "entity" => legacy_pulse = non_empty(val),
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
        pulse: pulse.or(legacy_pulse),
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
            pulse: None,
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
            pulse: None,
            topics: vec![],
        };
        let rendered = fm.render();
        assert!(rendered.contains("topics: []"));
        let parsed = parse(&rendered).unwrap();
        assert_eq!(parsed.topics, Vec::<String>::new());
    }

    #[test]
    fn render_writes_the_pulse_key_and_parse_reads_it_back() {
        let fm = Frontmatter {
            log: 7,
            date: "2026-09-24T12:00:00Z".to_string(),
            session_id: "s7".to_string(),
            message_count: 4,
            duration: "3m".to_string(),
            source: "session".to_string(),
            pulse: Some("Echo".to_string()),
            topics: vec![],
        };
        let rendered = fm.render();
        assert!(rendered.contains("\npulse: \"Echo\"\n"), "{rendered}");
        assert!(!rendered.contains("entity:"), "{rendered}");
        assert_eq!(parse(&rendered).unwrap(), fm);
    }

    #[test]
    fn render_omits_the_pulse_key_when_unknown() {
        let fm = Frontmatter {
            log: 1,
            date: "2026-09-24T12:00:00Z".to_string(),
            session_id: "s1".to_string(),
            message_count: 0,
            duration: "< 1m".to_string(),
            source: "jsonl".to_string(),
            pulse: None,
            topics: vec![],
        };
        assert!(!fm.render().contains("pulse:"));
    }

    #[test]
    fn render_keeps_a_hostile_pulse_name_on_one_line() {
        let fm = Frontmatter {
            log: 1,
            date: "d".to_string(),
            session_id: String::new(),
            message_count: 0,
            duration: String::new(),
            source: String::new(),
            pulse: Some("Ec\"ho\nlog: 999".to_string()),
            topics: vec![],
        };
        let parsed = parse(&fm.render()).unwrap();
        assert_eq!(parsed.log, 1);
        assert_eq!(parsed.pulse.as_deref(), Some("Echolog: 999"));
    }

    /// pulse-null's conversation log before 4.6.0: `entity:`, plus keys this
    /// parser does not model (`trigger`, `channel`, `peer`).
    #[test]
    fn parse_reads_the_legacy_entity_key() {
        let content = "---\nlog: 12\ndate: \"2026-09-01T10:00:00Z\"\ntrigger: session-end\n\
                       channel: discord\nentity: \"Echo\"\npeer: \"Synth\"\nmessage_count: 6\n---\n\n# Conversation 012\n";
        let fm = parse(content).unwrap();
        assert_eq!(fm.log, 12);
        assert_eq!(fm.pulse.as_deref(), Some("Echo"));
        assert_eq!(fm.message_count, 6);
    }

    /// pulse-null's quarantine log carries no `log:` — it is not an archive
    /// log, and the parser must keep refusing it whichever key names the pulse.
    #[test]
    fn parse_refuses_a_quarantine_log_under_either_key() {
        for key in ["entity", "pulse"] {
            let content = format!(
                "---\ndate: \"2026-09-01\"\n{key}: \"Echo\"\nsession_key: \"k\"\nlane: quarantine\n---\n"
            );
            assert!(parse(&content).is_none(), "{key}");
        }
    }

    #[test]
    fn parse_prefers_pulse_over_entity_whatever_the_order() {
        for block in [
            "entity: \"Old\"\npulse: \"New\"",
            "pulse: \"New\"\nentity: \"Old\"",
        ] {
            let content = format!("---\nlog: 1\ndate: \"d\"\n{block}\n---");
            assert_eq!(parse(&content).unwrap().pulse.as_deref(), Some("New"));
        }
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
