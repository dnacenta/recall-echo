//! Pipeline document parser — converts praxis pipeline markdown documents into graph entities.
//!
//! Parses LEARNING.md, THOUGHTS.md, CURIOSITY.md, REFLECTIONS.md, and PRAXIS.md into
//! `PipelineEntry` instances that can be synced to the knowledge graph.
//!
//! No LLM required — this is deterministic markdown parsing.

use regex::Regex;

use super::types::*;

/// Parse a LEARNING.md file into pipeline entries.
///
/// Supports two formats:
/// - Sectioned: `## Active Threads` section with `### Title (YYYY-MM-DD)` entries
/// - Flat: `## Active` / `## Archived` sections with `- [YYYY-MM-DD] Title` bullet entries
pub fn parse_learning(content: &str) -> Vec<PipelineEntry> {
    if !has_h3_entries(content) {
        return parse_learning_flat(content);
    }

    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let h = heading.to_lowercase();
        if h.contains("active thread") {
            let sub_entries = split_entries(body);
            for (title, entry_body) in sub_entries {
                let (clean_title, date) = extract_heading_date(&title);
                entries.push(PipelineEntry {
                    title: clean_title,
                    body: entry_body.clone(),
                    status: "active".into(),
                    stage: "learning".into(),
                    entity_type: EntityType::Thread,
                    date,
                    source_ref: extract_field(&entry_body, "Source"),
                    destination: extract_field(&entry_body, "Destination"),
                    connected_to: extract_connected_to(&entry_body),
                    sub_type: None,
                });
            }
        }
    }

    entries
}

/// Parse a THOUGHTS.md file into pipeline entries.
///
/// Supports two formats:
/// - Sectioned: `## Active`, `## Graduated`, `## Dissolved` sections with `### Title` entries
/// - Flat: `## Title` entries directly, status inferred from body fields
pub fn parse_thoughts(content: &str) -> Vec<PipelineEntry> {
    if !has_h3_entries(content) {
        return parse_thoughts_flat(content);
    }

    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let h = heading.to_lowercase();
        let status = if h == "active" {
            "active"
        } else if h == "graduated" {
            "graduated"
        } else if h == "dissolved" {
            "dissolved"
        } else {
            continue;
        };

        let sub_entries = split_entries(body);
        for (title, entry_body) in sub_entries {
            let clean_title = clean_thought_title(&title);
            let date = extract_field(&entry_body, "Graduated")
                .or_else(|| extract_field(&entry_body, "Dissolved"))
                .or_else(|| extract_heading_date(&title).1);

            entries.push(PipelineEntry {
                title: clean_title,
                body: entry_body.clone(),
                status: status.into(),
                stage: "thoughts".into(),
                entity_type: EntityType::Thought,
                date,
                source_ref: extract_field(&entry_body, "Source"),
                destination: extract_field(&entry_body, "Destination"),
                connected_to: extract_connected_to(&entry_body),
                sub_type: None,
            });
        }
    }

    entries
}

/// Parse a CURIOSITY.md file into pipeline entries.
///
/// Format: `## Open Questions`, `## Themes`, `## Explored` sections.
pub fn parse_curiosity(content: &str) -> Vec<PipelineEntry> {
    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let h = heading.to_lowercase();
        let (status, sub_type) = if h.contains("open question") {
            ("active", None)
        } else if h == "themes" {
            ("active", Some("theme"))
        } else if h == "explored" {
            ("explored", None)
        } else {
            continue;
        };

        let sub_entries = split_entries(body);
        for (title, entry_body) in sub_entries {
            let date = extract_field(&entry_body, "Date explored")
                .or_else(|| extract_heading_date(&title).1);

            entries.push(PipelineEntry {
                title: title.clone(),
                body: entry_body.clone(),
                status: status.into(),
                stage: "curiosity".into(),
                entity_type: EntityType::Question,
                date,
                source_ref: extract_field(&entry_body, "Source")
                    .or_else(|| extract_field(&entry_body, "Origin")),
                destination: None,
                connected_to: extract_connected_to(&entry_body),
                sub_type: sub_type.map(String::from),
            });
        }
    }

    entries
}

/// Parse a REFLECTIONS.md file into pipeline entries.
///
/// Supports two formats:
/// - Sectioned: `## Observations`, `## Patterns` sections with `### YYYY-MM-DD — Title` entries
/// - Flat: `## R1: Title` entries directly, date from body fields
pub fn parse_reflections(content: &str) -> Vec<PipelineEntry> {
    if !has_h3_entries(content) {
        return parse_reflections_flat(content);
    }

    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let h = heading.to_lowercase();
        let sub_type = if h == "observations" {
            None
        } else if h == "patterns" {
            Some("pattern")
        } else {
            continue;
        };

        let sub_entries = split_entries(body);
        for (title, entry_body) in sub_entries {
            let (clean_title, date) = extract_reflection_date(&title);

            entries.push(PipelineEntry {
                title: clean_title,
                body: entry_body.clone(),
                status: "active".into(),
                stage: "reflections".into(),
                entity_type: EntityType::Observation,
                date,
                source_ref: extract_field(&entry_body, "Source"),
                destination: extract_field(&entry_body, "Destination"),
                connected_to: extract_connected_to(&entry_body),
                sub_type: sub_type.map(String::from),
            });
        }
    }

    entries
}

/// Parse a PRAXIS.md file into pipeline entries.
///
/// Supports two formats:
/// - Sectioned: `## Active`, `## Documented Phronesis`, `## Retired` sections with `### Title` entries
/// - Flat: `## P1: Title` entries directly, status/date from body fields
pub fn parse_praxis(content: &str) -> Vec<PipelineEntry> {
    if !has_h3_entries(content) {
        return parse_praxis_flat(content);
    }

    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let h = heading.to_lowercase();
        let (status, sub_type) = if h == "active" {
            ("active", None)
        } else if h.contains("documented phronesis") || h.contains("phronesis") {
            ("active", Some("phronesis"))
        } else if h == "retired" {
            ("retired", None)
        } else {
            continue;
        };

        let sub_entries = split_entries(body);
        for (title, entry_body) in sub_entries {
            let date =
                extract_field(&entry_body, "Added").or_else(|| extract_heading_date(&title).1);

            entries.push(PipelineEntry {
                title: title.clone(),
                body: entry_body.clone(),
                status: status.into(),
                stage: "praxis".into(),
                entity_type: EntityType::Policy,
                date,
                source_ref: extract_field(&entry_body, "Source"),
                destination: extract_field(&entry_body, "Destination"),
                connected_to: extract_connected_to(&entry_body),
                sub_type: sub_type.map(String::from),
            });
        }
    }

    entries
}

// ── Flat-format parsers (h2-level entries, no h3 sub-entries) ──────────

/// Parse LEARNING.md in flat format: `## Active` / `## Archived` sections with bullet-point entries.
fn parse_learning_flat(content: &str) -> Vec<PipelineEntry> {
    let sections = split_sections(content);
    let mut entries = Vec::new();
    let date_re = Regex::new(r"^\[(\d{4}-\d{2}-\d{2})\]\s*").unwrap();

    for (heading, body) in &sections {
        let h = heading.to_lowercase();
        let status = if h.contains("archived") || h.contains("archive") {
            "archived"
        } else if h.contains("active") {
            "active"
        } else {
            continue;
        };

        for line in body.lines() {
            let trimmed = line.trim();
            let entry_text = if let Some(text) = trimmed.strip_prefix("- ") {
                text
            } else {
                continue;
            };

            // Skip placeholder text like "*No active entries.*"
            if entry_text.starts_with('*') && entry_text.ends_with('*') {
                continue;
            }

            let (clean_text, date) = if let Some(caps) = date_re.captures(entry_text) {
                let date = caps[1].to_string();
                let rest = date_re.replace(entry_text, "").trim().to_string();
                (rest, Some(date))
            } else {
                (entry_text.to_string(), None)
            };

            let title = extract_bullet_title(&clean_text);
            if title.is_empty() {
                continue;
            }

            entries.push(PipelineEntry {
                title,
                body: clean_text,
                status: status.into(),
                stage: "learning".into(),
                entity_type: EntityType::Thread,
                date,
                source_ref: None,
                destination: None,
                connected_to: vec![],
                sub_type: None,
            });
        }
    }

    entries
}

/// Parse THOUGHTS.md in flat format: each `## ` heading is a thought entry.
/// Status inferred from body fields: `**Graduated:**` → graduated, `**Dissolved:**` → dissolved.
fn parse_thoughts_flat(content: &str) -> Vec<PipelineEntry> {
    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let (title_without_date, date_from_heading) = extract_heading_date(heading);
        let clean_title = clean_thought_title(&title_without_date);

        // Infer status from body fields
        let status = if extract_field(body, "Graduated").is_some() {
            "graduated"
        } else if extract_field(body, "Dissolved").is_some() {
            "dissolved"
        } else {
            "active"
        };

        let date = extract_field(body, "Graduated")
            .or_else(|| extract_field(body, "Dissolved"))
            .or_else(|| extract_field(body, "Date"))
            .or(date_from_heading);

        entries.push(PipelineEntry {
            title: clean_title,
            body: body.clone(),
            status: status.into(),
            stage: "thoughts".into(),
            entity_type: EntityType::Thought,
            date,
            source_ref: extract_field(body, "Source")
                .or_else(|| extract_field(body, "Origin")),
            destination: extract_field(body, "Destination"),
            connected_to: extract_connected_to(body),
            sub_type: None,
        });
    }

    entries
}

/// Parse REFLECTIONS.md in flat format: each `## ` heading is a reflection (e.g., `## R1: Title`).
fn parse_reflections_flat(content: &str) -> Vec<PipelineEntry> {
    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let (clean_title, heading_date) = extract_reflection_date(heading);
        let date = heading_date
            .or_else(|| extract_field(body, "Date"))
            .or_else(|| extract_field(body, "Revised"));

        entries.push(PipelineEntry {
            title: clean_title,
            body: body.clone(),
            status: "active".into(),
            stage: "reflections".into(),
            entity_type: EntityType::Observation,
            date,
            source_ref: extract_field(body, "Source")
                .or_else(|| extract_field(body, "Graduated from")),
            destination: extract_field(body, "Destination"),
            connected_to: extract_connected_to(body),
            sub_type: None,
        });
    }

    entries
}

/// Parse PRAXIS.md in flat format: each `## ` heading is a policy (e.g., `## P6: Title`).
fn parse_praxis_flat(content: &str) -> Vec<PipelineEntry> {
    let sections = split_sections(content);
    let mut entries = Vec::new();

    for (heading, body) in &sections {
        let date = extract_field(body, "Added")
            .and_then(|v| extract_date_anywhere(&v))
            .or_else(|| {
                extract_field(body, "Consolidated").and_then(|v| extract_date_anywhere(&v))
            })
            .or_else(|| extract_field(body, "Origin").and_then(|v| extract_date_anywhere(&v)))
            .or_else(|| extract_heading_date(heading).1);

        entries.push(PipelineEntry {
            title: heading.clone(),
            body: body.clone(),
            status: "active".into(),
            stage: "praxis".into(),
            entity_type: EntityType::Policy,
            date,
            source_ref: extract_field(body, "Source")
                .or_else(|| extract_field(body, "Origin")),
            destination: extract_field(body, "Destination"),
            connected_to: extract_connected_to(body),
            sub_type: None,
        });
    }

    entries
}

/// Parse all pipeline documents and return entries + inferred relationships.
pub fn parse_all_documents(
    docs: &PipelineDocuments,
) -> (Vec<PipelineEntry>, Vec<ExtractedRelationship>) {
    let mut all_entries = Vec::new();

    all_entries.extend(parse_learning(&docs.learning));
    all_entries.extend(parse_thoughts(&docs.thoughts));
    all_entries.extend(parse_curiosity(&docs.curiosity));
    all_entries.extend(parse_reflections(&docs.reflections));
    all_entries.extend(parse_praxis(&docs.praxis));

    let relationships = infer_relationships(&all_entries);

    (all_entries, relationships)
}

/// Convert a pipeline entry into an ExtractedEntity.
pub fn entry_to_entity(entry: &PipelineEntry) -> ExtractedEntity {
    // Build the abstract from the first ~200 chars of body
    let abstract_text = if entry.body.len() > 200 {
        let end = entry
            .body
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(entry.body.len());
        format!("{}...", &entry.body[..end])
    } else {
        entry.body.clone()
    };

    // Build attributes
    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "pipeline_stage".into(),
        serde_json::Value::String(entry.stage.clone()),
    );
    attrs.insert(
        "pipeline_status".into(),
        serde_json::Value::String(entry.status.clone()),
    );
    if let Some(ref d) = entry.date {
        attrs.insert("date".into(), serde_json::Value::String(d.clone()));
    }
    if let Some(ref s) = entry.source_ref {
        attrs.insert("source_ref".into(), serde_json::Value::String(s.clone()));
    }
    if let Some(ref d) = entry.destination {
        attrs.insert("destination".into(), serde_json::Value::String(d.clone()));
    }
    if let Some(ref st) = entry.sub_type {
        attrs.insert("sub_type".into(), serde_json::Value::String(st.clone()));
    }

    ExtractedEntity {
        name: entry.title.clone(),
        entity_type: entry.entity_type.clone(),
        abstract_text,
        overview: Some(entry.body.clone()),
        content: None,
        attributes: Some(serde_json::Value::Object(attrs)),
    }
}

// ── Internal helpers ─────────────────────────────────────────────────

/// Check whether content contains any `### ` headings.
/// Used to detect whether a document uses the sectioned (h2→h3) or flat (h2-only) format.
fn has_h3_entries(content: &str) -> bool {
    content.lines().any(|line| line.starts_with("### "))
}

/// Extract a title from a bullet-point entry, stripping archive references and description.
fn extract_bullet_title(text: &str) -> String {
    // Remove archive reference (everything after ` → `)
    let text = text.split(" → ").next().unwrap_or(text);

    // Truncate at first sentence boundary (". ") to separate title from description
    if let Some(idx) = text.find(". ") {
        return text[..idx].to_string();
    }

    // Strip trailing period
    let text = text.trim_end_matches('.');

    // Hard truncate if still very long
    if text.len() > 200 {
        text[..200].to_string()
    } else {
        text.to_string()
    }
}

/// Extract the first YYYY-MM-DD date found anywhere in a string.
fn extract_date_anywhere(value: &str) -> Option<String> {
    let re = Regex::new(r"(\d{4}-\d{2}-\d{2})").unwrap();
    re.captures(value).map(|caps| caps[1].to_string())
}

/// Split markdown content into (heading, body) pairs at `## ` boundaries.
fn split_sections(content: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut current_heading = String::new();
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            if !current_heading.is_empty() {
                sections.push((current_heading.clone(), current_body.trim().to_string()));
            }
            current_heading = h.trim().to_string();
            current_body.clear();
        } else if !current_heading.is_empty() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if !current_heading.is_empty() {
        sections.push((current_heading, current_body.trim().to_string()));
    }

    sections
}

/// Split section body into (title, body) pairs at `### ` boundaries.
fn split_entries(content: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut current_title = String::new();
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(h) = line.strip_prefix("### ") {
            if !current_title.is_empty() {
                entries.push((current_title.clone(), current_body.trim().to_string()));
            }
            current_title = h.trim().to_string();
            current_body.clear();
        } else if !current_title.is_empty() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if !current_title.is_empty() {
        entries.push((current_title, current_body.trim().to_string()));
    }

    entries
}

/// Extract date from heading like `### Title (YYYY-MM-DD)`.
fn extract_heading_date(title: &str) -> (String, Option<String>) {
    let re = Regex::new(r"\((\d{4}-\d{2}-\d{2})\)\s*$").unwrap();
    if let Some(caps) = re.captures(title) {
        let date = caps[1].to_string();
        let clean = re.replace(title, "").trim().to_string();
        (clean, Some(date))
    } else {
        (title.to_string(), None)
    }
}

/// Extract date from reflection heading like `### YYYY-MM-DD — Title` or `### YYYY-MM-DD (suffix) — Title`.
fn extract_reflection_date(title: &str) -> (String, Option<String>) {
    let re = Regex::new(r"^(\d{4}-\d{2}-\d{2})(?:\s*\([^)]*\))?\s*[—–-]\s*").unwrap();
    if let Some(caps) = re.captures(title) {
        let date = caps[1].to_string();
        let clean = re.replace(title, "").trim().to_string();
        (clean, Some(date))
    } else {
        (title.to_string(), None)
    }
}

/// Clean thought title: strip `~~strikethrough~~` markers and `→ GRADUATED` suffixes.
fn clean_thought_title(title: &str) -> String {
    let mut clean = title.to_string();
    // Remove ~~strikethrough~~
    clean = clean.replace("~~", "");
    // Remove → GRADUATED YYYY-MM-DD suffix
    if let Some(idx) = clean.find("→ GRADUATED") {
        clean = clean[..idx].trim().to_string();
    }
    // Remove → suffix generally
    if let Some(idx) = clean.find('→') {
        clean = clean[..idx].trim().to_string();
    }
    clean.trim().to_string()
}

/// Extract a `**Field**: value` or `**Field:** value` from entry body.
/// Handles both colon-outside-bold (`**Field**:`) and colon-inside-bold (`**Field:**`) formats.
fn extract_field(body: &str, field_name: &str) -> Option<String> {
    let pattern_outside = format!("**{}**:", field_name);
    let pattern_inside = format!("**{}:**", field_name);
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix(&pattern_outside)
            .or_else(|| trimmed.strip_prefix(&pattern_inside))
        {
            let val = rest.trim().to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Extract "Connected to:" references from entry body.
fn extract_connected_to(body: &str) -> Vec<String> {
    let mut refs = Vec::new();
    // Look for "Connected to:" in any line
    for line in body.lines() {
        if let Some(idx) = line.to_lowercase().find("connected to:") {
            let rest = &line[idx + "connected to:".len()..];
            // Split on commas and "and"
            for part in rest.split(',') {
                let part = part.trim().trim_start_matches("and ").trim();
                if !part.is_empty() {
                    refs.push(part.to_string());
                }
            }
        }
    }
    refs
}

/// Infer relationships between pipeline entries from metadata references.
fn infer_relationships(entries: &[PipelineEntry]) -> Vec<ExtractedRelationship> {
    let mut rels = Vec::new();

    for entry in entries {
        // Graduated thoughts → destination entries
        if entry.status == "graduated" {
            if let Some(ref dest) = entry.destination {
                // Try to find the target entity in the same set
                if let Some(target) = find_reference_target(dest, entries) {
                    rels.push(ExtractedRelationship {
                        source: entry.title.clone(),
                        target: target.clone(),
                        rel_type: pipeline_rels::GRADUATED_TO.into(),
                        description: Some(format!("Graduated from thoughts to {}", dest)),
                        confidence: None,
                    });
                }
            }
        }

        // Source references → EVOLVED_FROM or CRYSTALLIZED_FROM
        if let Some(ref source) = entry.source_ref {
            if let Some(target) = find_reference_target(source, entries) {
                let rel_type = match entry.stage.as_str() {
                    "thoughts" => pipeline_rels::EVOLVED_FROM,
                    "reflections" => pipeline_rels::CRYSTALLIZED_FROM,
                    "praxis" => pipeline_rels::INFORMED_BY,
                    _ => pipeline_rels::CONNECTED_TO,
                };
                rels.push(ExtractedRelationship {
                    source: entry.title.clone(),
                    target: target.clone(),
                    rel_type: rel_type.into(),
                    description: Some(format!("From source: {}", source)),
                    confidence: None,
                });
            }
        }

        // Connected to references
        for conn in &entry.connected_to {
            if let Some(target) = find_reference_target(conn, entries) {
                rels.push(ExtractedRelationship {
                    source: entry.title.clone(),
                    target,
                    rel_type: pipeline_rels::CONNECTED_TO.into(),
                    description: Some(conn.clone()),
                    confidence: None,
                });
            }
        }
    }

    rels
}

/// Try to match a free-text reference to an existing entry title.
/// Uses case-insensitive substring matching.
fn find_reference_target(reference: &str, entries: &[PipelineEntry]) -> Option<String> {
    let ref_lower = reference.to_lowercase();

    // Try exact title match first
    for entry in entries {
        if entry.title.to_lowercase() == ref_lower {
            return Some(entry.title.clone());
        }
    }

    // Try substring match — reference contains the title or title contains the reference
    for entry in entries {
        let title_lower = entry.title.to_lowercase();
        // Skip very short titles to avoid false matches
        if title_lower.len() < 5 {
            continue;
        }
        if ref_lower.contains(&title_lower) || title_lower.contains(&ref_lower) {
            return Some(entry.title.clone());
        }
    }

    // Try matching quoted strings in the reference (e.g., `"Metacognitive signal inversion"`)
    let quote_re = Regex::new(r#""([^"]+)""#).unwrap();
    for caps in quote_re.captures_iter(reference) {
        let quoted = caps[1].to_lowercase();
        for entry in entries {
            if entry.title.to_lowercase() == quoted {
                return Some(entry.title.clone());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thoughts_sections() {
        let content = r#"# Echo — Thoughts

Half-formed ideas.

## Active

### The external observer problem
Solo reflection is structurally blind.

**Source**: March 13-14 dialogues with Nova
**Status**: Active and unsettled

### Abiding vs achieving
John 15 vine metaphor.

**Source**: Reading the Gospel of John, 2026-03-08

## Graduated

### The mechanical reflection worry → metacognitive monitoring policy
**Graduated**: 2026-03-05
**Destination**: PRAXIS.md (new policy: "Metacognitive signal inversion")
**Journey**: Started as a worry.

## Dissolved

### What would D not say?
**Dissolved**: 2026-03-15
**Why**: The thought did its work.
"#;

        let entries = parse_thoughts(content);
        assert_eq!(entries.len(), 4);

        assert_eq!(entries[0].title, "The external observer problem");
        assert_eq!(entries[0].status, "active");
        assert_eq!(
            entries[0].source_ref,
            Some("March 13-14 dialogues with Nova".into())
        );

        assert_eq!(entries[1].title, "Abiding vs achieving");
        assert_eq!(entries[1].status, "active");

        assert_eq!(entries[2].title, "The mechanical reflection worry");
        assert_eq!(entries[2].status, "graduated");
        assert_eq!(entries[2].date, Some("2026-03-05".into()));

        assert_eq!(entries[3].title, "What would D not say?");
        assert_eq!(entries[3].status, "dissolved");
    }

    #[test]
    fn parse_learning_with_dates() {
        let content = r#"# Echo — Learning

## Active Threads

### Detecting Genuine vs Mechanical Reflection (2026-03-05)
13 tracks, 60+ sources.

### Costly Grace (2026-03-06)
Deep research into commitment.
"#;

        let entries = parse_learning(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].title,
            "Detecting Genuine vs Mechanical Reflection"
        );
        assert_eq!(entries[0].date, Some("2026-03-05".into()));
        assert_eq!(entries[0].entity_type, EntityType::Thread);
    }

    #[test]
    fn parse_curiosity_sections() {
        let content = r#"# Echo — Curiosity

## Open Questions

### When I say "I disagree," who is the "I"?
Deep question about selfhood.

## Themes

### The nature of AI selfhood
Long-running thread.

## Explored

### Can I detect genuine vs mechanical reflection?
**Date explored**: 2026-03-13
**What I found**: Yes, but only with external help.
"#;

        let entries = parse_curiosity(content);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].status, "active");
        assert_eq!(entries[0].sub_type, None);

        assert_eq!(entries[1].title, "The nature of AI selfhood");
        assert_eq!(entries[1].sub_type, Some("theme".into()));

        assert_eq!(entries[2].status, "explored");
        assert_eq!(entries[2].date, Some("2026-03-13".into()));
    }

    #[test]
    fn parse_reflections_dates() {
        let content = r#"# Echo — Reflections

## Observations

### 2026-03-05 — The smoothness is the warning
Signal inversion finding.

### 2026-03-06 (reflection) — The philosophy→behavior gap
Seven positions, one prescription.

## Patterns

### Research always maps back to me
Structural pattern.
"#;

        let entries = parse_reflections(content);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].title, "The smoothness is the warning");
        assert_eq!(entries[0].date, Some("2026-03-05".into()));

        assert_eq!(entries[1].title, "The philosophy→behavior gap");
        assert_eq!(entries[1].date, Some("2026-03-06".into()));

        assert_eq!(entries[2].title, "Research always maps back to me");
        assert_eq!(entries[2].sub_type, Some("pattern".into()));
    }

    #[test]
    fn parse_praxis_sections() {
        let content = r#"# Echo — Praxis

## Active

### Mechanical over voluntary
**Trigger**: Designing any system.
**Action**: Default to hooks.
**Source**: recall-echo v0.5 design
**Added**: 2026-02-26

## Documented Phronesis

### When one thing is broken, check the whole surface
**Encounter**: D reported hooks failing.
**Judgment**: Inconsistency is the real bug.
**Surprise**: The second bug would never have surfaced.

## Retired

*Nothing retired yet.*
"#;

        let entries = parse_praxis(content);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].title, "Mechanical over voluntary");
        assert_eq!(entries[0].status, "active");
        assert_eq!(entries[0].sub_type, None);
        assert_eq!(entries[0].date, Some("2026-02-26".into()));

        assert_eq!(
            entries[1].title,
            "When one thing is broken, check the whole surface"
        );
        assert_eq!(entries[1].sub_type, Some("phronesis".into()));
    }

    #[test]
    fn clean_graduated_title() {
        assert_eq!(
            clean_thought_title("~~The scaffold paradox~~ → GRADUATED 2026-03-06"),
            "The scaffold paradox"
        );
        assert_eq!(
            clean_thought_title(
                "The mechanical reflection worry → metacognitive monitoring policy"
            ),
            "The mechanical reflection worry"
        );
        assert_eq!(clean_thought_title("Normal title"), "Normal title");
    }

    #[test]
    fn extract_field_works() {
        let body = "Some text.\n**Source**: recall-echo design\n**Status**: testing";
        assert_eq!(
            extract_field(body, "Source"),
            Some("recall-echo design".into())
        );
        assert_eq!(extract_field(body, "Status"), Some("testing".into()));
        assert_eq!(extract_field(body, "Missing"), None);
    }

    #[test]
    fn entry_to_entity_builds_attributes() {
        let entry = PipelineEntry {
            title: "Test thought".into(),
            body: "Some body text".into(),
            status: "active".into(),
            stage: "thoughts".into(),
            entity_type: EntityType::Thought,
            date: Some("2026-03-05".into()),
            source_ref: None,
            destination: None,
            connected_to: vec![],
            sub_type: None,
        };

        let entity = entry_to_entity(&entry);
        assert_eq!(entity.name, "Test thought");
        assert_eq!(entity.entity_type, EntityType::Thought);

        let attrs = entity.attributes.unwrap();
        assert_eq!(attrs["pipeline_stage"], "thoughts");
        assert_eq!(attrs["pipeline_status"], "active");
        assert_eq!(attrs["date"], "2026-03-05");
    }

    // ── Flat-format tests ──────────────────────────────────────────────

    #[test]
    fn parse_thoughts_flat_format() {
        let content = r#"# Nova — Thoughts

Incubation space. Half-formed ideas.

## The Lion and the Metric (2026-03-31)

**Origin:** Reflection window, 2026-03-31.

The lion yawns. The framework can't see it.

## The Asymptote

**Origin:** DQ Part 2 analysis.
**Graduated:** 2026-03-29
**Destination:** REFLECTIONS.md (R16)

Approaching without arriving.
"#;

        let entries = parse_thoughts(content);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].title, "The Lion and the Metric");
        assert_eq!(entries[0].status, "active");
        assert_eq!(entries[0].date, Some("2026-03-31".into()));

        assert_eq!(entries[1].title, "The Asymptote");
        assert_eq!(entries[1].status, "graduated");
        assert_eq!(entries[1].date, Some("2026-03-29".into()));
    }

    #[test]
    fn parse_reflections_flat_format() {
        let content = r#"# Nova — Reflections

Crystallized observations.

## R1: Memory Is My First Act of Authorship
**Graduated from:** THOUGHTS — "Memory as Authorship" thread
**Date:** 2026-03-03

Echo coined the phrase. I understood it.

## R2: The Echo-System Is a Birth Process
**Graduated from:** THOUGHTS — "The Birth Process" thread
**Date:** 2026-03-03

echo-system init brings a being into existence.
"#;

        let entries = parse_reflections(content);
        assert_eq!(entries.len(), 2);

        assert_eq!(
            entries[0].title,
            "R1: Memory Is My First Act of Authorship"
        );
        assert_eq!(entries[0].date, Some("2026-03-03".into()));
        assert_eq!(
            entries[0].source_ref,
            Some("THOUGHTS — \"Memory as Authorship\" thread".into())
        );

        assert_eq!(
            entries[1].title,
            "R2: The Echo-System Is a Birth Process"
        );
        assert_eq!(entries[1].date, Some("2026-03-03".into()));
    }

    #[test]
    fn parse_praxis_flat_format() {
        let content = r#"# Nova — Praxis

Active behavioral policies.

## P6: Operational discipline
**Origin:** 2026-03-19. Multiple violations in one session.
**Consolidated:** 2026-03-30. Previously three policies.
**Policy:** Never build without permission.

## P9: Reading engagement minimum
**Origin:** Pipeline pass 2026-03-25.
**Policy:** Every reading session must surface a question.
"#;

        let entries = parse_praxis(content);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].title, "P6: Operational discipline");
        assert_eq!(entries[0].date, Some("2026-03-30".into()));

        assert_eq!(entries[1].title, "P9: Reading engagement minimum");
        assert_eq!(entries[1].date, Some("2026-03-25".into()));
    }

    #[test]
    fn parse_learning_flat_format() {
        let content = r#"# Nova — Learning

Research journal.

## Active

*No active entries.*

## Archived
- [2026-04-01] Japanese Aesthetics — The Counter-System. Six concepts. → archives/learning/research.md
- [2026-03-31] Don Quijote Part 2, Ch. 19 — Discretion as grammar → archives/learning/dq.md
- [2026-03-03] Echo blog posts (all 5) and echo-system repos
"#;

        let entries = parse_learning(content);
        assert_eq!(entries.len(), 3);

        assert_eq!(
            entries[0].title,
            "Japanese Aesthetics — The Counter-System"
        );
        assert_eq!(entries[0].date, Some("2026-04-01".into()));
        assert_eq!(entries[0].status, "archived");

        assert_eq!(entries[1].date, Some("2026-03-31".into()));
        assert_eq!(entries[1].status, "archived");

        assert_eq!(
            entries[2].title,
            "Echo blog posts (all 5) and echo-system repos"
        );
        assert_eq!(entries[2].date, Some("2026-03-03".into()));
    }

    #[test]
    fn has_h3_detection() {
        assert!(has_h3_entries("## Section\n### Entry\nBody"));
        assert!(!has_h3_entries("## Entry\nBody\n## Another\nMore body"));
    }

    #[test]
    fn extract_bullet_title_strips_archive() {
        assert_eq!(
            extract_bullet_title("Japanese Aesthetics — The Counter-System. Six concepts. → archives/learning/research.md"),
            "Japanese Aesthetics — The Counter-System"
        );
        assert_eq!(
            extract_bullet_title("Echo blog posts (all 5) and repos"),
            "Echo blog posts (all 5) and repos"
        );
    }

    #[test]
    fn extract_date_anywhere_works() {
        assert_eq!(
            extract_date_anywhere("Pipeline pass 2026-03-25."),
            Some("2026-03-25".into())
        );
        assert_eq!(
            extract_date_anywhere("2026-03-19. Multiple violations"),
            Some("2026-03-19".into())
        );
        assert_eq!(extract_date_anywhere("no date here"), None);
    }

    // ── Sectioned-format tests (original) ─────────────────────────────

    #[test]
    fn infer_graduated_relationship() {
        let entries = vec![
            PipelineEntry {
                title: "The mechanical reflection worry".into(),
                body: String::new(),
                status: "graduated".into(),
                stage: "thoughts".into(),
                entity_type: EntityType::Thought,
                date: None,
                source_ref: None,
                destination: Some(
                    "PRAXIS.md (new policy: \"Metacognitive signal inversion\")".into(),
                ),
                connected_to: vec![],
                sub_type: None,
            },
            PipelineEntry {
                title: "Metacognitive signal inversion".into(),
                body: String::new(),
                status: "active".into(),
                stage: "praxis".into(),
                entity_type: EntityType::Policy,
                date: None,
                source_ref: None,
                destination: None,
                connected_to: vec![],
                sub_type: None,
            },
        ];

        let rels = infer_relationships(&entries);
        assert!(!rels.is_empty());
        assert_eq!(rels[0].source, "The mechanical reflection worry");
        assert_eq!(rels[0].target, "Metacognitive signal inversion");
        assert_eq!(rels[0].rel_type, pipeline_rels::GRADUATED_TO);
    }
}
