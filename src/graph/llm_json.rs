// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Finding the JSON object in a model's answer, and saying precisely why
//! when there is none.
//!
//! A model asked for "ONLY the JSON" still sometimes wraps it in a fence,
//! puts a sentence before or after it, or stops halfway through. Every one of
//! those is handled by a real JSON parser here — never by counting braces,
//! which a `}` inside a string silently defeats:
//!
//! ```text
//! answer ─▶ first `{` ─▶ serde_json reads ONE value from there
//!                          ├─ a complete value ─▶ Ok (anything after it is ignored)
//!                          ├─ input ran out    ─▶ Truncated
//!                          └─ bad token        ─▶ InvalidJson
//! no `{` at all ─────────────────────────────────▶ NoJson
//! ```

use std::fmt;

use serde_json::error::Category;
use serde_json::Value;

/// Why an answer yielded no usable JSON object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonFailure {
    /// No object anywhere: prose, a refusal, a format the transcript
    /// dictated.
    NoJson,
    /// An object starts and the answer ends before it closes — an output
    /// cap, or a model that stopped early.
    Truncated { detail: String },
    /// The object is complete in length but not JSON: a stray token, an
    /// unescaped quote, a code expression where a value belongs.
    InvalidJson { detail: String },
    /// Valid JSON, but not the shape that was asked for.
    Shape { detail: String },
}

impl JsonFailure {
    /// A short, stable name for the class, for warnings and counts.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::NoJson => "no JSON",
            Self::Truncated { .. } => "truncated",
            Self::InvalidJson { .. } => "invalid JSON",
            Self::Shape { .. } => "unexpected shape",
        }
    }

    /// Whether the answer ran out mid-object — a size problem rather than a
    /// content problem.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::Truncated { .. })
    }
}

impl fmt::Display for JsonFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoJson => f.write_str("no JSON object in the response"),
            Self::Truncated { detail } | Self::InvalidJson { detail } | Self::Shape { detail } => {
                write!(f, "{}: {detail}", self.class())
            }
        }
    }
}

/// The first JSON object in `text`, parsed; text around it is ignored.
///
/// The object starts at the first `{`. Markdown fences and prose before it
/// hold no `{` in practice, and prose after it is never read: the parser
/// stops at the end of the first complete value.
///
/// # Errors
///
/// [`JsonFailure::NoJson`] without a `{`, [`JsonFailure::Truncated`] when the
/// text ends inside the object, [`JsonFailure::InvalidJson`] on anything else
/// the parser rejects.
pub fn first_json_object(text: &str) -> Result<serde_json::Map<String, Value>, JsonFailure> {
    // A CLI ends its output with a newline; inside a string cut short, that
    // newline would read as a control character instead of the end.
    let text = text.trim_end();
    let start = text.find('{').ok_or(JsonFailure::NoJson)?;
    let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
    match values.next() {
        Some(Ok(Value::Object(object))) => Ok(object),
        Some(Err(err)) => Err(classify(&err)),
        // A value read from a `{` is an object; anything else is no value.
        _ => Err(JsonFailure::NoJson),
    }
}

/// Recover the complete leading elements of an answer cut off mid-object.
///
/// The extraction answer is one object of arrays — `{"entities": [{…}, {…`.
/// Scanning with string and escape state, this finds the end of the last
/// object that closed as an element of one of those top-level arrays, cuts
/// there, closes the array and the root, and hands the result to the real
/// parser. Anything the parser does not accept is not salvaged: a guess at
/// the shape never reaches the graph.
///
/// `None` when the text is not truncated JSON, or no element completed
/// before the cut.
#[must_use]
pub fn salvage_truncated(text: &str) -> Option<serde_json::Map<String, Value>> {
    let text = text.trim_end();
    let start = text.find('{')?;
    let body = &text[start..];
    let cut = last_complete_array_element(body)?;
    let mut repaired = String::with_capacity(cut + 2);
    repaired.push_str(&body[..cut]);
    repaired.push_str("]}");
    match serde_json::from_str::<Value>(&repaired) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

/// Byte offset just past the last `}` that closed an object sitting directly
/// in an array that sits directly in the root object.
fn last_complete_array_element(body: &str) -> Option<usize> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Open {
        Object,
        Array,
    }

    let mut stack: Vec<Open> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut last = None;

    for (i, byte) in body.bytes().enumerate() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => stack.push(Open::Object),
            b'[' => stack.push(Open::Array),
            b'}' | b']' => {
                let closing = if byte == b'}' {
                    Open::Object
                } else {
                    Open::Array
                };
                if stack.pop() != Some(closing) {
                    // Mismatched nesting is not truncation; nothing is safe.
                    return None;
                }
                if stack.is_empty() {
                    // The root closed: the text was never truncated.
                    return None;
                }
                if closing == Open::Object && stack == [Open::Object, Open::Array] {
                    last = Some(i + 1);
                }
            }
            _ => {}
        }
    }
    last
}

fn classify(err: &serde_json::Error) -> JsonFailure {
    match err.classify() {
        Category::Eof => JsonFailure::Truncated {
            detail: format!("the response ends inside the object ({err})"),
        },
        Category::Syntax | Category::Io => JsonFailure::InvalidJson {
            detail: err.to_string(),
        },
        Category::Data => JsonFailure::Shape {
            detail: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_object_parses() {
        let object = first_json_object(r#"{"entities": []}"#).unwrap();
        assert!(object.contains_key("entities"));
    }

    #[test]
    fn fences_and_prose_around_the_object_are_ignored() {
        let text = "Here is the extraction:\n```json\n{\"entities\": [], \"note\": \"a } in a string\"}\n```\nLet me know if you need more.";
        let object = first_json_object(text).unwrap();
        assert_eq!(object["note"], "a } in a string");
    }

    #[test]
    fn a_second_object_after_the_first_is_ignored() {
        let object = first_json_object(r#"{"a": 1} {"b": 2}"#).unwrap();
        assert!(object.contains_key("a") && !object.contains_key("b"));
    }

    #[test]
    fn text_without_an_object_is_no_json() {
        assert_eq!(
            first_json_object("VERDICT: REQUEST_CHANGES"),
            Err(JsonFailure::NoJson)
        );
    }

    #[test]
    fn an_object_that_never_closes_is_truncated() {
        let failure = first_json_object("```json\n{\"entities\": [{\"name\": \"PR #2").unwrap_err();
        assert!(failure.is_truncated(), "{failure}");
    }

    /// Recorded from Synth's extraction of conversation-575, chunk 47
    /// (claude, sonnet): a JavaScript expression where an array belongs.
    #[test]
    fn a_code_expression_in_place_of_a_value_is_invalid_json() {
        let text = r#"{"entities":[{"name":"Kinship","type":"concept","abstract":"Bound by trust.","overview":null,"content":null,"attributes":{}}],"relationships":[[]].length ? null : null,"cases":[]}"#;
        let failure = first_json_object(text).unwrap_err();
        assert_eq!(failure.class(), "invalid JSON");
        assert!(failure.to_string().contains("column"), "{failure}");
    }

    /// CLIs end their output with a newline, even one cut off mid-string.
    #[test]
    fn a_trailing_newline_does_not_hide_a_truncation() {
        let failure = first_json_object("{\"entities\": [{\"name\": \"cut\n").unwrap_err();
        assert!(failure.is_truncated(), "{failure}");
    }

    #[test]
    fn salvage_keeps_every_element_that_completed() {
        let text = r#"```json
{"entities": [{"name": "A", "abstract": "has a } brace"}, {"name": "B"}], "relationships": [{"source": "A", "target": "B"}, {"source": "B", "tar"#;
        let object = salvage_truncated(text).unwrap();
        assert_eq!(object["entities"].as_array().unwrap().len(), 2);
        assert_eq!(object["relationships"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn salvage_needs_at_least_one_complete_element() {
        assert!(salvage_truncated(r#"{"entities": [{"name": "A", "abs"#).is_none());
        assert!(salvage_truncated(r#"{"entities": ["#).is_none());
    }

    #[test]
    fn salvage_refuses_text_that_was_never_truncated() {
        assert!(salvage_truncated(r#"{"entities": [{"name": "A"}]} trailing"#).is_none());
        assert!(salvage_truncated("no json").is_none());
    }

    #[test]
    fn salvage_ignores_escaped_quotes_inside_strings() {
        let text = r#"{"entities": [{"name": "say \"}]\" twice"}, {"name": "cut"#;
        let object = salvage_truncated(text).unwrap();
        assert_eq!(object["entities"][0]["name"], "say \"}]\" twice");
        assert_eq!(object["entities"].as_array().unwrap().len(), 1);
    }
}
