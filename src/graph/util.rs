// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared utility functions for the graph subsystem.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Read a counter that a record may predate.
///
/// SurrealDB renders a field an older record never had as absent under
/// `SELECT *` and as `null` under a projection; both mean the same thing here,
/// and both mean zero.
pub fn count_or_zero<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<i64>::deserialize(deserializer)?.unwrap_or(0))
}

/// Strip markdown code fencing (```json ... ```) from LLM responses.
#[must_use]
pub fn strip_markdown_fencing(text: &str) -> String {
    let trimmed = text.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or(trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let stripped = stripped.strip_suffix("```").unwrap_or(stripped);
    stripped.trim().to_string()
}

/// Whether an LLM response looks like JSON that was cut off mid-object.
///
/// A response truncated against the output cap starts an object it never
/// closes: there is a `{`, and no balanced object to extract. Distinct from
/// "no JSON at all" (a refusal, prose, a hijacked format), which should not
/// be retried as a size problem.
#[must_use]
pub fn is_truncated_json(text: &str) -> bool {
    let cleaned = strip_markdown_fencing(text);
    cleaned.contains('{') && extract_json_object(&cleaned).is_none()
}

/// Extract the first balanced JSON object from a string.
///
/// Finds the first `{` and returns the substring up to the matching `}`.
#[must_use]
pub fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes[start..].iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse a SurrealDB datetime value (serde_json::Value) into a chrono DateTime.
///
/// Handles both standard ISO 8601 and SurrealDB's datetime format.
#[must_use]
pub fn parse_datetime(val: &serde_json::Value) -> Option<DateTime<Utc>> {
    match val {
        serde_json::Value::String(s) => s.parse::<DateTime<Utc>>().ok().or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ")
                .ok()
                .map(|ndt| ndt.and_utc())
        }),
        _ => None,
    }
}

/// Merge two JSON objects, with `overlay` keys taking precedence.
///
/// If either value is not an object, returns `overlay`.
#[must_use]
pub fn merge_json_objects(
    base: &serde_json::Value,
    overlay: &serde_json::Value,
) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            let mut merged = b.clone();
            for (k, v) in o {
                merged.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(merged)
        }
        _ => overlay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fencing_json() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        assert_eq!(strip_markdown_fencing(input), "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_fencing_plain() {
        let input = "```\n{\"key\": \"value\"}\n```";
        assert_eq!(strip_markdown_fencing(input), "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_fencing_none() {
        let input = "{\"key\": \"value\"}";
        assert_eq!(strip_markdown_fencing(input), input);
    }

    #[test]
    fn extract_json_simple() {
        let input = "Some text {\"key\": \"value\"} more text";
        assert_eq!(extract_json_object(input), Some("{\"key\": \"value\"}"));
    }

    #[test]
    fn extract_json_nested() {
        let input = "{\"outer\": {\"inner\": 1}}";
        assert_eq!(extract_json_object(input), Some(input));
    }

    #[test]
    fn extract_json_none() {
        assert_eq!(extract_json_object("no json here"), None);
    }

    #[test]
    fn parse_datetime_iso() {
        let val = serde_json::Value::String("2024-01-15T10:30:00Z".into());
        let dt = parse_datetime(&val);
        assert!(dt.is_some());
    }

    #[test]
    fn parse_datetime_invalid() {
        let val = serde_json::Value::String("not-a-date".into());
        assert!(parse_datetime(&val).is_none());
    }

    #[test]
    fn parse_datetime_non_string() {
        let val = serde_json::json!(42);
        assert!(parse_datetime(&val).is_none());
    }

    #[test]
    fn merge_objects() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let overlay = serde_json::json!({"b": 3, "c": 4});
        let merged = merge_json_objects(&base, &overlay);
        assert_eq!(merged, serde_json::json!({"a": 1, "b": 3, "c": 4}));
    }

    #[test]
    fn merge_non_objects() {
        let base = serde_json::json!("string");
        let overlay = serde_json::json!(42);
        assert_eq!(merge_json_objects(&base, &overlay), serde_json::json!(42));
    }

    /// The real failure shape: an output-capped response opens a fence and an
    /// object and closes neither.
    #[test]
    fn a_response_cut_off_mid_object_is_truncated() {
        assert!(is_truncated_json(
            "```json\n{\n  \"entities\": [ { \"name\": \"PR #2399\","
        ));
        assert!(is_truncated_json("{ \"entities\": [ { \"name\": \"x\""));
    }

    #[test]
    fn complete_or_json_free_responses_are_not_truncated() {
        // Balanced object, fenced or bare.
        assert!(!is_truncated_json("```json\n{\"entities\": []}\n```"));
        assert!(!is_truncated_json("{\"entities\": []}"));
        // No JSON at all — a hijacked or refused response is not a size
        // problem, and must not trigger the terse retry.
        assert!(!is_truncated_json(
            "VERDICT: REQUEST_CHANGES\nSUMMARY: Requesting changes"
        ));
    }
}
