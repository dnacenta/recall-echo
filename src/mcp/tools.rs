// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tool catalogue exposed over MCP.
//!
//! Each tool is a thin, well-described face on one daemon operation. The
//! descriptions are the interface: an agent picks a tool by reading them, so
//! they say what the tool answers and when to prefer a sibling, not how it is
//! implemented.
//!
//! Nothing here touches the store. A tool turns arguments into a
//! [`Request`]; [`crate::mcp`] runs it through the daemon.

use serde_json::{json, Value};

use crate::serve::{
    OverviewArgs, QueryArgs, Request, SearchArgs, SearchEpisodesArgs, TraverseArgs,
};

/// Result limits an agent may ask for. The daemon clamps far higher; these
/// bounds keep a single tool result inside a sane share of the context window.
const MIN_LIMIT: u64 = 1;
const MAX_LIMIT: u64 = 50;
const MAX_EPISODE_LIMIT: u64 = 20;
/// Deepest traversal a tool may ask for. Expansion is exponential in the
/// branching factor, and the rendered tree has to stay readable.
const MAX_TRAVERSE_DEPTH: u64 = 4;

const DEFAULT_ENTITY_LIMIT: u64 = 8;
const DEFAULT_EPISODE_LIMIT: u64 = 5;
const DEFAULT_TRAVERSE_DEPTH: u64 = 2;
/// One hop of graph expansion around the semantic hits — the same default the
/// `graph query` CLI uses. Deeper expansion buys noise faster than recall.
const QUERY_GRAPH_DEPTH: u32 = 1;

/// A tool call whose arguments the agent can fix by trying again.
///
/// Reported as a tool execution error (`isError: true`), never as a JSON-RPC
/// error: the model is the one who can correct it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidArguments(pub String);

impl std::fmt::Display for InvalidArguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A memory tool an MCP client can call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Semantic entity search.
    Search,
    /// Hybrid retrieval: semantic + graph expansion + optional episodes.
    Query,
    /// Relationships out of one named entity.
    Traverse,
    /// Episode (conversation-fragment) search.
    Episodes,
    /// What memory holds, without being asked about anything in particular.
    Overview,
    /// Graph counts.
    Status,
}

/// Every tool, in the order `tools/list` reports them.
///
/// The order is fixed: clients cache the tool list, and a stable order keeps
/// their prompt caches warm.
pub const ALL: [Tool; 6] = [
    Tool::Query,
    Tool::Search,
    Tool::Episodes,
    Tool::Traverse,
    Tool::Overview,
    Tool::Status,
];

impl Tool {
    /// Look a tool up by its wire name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// The name an agent calls this tool by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Tool::Search => "recall_search",
            Tool::Query => "recall_query",
            Tool::Traverse => "recall_traverse",
            Tool::Episodes => "recall_episodes",
            Tool::Overview => "recall_overview",
            Tool::Status => "recall_status",
        }
    }

    /// Human-readable name, for client UIs that show one.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Tool::Search => "Search memory for entities",
            Tool::Query => "Recall from memory",
            Tool::Traverse => "Explore an entity's relationships",
            Tool::Episodes => "Search past conversations",
            Tool::Overview => "What memory holds",
            Tool::Status => "Memory graph status",
        }
    }

    /// What the tool answers, and when to prefer a sibling.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Tool::Query => {
                "Recall what you already know about something, from your own long-term memory of \
                 past sessions. This is the default memory lookup and usually the right first \
                 call. It runs a semantic search over the distilled knowledge graph, expands one \
                 hop along the relationships around each hit, and (by default) also returns the \
                 conversation fragments the knowledge came from. Reach for it whenever the user \
                 refers to something outside this conversation — \"the approach we settled on\", \
                 \"like we did last time\", \"my usual setup\" — or before asserting that you \
                 have no prior context. Returns each entity with its type, a one-line abstract, \
                 a retrieval score, and whether it was matched directly or pulled in through a \
                 relationship."
            }
            Tool::Search => {
                "Semantic search over the entities in your long-term memory: the people, \
                 projects, tools, services, decisions, preferences and concepts distilled from \
                 past conversations. Matching is by meaning, not keywords, so \"how do we ship \
                 releases\" finds entities about CI, tagging and deployment. Returns names, \
                 types, abstracts and retrieval scores — a compact map of what is known. Use \
                 this when you want the inventory of relevant entities and nothing more; use \
                 recall_query when you also want their relationships and the original \
                 conversation text."
            }
            Tool::Episodes => {
                "Search the raw conversation fragments (episodes) stored in memory, rather than \
                 the distilled entities. Each result is a dated chunk of a past session with its \
                 session id and text. Use it when you need what was actually said — exact \
                 wording, a command, a number, a snippet of code — instead of a summarised fact, \
                 or when recall_search and recall_query come back empty because the topic was \
                 discussed but never distilled into an entity. Episodes are the ground truth the \
                 entities were derived from."
            }
            Tool::Traverse => {
                "Walk the relationships out of one named entity and show them as a tree, with \
                 each edge's confidence. Use it after recall_search or recall_query has given \
                 you an exact entity name, when you need the structure around a fact rather than \
                 more facts: what a project depends on, who decided what, which choice \
                 superseded which. The entity name must match an existing entity exactly. Edges \
                 annotated with a percentage are ones the graph is not fully certain of — that \
                 number is accumulated Bayesian evidence, not a guess — and edges marked \
                 [superseded] describe something that was true once and no longer is."
            }
            Tool::Overview => {
                "Read out what memory actually holds, without querying for anything in \
                 particular: the strongest entities of each type, how firmly the relationships \
                 between them are believed, the least certain of those relationships, and the \
                 ones whose confidence rests largely on the agent having repeated itself. Use it \
                 at the start of working with a user you have no context on, when the user asks \
                 what you remember about them, or before assuming memory is empty. Unlike \
                 recall_status, which only counts rows, this returns the content — and it is the \
                 only tool that surfaces where memory is unsure, which is worth saying out loud \
                 rather than presenting an uncertain fact as settled. Takes no arguments; ask \
                 recall_query instead when you have a specific subject in mind."
            }
            Tool::Status => {
                "Report the size and shape of the memory graph: how many entities, relationships \
                 and conversation episodes it holds, plus the entity counts by type. Use it to \
                 tell an empty memory apart from a failed lookup — if a recall returns nothing, \
                 this says whether that means \"never discussed\" or \"nothing has been ingested \
                 yet\". Takes no arguments."
            }
        }
    }

    /// JSON Schema for this tool's arguments (JSON Schema 2020-12).
    #[must_use]
    pub fn input_schema(self) -> Value {
        match self {
            Tool::Search => object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "What to look for, in natural language. A question or a \
                                        topic both work; matching is on meaning, not wording."
                    },
                    "limit": limit_schema(
                        MAX_LIMIT,
                        DEFAULT_ENTITY_LIMIT,
                        "Maximum entities to return.",
                    ),
                }),
                &["query"],
            ),
            Tool::Query => object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "What you are trying to remember, in natural language. \
                                        Phrase it as the actual question — the whole query is \
                                        embedded, so more context retrieves better."
                    },
                    "limit": limit_schema(
                        MAX_LIMIT,
                        DEFAULT_ENTITY_LIMIT,
                        "Maximum entities to return.",
                    ),
                    "include_episodes": {
                        "type": "boolean",
                        "description": "Also return the conversation fragments behind the \
                                        entities. Defaults to true; set false when you only \
                                        need the distilled facts and want a shorter result."
                    },
                }),
                &["query"],
            ),
            Tool::Episodes => object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "What was said, in natural language. Matching is on \
                                        meaning, so paraphrasing the topic works."
                    },
                    "limit": limit_schema(
                        MAX_EPISODE_LIMIT,
                        DEFAULT_EPISODE_LIMIT,
                        "Maximum conversation fragments to return. Fragments are long; ask for \
                         few.",
                    ),
                }),
                &["query"],
            ),
            Tool::Traverse => object_schema(
                json!({
                    "entity": {
                        "type": "string",
                        "description": "Exact name of the entity to start from, as returned by \
                                        recall_search or recall_query."
                    },
                    "depth": {
                        "type": "integer",
                        "minimum": MIN_LIMIT,
                        "maximum": MAX_TRAVERSE_DEPTH,
                        "description": "How many relationship hops to follow (1-4). Defaults to \
                                        2. Each hop multiplies the size of the answer."
                    },
                }),
                &["entity"],
            ),
            Tool::Overview | Tool::Status => json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    /// The full `Tool` object `tools/list` reports.
    #[must_use]
    pub fn descriptor(self) -> Value {
        json!({
            "name": self.name(),
            "title": self.title(),
            "description": self.description(),
            "inputSchema": self.input_schema(),
        })
    }

    /// Turn call arguments into the daemon request that answers them.
    ///
    /// `arguments` is whatever the client sent; a missing `arguments` member
    /// arrives here as [`Value::Null`].
    pub fn request(self, arguments: &Value) -> Result<Request, InvalidArguments> {
        let args = normalize(self, arguments)?;
        let request = match self {
            Tool::Search => Request::Search(SearchArgs {
                query: required_text(&args, "query")?,
                limit: bounded_int(&args, "limit", DEFAULT_ENTITY_LIMIT, MAX_LIMIT)? as usize,
                entity_type: None,
                keyword: None,
            }),
            Tool::Query => Request::Query(QueryArgs {
                query: required_text(&args, "query")?,
                limit: bounded_int(&args, "limit", DEFAULT_ENTITY_LIMIT, MAX_LIMIT)? as usize,
                entity_type: None,
                keyword: None,
                depth: QUERY_GRAPH_DEPTH,
                episodes: flag(&args, "include_episodes", true)?,
            }),
            Tool::Episodes => Request::SearchEpisodes(SearchEpisodesArgs {
                query: required_text(&args, "query")?,
                limit: bounded_int(&args, "limit", DEFAULT_EPISODE_LIMIT, MAX_EPISODE_LIMIT)?
                    as usize,
            }),
            Tool::Traverse => Request::Traverse(TraverseArgs {
                entity: required_text(&args, "entity")?,
                depth: bounded_int(&args, "depth", DEFAULT_TRAVERSE_DEPTH, MAX_TRAVERSE_DEPTH)?
                    as u32,
                type_filter: None,
            }),
            // An overview an agent has to page through is not an overview:
            // the daemon's default listing size is the right one, always.
            Tool::Overview => Request::Overview(OverviewArgs { per_type: 0 }),
            Tool::Status => Request::Status,
        };
        Ok(request)
    }
}

// ── Schema helpers ───────────────────────────────────────────────────────

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn limit_schema(max: u64, default: u64, purpose: &str) -> Value {
    json!({
        "type": "integer",
        "minimum": MIN_LIMIT,
        "maximum": max,
        "description": format!("{purpose} Between {MIN_LIMIT} and {max}; defaults to {default}."),
    })
}

// ── Argument helpers ─────────────────────────────────────────────────────

/// Accept the argument object, an absent one, or the empty one.
///
/// Anything else is a shape the agent can fix.
fn normalize(tool: Tool, arguments: &Value) -> Result<Value, InvalidArguments> {
    match arguments {
        Value::Object(_) => Ok(arguments.clone()),
        Value::Null => Ok(json!({})),
        other => Err(InvalidArguments(format!(
            "{}: `arguments` must be a JSON object, got {}",
            tool.name(),
            type_name(other)
        ))),
    }
}

fn required_text(args: &Value, field: &str) -> Result<String, InvalidArguments> {
    match args.get(field) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        Some(Value::String(_)) => Err(InvalidArguments(format!(
            "`{field}` must not be empty — say what you are looking for"
        ))),
        Some(other) => Err(InvalidArguments(format!(
            "`{field}` must be a string, got {}",
            type_name(other)
        ))),
        None => Err(InvalidArguments(format!("`{field}` is required"))),
    }
}

/// A whole number in `MIN_LIMIT..=max`, defaulting when absent.
///
/// Out-of-range numbers are clamped rather than rejected: asking for more
/// than the ceiling is a preference, not a mistake. A non-integer is a
/// mistake, and says so.
fn bounded_int(args: &Value, field: &str, default: u64, max: u64) -> Result<u64, InvalidArguments> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) => Ok(value.clamp(MIN_LIMIT, max)),
            // Negative or fractional: as_i64 catches the negatives, and
            // anything else is a float the caller meant as a count.
            None if number.as_i64().is_some() => Ok(MIN_LIMIT),
            None => Err(InvalidArguments(format!(
                "`{field}` must be a whole number between {MIN_LIMIT} and {max}"
            ))),
        },
        Some(other) => Err(InvalidArguments(format!(
            "`{field}` must be a whole number between {MIN_LIMIT} and {max}, got {}",
            type_name(other)
        ))),
    }
}

fn flag(args: &Value, field: &str, default: bool) -> Result<bool, InvalidArguments> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(InvalidArguments(format!(
            "`{field}` must be true or false, got {}",
            type_name(other)
        ))),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_resolves_from_its_own_name() {
        for tool in ALL {
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
    }

    #[test]
    fn unknown_names_do_not_resolve() {
        assert_eq!(Tool::from_name("recall_forget"), None);
        assert_eq!(Tool::from_name(""), None);
    }

    #[test]
    fn descriptors_carry_a_schema_and_a_description() {
        for tool in ALL {
            let descriptor = tool.descriptor();
            assert_eq!(descriptor["name"], tool.name());
            assert_eq!(descriptor["inputSchema"]["type"], "object");
            assert!(
                descriptor["description"].as_str().unwrap().len() > 80,
                "{} needs a description an agent can choose from",
                tool.name()
            );
        }
    }

    #[test]
    fn search_defaults_the_limit() {
        let request = Tool::Search.request(&json!({ "query": "rust" })).unwrap();
        assert_eq!(
            request,
            Request::Search(SearchArgs {
                query: "rust".into(),
                limit: DEFAULT_ENTITY_LIMIT as usize,
                entity_type: None,
                keyword: None,
            })
        );
    }

    #[test]
    fn query_includes_episodes_unless_told_otherwise() {
        let Request::Query(args) = Tool::Query.request(&json!({ "query": "deploys" })).unwrap()
        else {
            panic!("expected a query request");
        };
        assert!(args.episodes);
        assert_eq!(args.depth, QUERY_GRAPH_DEPTH);

        let Request::Query(args) = Tool::Query
            .request(&json!({ "query": "deploys", "include_episodes": false }))
            .unwrap()
        else {
            panic!("expected a query request");
        };
        assert!(!args.episodes);
    }

    #[test]
    fn status_ignores_arguments_and_absent_arguments() {
        assert_eq!(Tool::Status.request(&Value::Null).unwrap(), Request::Status);
        assert_eq!(
            Tool::Status.request(&json!({ "noise": 1 })).unwrap(),
            Request::Status
        );
    }

    #[test]
    fn oversized_limits_clamp_instead_of_failing() {
        let Request::Search(args) = Tool::Search
            .request(&json!({ "query": "rust", "limit": 9_000 }))
            .unwrap()
        else {
            panic!("expected a search request");
        };
        assert_eq!(args.limit, MAX_LIMIT as usize);

        let Request::Traverse(args) = Tool::Traverse
            .request(&json!({ "entity": "Rust", "depth": 0 }))
            .unwrap()
        else {
            panic!("expected a traverse request");
        };
        assert_eq!(args.depth, MIN_LIMIT as u32);
    }

    #[test]
    fn missing_required_arguments_are_reported_by_name() {
        let error = Tool::Search.request(&json!({})).unwrap_err();
        assert!(error.to_string().contains("`query` is required"), "{error}");

        let error = Tool::Traverse.request(&json!({ "depth": 2 })).unwrap_err();
        assert!(
            error.to_string().contains("`entity` is required"),
            "{error}"
        );
    }

    #[test]
    fn blank_and_mistyped_arguments_are_rejected() {
        let error = Tool::Search.request(&json!({ "query": "  " })).unwrap_err();
        assert!(error.to_string().contains("must not be empty"), "{error}");

        let error = Tool::Search.request(&json!({ "query": 12 })).unwrap_err();
        assert!(error.to_string().contains("must be a string"), "{error}");

        let error = Tool::Search
            .request(&json!({ "query": "rust", "limit": "many" }))
            .unwrap_err();
        assert!(error.to_string().contains("whole number"), "{error}");

        let error = Tool::Query
            .request(&json!({ "query": "rust", "include_episodes": "yes" }))
            .unwrap_err();
        assert!(error.to_string().contains("true or false"), "{error}");
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        let error = Tool::Search.request(&json!([1, 2, 3])).unwrap_err();
        assert!(
            error.to_string().contains("must be a JSON object"),
            "{error}"
        );
    }

    #[test]
    fn schemas_declare_their_required_arguments() {
        assert_eq!(Tool::Search.input_schema()["required"], json!(["query"]));
        assert_eq!(Tool::Traverse.input_schema()["required"], json!(["entity"]));
        assert_eq!(
            Tool::Status.input_schema()["additionalProperties"],
            json!(false)
        );
    }
}
