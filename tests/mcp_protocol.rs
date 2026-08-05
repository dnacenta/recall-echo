//! MCP protocol conformance: the handshake, the tool catalogue, and what a
//! client sees on every failure mode.
//!
//! These run entirely against a stubbed [`GraphBackend`], so nothing here
//! opens a store, starts a daemon or loads an embedding model. What is under
//! test is the wire contract — a client that follows the spec must never see
//! a panic, a dropped response, or a shape it cannot parse.

use std::collections::HashMap;
use std::sync::Mutex;

use recall_echo::error::RecallError;
use recall_echo::graph::types::{
    EntityDetail, EntitySummary, EntityType, Episode, EpisodeSearchResult, GraphStats, MatchSource,
    QueryResult, ScoredEntity, TraversalEdge, TraversalNode,
};
use recall_echo::mcp::{GraphBackend, McpServer, PREFERRED_PROTOCOL_VERSION};
use recall_echo::serve::Request;
use serde_json::{json, Value};

// ── Stub backend ─────────────────────────────────────────────────────────

/// What the stubbed daemon does with whatever request reaches it.
enum Outcome {
    /// Answer with a payload shaped like the real operation's output.
    Canned,
    /// Answer with this exact payload, whatever was asked.
    Data(Value),
    /// Fail the way the daemon reports a named error.
    Remote {
        code: &'static str,
        message: &'static str,
    },
}

struct Stub {
    outcome: Outcome,
    calls: Mutex<Vec<Request>>,
}

impl Stub {
    fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl GraphBackend for Stub {
    async fn execute(&self, request: &Request) -> Result<Value, RecallError> {
        self.calls.lock().unwrap().push(request.clone());
        match &self.outcome {
            Outcome::Canned => Ok(canned_payload(request)),
            Outcome::Data(data) => Ok(data.clone()),
            Outcome::Remote { code, message } => Err(RecallError::Remote {
                code: (*code).to_string(),
                message: (*message).to_string(),
            }),
        }
    }
}

/// Payloads built from the real result types, so the test cannot drift from
/// the shapes the daemon actually serializes.
fn canned_payload(request: &Request) -> Value {
    match request {
        Request::Search(_) => json!([scored_entity()]),
        Request::Query(_) => serde_json::to_value(QueryResult {
            entities: vec![scored_entity_typed()],
            episodes: vec![episode_typed()],
        })
        .unwrap(),
        Request::SearchEpisodes(_) => json!([episode()]),
        Request::Traverse(_) => serde_json::to_value(traversal_tree()).unwrap(),
        Request::Status => serde_json::to_value(GraphStats {
            entity_count: 12,
            relationship_count: 30,
            episode_count: 400,
            entity_type_counts: HashMap::from([("project".to_string(), 7)]),
        })
        .unwrap(),
        other => panic!("no MCP tool builds a {} request", other.op_name()),
    }
}

fn entity_detail() -> EntityDetail {
    EntityDetail {
        id: json!("entity:rust"),
        name: "Rust".into(),
        entity_type: EntityType::Tool,
        abstract_text: "Systems language with ownership.".into(),
        overview: "Rust enforces memory safety without a garbage collector.".into(),
        attributes: None,
        access_count: 4,
        utility_score: 0.77,
        updated_at: json!("2026-05-01T09:15:30.123Z"),
        source: Some("archive-log-042".into()),
    }
}

fn scored_entity_typed() -> ScoredEntity {
    ScoredEntity {
        entity: entity_detail(),
        score: 0.842,
        source: MatchSource::Semantic,
    }
}

fn scored_entity() -> Value {
    serde_json::to_value(scored_entity_typed()).unwrap()
}

fn episode() -> Value {
    serde_json::to_value(episode_typed()).unwrap()
}

fn episode_typed() -> EpisodeSearchResult {
    EpisodeSearchResult {
        episode: Episode {
            id: json!("episode:1"),
            session_id: "sess-9".into(),
            timestamp: json!("2026-04-02T18:00:00Z"),
            abstract_text: "Talked about the release pipeline.".into(),
            overview: None,
            content: Some("We tag v3 and CI drafts the release.".into()),
            embedding: None,
            log_number: Some(42),
            provenance: Some("human".into()),
            access_count: 1,
        },
        score: 0.66,
        distance: 0.3,
    }
}

fn traversal_tree() -> TraversalNode {
    TraversalNode {
        entity: EntitySummary {
            id: json!("entity:rust"),
            name: "Rust".into(),
            entity_type: EntityType::Tool,
            abstract_text: "Systems language with ownership.".into(),
        },
        edges: vec![TraversalEdge {
            rel_type: "USES".into(),
            direction: "->".into(),
            target: TraversalNode {
                entity: EntitySummary {
                    id: json!("entity:cargo"),
                    name: "Cargo".into(),
                    entity_type: EntityType::Tool,
                    abstract_text: "Rust's build tool.".into(),
                },
                edges: Vec::new(),
            },
            valid_from: json!("2026-01-01T00:00:00Z"),
            valid_until: None,
            confidence: 0.62,
        }],
    }
}

// ── Harness ──────────────────────────────────────────────────────────────

fn server(outcome: Outcome) -> McpServer<Stub> {
    McpServer::new(Stub::new(outcome))
}

async fn send(server: &McpServer<Stub>, message: Value) -> Option<Value> {
    server.handle_line(&message.to_string()).await
}

/// Send and require an answer — for requests, which always get one.
async fn answer(server: &McpServer<Stub>, message: Value) -> Value {
    send(server, message)
        .await
        .expect("a request must be answered")
}

fn call(id: u32, tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
}

fn text_of(result: &Value) -> &str {
    result["content"][0]["text"]
        .as_str()
        .expect("a text content block")
}

// ── Initialization ───────────────────────────────────────────────────────

#[tokio::test]
async fn initialize_answers_with_capabilities_and_identity() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PREFERRED_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        }),
    )
    .await;

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("error").is_none(), "{response}");

    let result = &response["result"];
    assert_eq!(result["protocolVersion"], PREFERRED_PROTOCOL_VERSION);
    assert!(result["capabilities"]["tools"].is_object(), "{result}");
    assert_eq!(result["serverInfo"]["name"], "recall-echo");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    // The handshake is the only place to tell an agent when to use memory.
    let instructions = result["instructions"].as_str().unwrap();
    assert!(instructions.contains("recall_query"), "{instructions}");
}

#[tokio::test]
async fn initialize_offers_our_version_when_the_client_asks_for_one_we_lack() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "1900-01-01" }
        }),
    )
    .await;
    assert_eq!(
        response["result"]["protocolVersion"],
        PREFERRED_PROTOCOL_VERSION
    );
}

#[tokio::test]
async fn initialize_survives_absent_params() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
    )
    .await;
    assert_eq!(
        response["result"]["protocolVersion"],
        PREFERRED_PROTOCOL_VERSION
    );
}

#[tokio::test]
async fn notifications_are_never_answered() {
    let server = server(Outcome::Canned);
    assert!(send(
        &server,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .await
    .is_none());
    assert!(send(
        &server,
        json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 3 } })
    )
    .await
    .is_none());
    // Even an id on a notification method must not produce a response.
    assert!(send(
        &server,
        json!({ "jsonrpc": "2.0", "id": 7, "method": "notifications/initialized" })
    )
    .await
    .is_none());
}

#[tokio::test]
async fn ping_answers_an_empty_result() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
    )
    .await;
    assert_eq!(response["result"], json!({}));
}

// ── tools/list ───────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_list_describes_every_tool() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
    )
    .await;

    let tools = response["result"]["tools"]
        .as_array()
        .expect("a tool array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "recall_query",
            "recall_search",
            "recall_episodes",
            "recall_traverse",
            "recall_status"
        ]
    );

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(tool["title"].is_string(), "{name} has no title");
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 80),
            "{name} has no usable description"
        );
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{name} schema is not an object");
        assert!(schema["properties"].is_object(), "{name} has no properties");
    }
}

#[tokio::test]
async fn tools_list_is_stable_across_calls() {
    let server = server(Outcome::Canned);
    let first = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    let second = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    assert_eq!(first["result"], second["result"]);
}

#[tokio::test]
async fn tools_list_rejects_a_cursor_it_never_issued() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {"cursor": "page-2"}}),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602);
}

// ── tools/call, happy paths ──────────────────────────────────────────────

#[tokio::test]
async fn every_advertised_tool_can_be_called() {
    let server = server(Outcome::Canned);
    let listed = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;

    for tool in listed["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let arguments = match name {
            "recall_traverse" => json!({ "entity": "Rust" }),
            "recall_status" => json!({}),
            _ => json!({ "query": "release pipeline" }),
        };
        let response = answer(&server, call(9, name, arguments)).await;
        assert!(
            response.get("error").is_none(),
            "{name} produced a protocol error: {response}"
        );
        assert_eq!(response["result"]["isError"], false, "{name}: {response}");
        assert!(
            !text_of(&response["result"]).is_empty(),
            "{name} returned no text"
        );
    }
}

#[tokio::test]
async fn recall_search_returns_named_entities_with_scores() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        call(
            1,
            "recall_search",
            json!({ "query": "ownership", "limit": 3 }),
        ),
    )
    .await;

    let text = text_of(&response["result"]);
    assert!(text.contains("Rust [tool]"), "{text}");
    assert!(text.contains("score 0.84"), "{text}");
    assert!(text.contains("Systems language with ownership."), "{text}");
    // Not a JSON dump.
    assert!(!text.contains("\"entity\""), "{text}");
}

#[tokio::test]
async fn recall_query_reaches_the_daemon_with_the_arguments_it_was_given() {
    let stub = Stub::new(Outcome::Canned);
    let server = McpServer::new(stub);
    answer(
        &server,
        call(
            1,
            "recall_query",
            json!({ "query": "how we ship", "limit": 4, "include_episodes": false }),
        ),
    )
    .await;

    // The daemon protocol is the only path to the store; assert we speak it.
    let Request::Query(args) = &server.backend_calls()[0] else {
        panic!("recall_query must issue a Query request");
    };
    assert_eq!(args.query, "how we ship");
    assert_eq!(args.limit, 4);
    assert!(!args.episodes);
}

#[tokio::test]
async fn recall_query_renders_entities_and_conversation_fragments() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        call(1, "recall_query", json!({ "query": "release" })),
    )
    .await;
    let text = text_of(&response["result"]);
    assert!(text.contains("1 entity:"), "{text}");
    assert!(text.contains("1 conversation fragment:"), "{text}");
    assert!(
        text.contains("We tag v3 and CI drafts the release."),
        "{text}"
    );
}

#[tokio::test]
async fn recall_traverse_renders_a_tree_with_confidence() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        call(
            1,
            "recall_traverse",
            json!({ "entity": "Rust", "depth": 1 }),
        ),
    )
    .await;
    let text = text_of(&response["result"]);
    assert!(text.contains("Relationships from \"Rust\""), "{text}");
    assert!(text.contains("USES Cargo"), "{text}");
    assert!(text.contains("[62%]"), "{text}");
}

#[tokio::test]
async fn recall_status_reports_counts() {
    let server = server(Outcome::Canned);
    let response = answer(&server, call(1, "recall_status", json!({}))).await;
    let text = text_of(&response["result"]);
    assert!(text.contains("12 entities"), "{text}");
    assert!(text.contains("400 conversation episodes"), "{text}");
    assert!(text.contains("project 7"), "{text}");
}

#[tokio::test]
async fn a_tool_call_without_arguments_uses_the_defaults() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "recall_status" }
        }),
    )
    .await;
    assert_eq!(response["result"]["isError"], false, "{response}");
}

#[tokio::test]
async fn an_empty_result_set_explains_itself() {
    let server = server(Outcome::Data(json!([])));
    let response = answer(
        &server,
        call(1, "recall_search", json!({ "query": "quantum" })),
    )
    .await;
    assert_eq!(response["result"]["isError"], false);
    let text = text_of(&response["result"]);
    assert!(text.contains("No entities"), "{text}");
    assert!(text.contains("recall_episodes"), "{text}");
}

// ── tools/call, failure paths ────────────────────────────────────────────

#[tokio::test]
async fn a_backend_failure_is_a_tool_error_not_a_protocol_error() {
    let server = server(Outcome::Remote {
        code: "not_found",
        message: "entity not found: Rsut",
    });
    let response = answer(
        &server,
        call(1, "recall_traverse", json!({ "entity": "Rsut" })),
    )
    .await;

    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], true);
    let text = text_of(&response["result"]);
    assert!(text.contains("recall_traverse failed"), "{text}");
    assert!(text.contains("entity not found: Rsut"), "{text}");
    assert!(text.contains("recall_search"), "{text}");
}

#[tokio::test]
async fn an_unavailable_store_tells_the_agent_not_to_retry() {
    let server = server(Outcome::Remote {
        code: "embedding",
        message: "embedding model unavailable",
    });
    let response = answer(&server, call(1, "recall_search", json!({ "query": "x" }))).await;
    assert_eq!(response["result"]["isError"], true);
    assert!(text_of(&response["result"]).contains("do not retry"));
}

#[tokio::test]
async fn invalid_arguments_are_a_tool_error_the_model_can_fix() {
    let server = server(Outcome::Canned);

    let missing = answer(&server, call(1, "recall_search", json!({}))).await;
    assert!(missing.get("error").is_none(), "{missing}");
    assert_eq!(missing["result"]["isError"], true);
    assert!(text_of(&missing["result"]).contains("`query` is required"));

    let mistyped = answer(&server, call(2, "recall_search", json!({ "query": 7 }))).await;
    assert_eq!(mistyped["result"]["isError"], true);
    assert!(text_of(&mistyped["result"]).contains("must be a string"));
}

#[tokio::test]
async fn an_unreadable_payload_fails_the_call_rather_than_the_process() {
    let server = server(Outcome::Data(json!({ "unexpected": "shape" })));
    let response = answer(&server, call(1, "recall_search", json!({ "query": "x" }))).await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(text_of(&response["result"]).contains("could not read"));
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error() {
    let server = server(Outcome::Canned);
    let response = answer(&server, call(1, "recall_forget", json!({}))).await;

    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], -32602);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("recall_forget"));
    // The client is told what it could have called instead.
    assert!(response["error"]["data"]["available"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "recall_query"));
}

#[tokio::test]
async fn a_tool_call_without_a_name_is_a_protocol_error() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}}),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602);
}

// ── Malformed and unknown traffic ────────────────────────────────────────

#[tokio::test]
async fn an_unknown_method_is_method_not_found() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}),
    )
    .await;
    assert_eq!(response["id"], 5);
    assert_eq!(response["error"]["code"], -32601);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("resources/list"));
}

#[tokio::test]
async fn malformed_json_is_a_parse_error_against_a_null_id() {
    let server = server(Outcome::Canned);
    let response = server
        .handle_line("{\"jsonrpc\": \"2.0\", \"id\": 1, ")
        .await
        .expect("a parse error must be reported");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], Value::Null);
    assert_eq!(response["error"]["code"], -32700);
}

#[tokio::test]
async fn a_request_without_a_method_is_an_invalid_request() {
    let server = server(Outcome::Canned);
    let response = answer(&server, json!({"jsonrpc": "2.0", "id": 6, "params": {}})).await;
    assert_eq!(response["id"], 6);
    assert_eq!(response["error"]["code"], -32600);
}

#[tokio::test]
async fn a_foreign_jsonrpc_version_is_rejected() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!({"jsonrpc": "1.0", "id": 7, "method": "ping"}),
    )
    .await;
    assert_eq!(response["error"]["code"], -32600);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("2.0"));
}

#[tokio::test]
async fn a_structurally_invalid_message_is_answered_against_a_null_id() {
    let server = server(Outcome::Canned);
    // Not a notification — a notification is a *valid* request without an id,
    // and JSON-RPC 2.0 answers anything else against a null id.
    for message in [
        json!({"jsonrpc": "2.0", "params": {}}),
        json!({"jsonrpc": "9.9", "method": "ping"}),
    ] {
        let response = answer(&server, message).await;
        assert_eq!(response["id"], Value::Null, "{response}");
        assert_eq!(response["error"]["code"], -32600, "{response}");
    }
}

#[tokio::test]
async fn a_null_id_is_treated_as_a_notification() {
    let server = server(Outcome::Canned);
    assert!(send(
        &server,
        json!({"jsonrpc": "2.0", "id": null, "method": "ping"})
    )
    .await
    .is_none());
}

#[tokio::test]
async fn a_non_object_message_is_rejected_without_panicking() {
    let server = server(Outcome::Canned);
    let response = answer(&server, json!("hello")).await;
    assert_eq!(response["error"]["code"], -32600);
    assert_eq!(response["id"], Value::Null);
}

#[tokio::test]
async fn a_batch_is_answered_once_per_request() {
    let server = server(Outcome::Canned);
    let response = answer(
        &server,
        json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
        ]),
    )
    .await;

    let responses = response.as_array().expect("a batch response");
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
}

#[tokio::test]
async fn a_batch_of_only_notifications_is_silent() {
    let server = server(Outcome::Canned);
    assert!(send(
        &server,
        json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}])
    )
    .await
    .is_none());
}

#[tokio::test]
async fn an_empty_batch_is_an_invalid_request() {
    let server = server(Outcome::Canned);
    let response = answer(&server, json!([])).await;
    assert_eq!(response["error"]["code"], -32600);
}

// ── Read-only contract ───────────────────────────────────────────────────

#[tokio::test]
async fn no_tool_can_write_to_the_graph() {
    let server = server(Outcome::Canned);
    let listed = answer(
        &server,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;

    for tool in listed["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let arguments = match name {
            "recall_traverse" => json!({ "entity": "Rust" }),
            "recall_status" => json!({}),
            _ => json!({ "query": "anything" }),
        };
        answer(&server, call(2, name, arguments)).await;
    }

    for request in server.backend_calls() {
        assert!(
            request.is_retryable(),
            "{} mutates the graph; MCP tools are read-only",
            request.op_name()
        );
        assert!(
            !matches!(
                request,
                Request::AddEntity(_)
                    | Request::Relate(_)
                    | Request::IngestArchive(_)
                    | Request::SyncPipeline(_)
                    | Request::Feedback(_)
                    | Request::Shutdown
            ),
            "{} is not a read operation",
            request.op_name()
        );
    }
}

/// Access to what the stub recorded.
trait BackendCalls {
    fn backend_calls(&self) -> Vec<Request>;
}

impl BackendCalls for McpServer<Stub> {
    fn backend_calls(&self) -> Vec<Request> {
        self.backend().calls.lock().unwrap().clone()
    }
}
