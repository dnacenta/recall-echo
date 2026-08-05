// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MCP server — the read path into the knowledge graph.
//!
//! Without this, the graph is write-only in practice. `SessionEnd` ingests
//! episodes and `SessionStart` runs `consume`, which only prints EPHEMERAL.md;
//! nothing in a normal session ever queries the store. Bayesian confidence,
//! HNSW search, provenance weighting and temporal decay all sit behind a
//! command a human has to type by hand. An MCP server closes that loop: the
//! agent asks its own memory, with the actual question, at the moment the
//! question comes up.
//!
//! # Shape
//!
//! JSON-RPC 2.0 over stdin/stdout, one message per line — the standard local
//! MCP transport. The surface is small on purpose: `initialize`, `ping`,
//! `tools/list`, `tools/call`, and notifications, which are consumed
//! silently. Anything else is a JSON-RPC `method not found`; nothing here
//! panics on hostile input.
//!
//! Every tool runs through [`crate::serve_client::execute`], so the MCP server
//! is just another daemon client and inherits the daemon's locking discipline,
//! concurrency and auto-start. It never opens the store itself.
//!
//! # Read-only
//!
//! No tool writes. The graph's confidence model deliberately discounts what
//! the agent asserts about itself (see `[graph.provenance]`); a tool that let
//! the model create entities and edges directly would route around exactly
//! the mechanism that keeps self-generated claims from becoming evidence.
//! Writing stays on the ingest path, where every episode is stamped with its
//! authorship.

pub mod render;
pub mod tools;

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::error::RecallError;
use crate::graph::types::{
    EpisodeSearchResult, GraphStats, QueryResult, ScoredEntity, TraversalNode,
};
use crate::serve::Request;
use crate::serve_client;
use tools::Tool;

/// MCP revisions this server speaks, newest first.
///
/// All of them carry the same `initialize` / `tools/list` / `tools/call`
/// shapes for a tools-only server, so one implementation serves them all.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision offered to a client that asks for one we do not implement.
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-11-25";

/// Largest single JSON-RPC message accepted. A tool call is tiny; anything
/// approaching this is a broken or hostile client trying to make us buffer
/// without bound.
const MAX_MESSAGE_BYTES: u64 = 4 * 1024 * 1024;

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// What the client is told this server is for, at handshake time. It is the
/// only chance to say *when* to reach for memory before the model has to
/// decide.
const INSTRUCTIONS: &str = "\
recall-echo is this agent's own long-term memory: a knowledge graph of entities, \
relationships and conversation fragments built from previous sessions, with Bayesian \
confidence on every relationship. None of it is loaded automatically — memory is written \
when a session ends and read only when one of these tools is called.

Call recall_query before answering anything that depends on earlier sessions: the user's \
established preferences and setup, decisions already made, projects already discussed, or \
any reference to \"what we did\" that is not in the current conversation. Prefer asking \
memory over asking the user to repeat themselves. Every tool here is read-only and cheap; \
calling one speculatively costs nothing but tokens.";

// ── Backend ──────────────────────────────────────────────────────────────

/// The graph operations this server runs tools against.
///
/// One method, one meaning: hand a daemon [`Request`] over, get its JSON back.
/// The indirection exists so the protocol layer can be exercised without a
/// store, an embedding model or a daemon.
#[async_trait::async_trait]
pub trait GraphBackend: Send + Sync {
    /// Run a graph operation and return the daemon's `data` payload.
    async fn execute(&self, request: &Request) -> Result<Value, RecallError>;
}

/// The real backend: the graph daemon for a memory directory.
#[derive(Debug, Clone)]
pub struct DaemonBackend {
    memory_dir: PathBuf,
}

impl DaemonBackend {
    #[must_use]
    pub fn new(memory_dir: impl Into<PathBuf>) -> Self {
        Self {
            memory_dir: memory_dir.into(),
        }
    }
}

#[async_trait::async_trait]
impl GraphBackend for DaemonBackend {
    async fn execute(&self, request: &Request) -> Result<Value, RecallError> {
        serve_client::execute(&self.memory_dir, request).await
    }
}

// ── Wire types ───────────────────────────────────────────────────────────

/// An incoming JSON-RPC message. A message without an `id` is a notification
/// and is never answered.
#[derive(Debug, Deserialize)]
struct RpcMessage {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

/// A JSON-RPC error, as returned in the `error` member of a response.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    fn to_value(&self) -> Value {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, error: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error.to_value() })
}

// ── Server ───────────────────────────────────────────────────────────────

/// An MCP server over some [`GraphBackend`].
///
/// Stateless by design: it does not require `initialize` before answering
/// `tools/list`, because refusing would only turn a client's ordering bug into
/// a silent memory outage. Nothing it returns depends on connection state.
#[derive(Debug, Clone)]
pub struct McpServer<B> {
    backend: B,
    server_version: String,
}

impl<B: GraphBackend> McpServer<B> {
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// The backend this server runs tools against.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Handle one line of the transport, returning the message to write back.
    ///
    /// `None` means "say nothing": a notification, or a batch of them.
    pub async fn handle_line(&self, line: &str) -> Option<Value> {
        let incoming: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                return Some(failure(
                    Value::Null,
                    &RpcError::new(PARSE_ERROR, format!("invalid JSON: {err}")),
                ))
            }
        };

        match incoming {
            Value::Array(messages) if messages.is_empty() => Some(failure(
                Value::Null,
                &RpcError::new(INVALID_REQUEST, "a batch must not be empty"),
            )),
            Value::Array(messages) => {
                let mut responses = Vec::with_capacity(messages.len());
                for message in messages {
                    if let Some(response) = self.handle_message(message).await {
                        responses.push(response);
                    }
                }
                (!responses.is_empty()).then_some(Value::Array(responses))
            }
            other => self.handle_message(other).await,
        }
    }

    async fn handle_message(&self, message: Value) -> Option<Value> {
        // Recovered before parsing so a structurally invalid request can still
        // be answered against the id the client is waiting on.
        let id = message.get("id").cloned().unwrap_or(Value::Null);

        // A structurally invalid message is not a notification, even without
        // an id: JSON-RPC 2.0 answers it against a null id rather than
        // leaving the client to time out.
        let request: RpcMessage = match serde_json::from_value(message) {
            Ok(request) => request,
            Err(err) => {
                return Some(failure(
                    id,
                    &RpcError::new(INVALID_REQUEST, format!("invalid JSON-RPC request: {err}")),
                ))
            }
        };

        if request.jsonrpc != "2.0" {
            return Some(failure(
                id,
                &RpcError::new(
                    INVALID_REQUEST,
                    format!(
                        "unsupported JSON-RPC version `{}`; this server speaks 2.0",
                        request.jsonrpc
                    ),
                ),
            ));
        }

        // A well-formed notification is never answered, whatever it carries.
        if request.method.starts_with("notifications/") || request.id.is_none() {
            return None;
        }
        let id = request.id.unwrap_or(Value::Null);

        let result = self.dispatch(&request.method, request.params).await;
        Some(match result {
            Ok(value) => success(id, value),
            Err(error) => failure(id, &error),
        })
    }

    async fn dispatch(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => self.list_tools(params),
            "tools/call" => self.call_tool(params).await,
            other => Err(
                RpcError::new(METHOD_NOT_FOUND, format!("unknown method `{other}`")).with_data(
                    json!({
                        "supported": ["initialize", "ping", "tools/list", "tools/call"]
                    }),
                ),
            ),
        }
    }

    fn initialize(&self, params: Option<Value>) -> Value {
        let requested = params
            .as_ref()
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str);

        json!({
            "protocolVersion": negotiate_protocol_version(requested),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "recall-echo",
                "title": "recall-echo memory",
                "version": self.server_version,
            },
            "instructions": INSTRUCTIONS,
        })
    }

    fn list_tools(&self, params: Option<Value>) -> Result<Value, RpcError> {
        // The catalogue is static and fits in one page, so no cursor we could
        // have issued is ever valid.
        if let Some(cursor) = params.as_ref().and_then(|params| params.get("cursor")) {
            if !cursor.is_null() {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "the tool list is a single page; no cursor is valid",
                ));
            }
        }

        let catalogue: Vec<Value> = tools::ALL.into_iter().map(Tool::descriptor).collect();
        Ok(json!({ "tools": catalogue }))
    }

    async fn call_tool(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let params = params.unwrap_or(Value::Null);
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "tools/call requires a `name` naming the tool to run",
            ));
        };
        let Some(tool) = Tool::from_name(name) else {
            return Err(
                RpcError::new(INVALID_PARAMS, format!("unknown tool `{name}`")).with_data(json!({
                    "available": tools::ALL.map(Tool::name),
                })),
            );
        };

        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        let request = match tool.request(&arguments) {
            Ok(request) => request,
            Err(invalid) => return Ok(tool_error(invalid.to_string())),
        };

        match self.backend.execute(&request).await {
            Ok(data) => Ok(match render(&request, data) {
                Ok(text) => tool_success(text),
                Err(err) => tool_error(format!(
                    "{} could not read the memory store's answer: {err}",
                    tool.name()
                )),
            }),
            Err(err) => Ok(tool_error(explain(tool, &err))),
        }
    }
}

/// Echo the client's revision when we speak it, otherwise offer ours.
#[must_use]
pub fn negotiate_protocol_version(requested: Option<&str>) -> &str {
    match requested {
        Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => version,
        _ => PREFERRED_PROTOCOL_VERSION,
    }
}

fn tool_success(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

/// A failure the model can act on: reported in the result, not as a JSON-RPC
/// error, so the client passes it back to the model instead of swallowing it.
fn tool_error(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

/// Render a daemon payload as the text its tool promised.
fn render(request: &Request, data: Value) -> Result<String, serde_json::Error> {
    let text = match request {
        Request::Search(args) => {
            let results: Vec<ScoredEntity> = serde_json::from_value(data)?;
            render::entities(&args.query, &results)
        }
        Request::Query(args) => {
            let result: QueryResult = serde_json::from_value(data)?;
            render::query_result(&args.query, &result)
        }
        Request::SearchEpisodes(args) => {
            let results: Vec<EpisodeSearchResult> = serde_json::from_value(data)?;
            render::episodes(&args.query, &results)
        }
        Request::Traverse(args) => {
            let tree: TraversalNode = serde_json::from_value(data)?;
            render::traversal(&args.entity, args.depth, &tree)
        }
        Request::Status => {
            let stats: GraphStats = serde_json::from_value(data)?;
            render::status(&stats)
        }
        // No tool builds any other request; a payload we cannot name is
        // still better returned than dropped.
        _ => serde_json::to_string_pretty(&data)?,
    };
    Ok(text)
}

/// A tool failure said in terms the model can do something about.
fn explain(tool: Tool, error: &RecallError) -> String {
    let mut message = format!("{} failed: {error}", tool.name());
    if let Some(hint) = hint(error) {
        message.push(' ');
        message.push_str(hint);
    }
    message
}

fn hint(error: &RecallError) -> Option<&'static str> {
    match error {
        RecallError::Remote { code, .. } => match code.as_str() {
            "not_found" => Some(
                "Names must match an existing entity exactly — use recall_search or \
                 recall_query to find the exact name first.",
            ),
            "embedding" => Some(
                "The embedding model could not be loaded, so semantic recall is unavailable \
                 until it is; do not retry this session.",
            ),
            "locked" => Some(
                "Another recall-echo operation is holding the memory store; the same call \
                 should succeed shortly.",
            ),
            _ => None,
        },
        RecallError::NotInitialized(_) => Some(
            "Memory is not initialised in this directory; `recall-echo init` creates it. \
             Do not retry until it is.",
        ),
        RecallError::Daemon(_) => Some(
            "The memory daemon could not be reached, so memory is unavailable — continue \
             without it rather than retrying.",
        ),
        _ => None,
    }
}

// ── stdio transport ──────────────────────────────────────────────────────

/// A message reader capped at [`MAX_MESSAGE_BYTES`] per message.
type MessageLines = tokio::io::Lines<BufReader<tokio::io::Take<tokio::io::Stdin>>>;

/// Serve MCP over stdin/stdout until the client closes the connection.
///
/// Messages are handled one at a time. MCP permits interleaved responses, but
/// the daemon serializes graph work anyway, so concurrency here would buy
/// nothing and cost an interleaved-write hazard on stdout.
pub async fn run(memory_dir: &Path) -> Result<(), RecallError> {
    serve(McpServer::new(DaemonBackend::new(memory_dir))).await
}

async fn serve<B: GraphBackend>(server: McpServer<B>) -> Result<(), RecallError> {
    let mut lines = BufReader::new(tokio::io::stdin().take(MAX_MESSAGE_BYTES)).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // The client closed stdin: the specified way to shut a stdio
            // server down.
            Ok(None) => return Ok(()),
            Err(err) => return Err(err.into()),
        };

        if message_cap_reached(&mut lines) {
            let response = failure(
                Value::Null,
                &RpcError::new(
                    INVALID_REQUEST,
                    format!("message exceeds the {MAX_MESSAGE_BYTES}-byte limit"),
                ),
            );
            write_message(&mut stdout, &response).await?;
            return Ok(());
        }
        recharge_message_cap(&mut lines);

        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line).await {
            write_message(&mut stdout, &response).await?;
        }
    }
}

fn message_cap_reached(lines: &mut MessageLines) -> bool {
    lines.get_mut().get_mut().limit() == 0
}

fn recharge_message_cap(lines: &mut MessageLines) {
    lines.get_mut().get_mut().set_limit(MAX_MESSAGE_BYTES);
}

async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<(), RecallError> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stdout.write_all(&line).await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preferred_version_is_one_we_support() {
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&PREFERRED_PROTOCOL_VERSION));
        assert_eq!(SUPPORTED_PROTOCOL_VERSIONS[0], PREFERRED_PROTOCOL_VERSION);
    }

    #[test]
    fn a_supported_version_is_echoed_back() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(negotiate_protocol_version(Some(version)), *version);
        }
    }

    #[test]
    fn an_unknown_version_falls_back_to_ours() {
        assert_eq!(
            negotiate_protocol_version(Some("1900-01-01")),
            PREFERRED_PROTOCOL_VERSION
        );
        assert_eq!(negotiate_protocol_version(None), PREFERRED_PROTOCOL_VERSION);
    }

    #[test]
    fn hints_are_attached_only_where_they_help() {
        let not_found = RecallError::Remote {
            code: "not_found".into(),
            message: "entity not found: Rust".into(),
        };
        let text = explain(Tool::Traverse, &not_found);
        assert!(text.starts_with("recall_traverse failed:"), "{text}");
        assert!(text.contains("recall_search"), "{text}");

        let unknown = RecallError::Remote {
            code: "db".into(),
            message: "connection reset".into(),
        };
        assert_eq!(
            explain(Tool::Status, &unknown),
            "recall_status failed: connection reset"
        );
    }

    #[test]
    fn an_unrenderable_payload_is_dumped_rather_than_dropped() {
        let text = render(&Request::Hello, json!({ "version": "3.13.0" })).unwrap();
        assert!(text.contains("3.13.0"), "{text}");
    }
}
