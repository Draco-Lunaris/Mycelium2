//! Agent-loop integration tests against a mock OpenAI-compatible LLM
//! server: the tool-call loop (search → read → answer for queries;
//! search → write for mutations), the deterministic fallback when no
//! LLM is reachable, and index.md regeneration on writes.

use axum::response::IntoResponse;
use mycelium_core::concept::Concept;
use mycelium_crypto::keys::ServiceKey;
use mycelium_librarian::agent::{self, AgentMode};
use mycelium_librarian::llm::{ConversationTurn, LlmClient, LlmConfig};
use mycelium_store::{ConceptStore, Store};

/// A minimal OpenAI-compatible mock: serves scripted responses in
/// order, keyed by the request's tool expectations. Each POST to
/// /chat/completions pops the next scripted response.
/// A minimal OpenAI-compatible mock: serves scripted responses in
/// order (LIFO). Captures every request body for protocol assertions.
type Shared = std::sync::Arc<tokio::sync::Mutex<MockState>>;
struct MockState {
    responses: Vec<serde_json::Value>,
    requests: Vec<serde_json::Value>,
}
async fn mock_llm(responses: Vec<serde_json::Value>) -> (LlmClient, Shared) {
    let state: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(MockState {
        responses,
        requests: Vec::new(),
    }));
    let handler_state = std::sync::Arc::clone(&state);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = handler_state;
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let mut guard = state.lock().await;
                guard.requests.push(parsed);
                match guard.responses.pop() {
                    Some(resp) => axum::Json(resp).into_response(),
                    None => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "script exhausted",
                    )
                        .into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = LlmClient::new(&LlmConfig {
        url: format!("http://{addr}/v1"),
        model: "mock".into(),
    });
    (client, state)
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }]
    })
}

fn tool_call_response(id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }]
            }
        }]
    })
}

async fn test_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
}

fn concept(path: &str, title: &str, body: &str) -> Concept {
    Concept::new(
        mycelium_core::concept::Frontmatter {
            concept_type: "Note".into(),
            title: Some(title.into()),
            ..Default::default()
        },
        body.into(),
        path.into(),
    )
}

#[tokio::test]
async fn query_agent_searches_reads_and_answers() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);
    cs.put(&concept(
        "/notes/zebras.md",
        "Zebra Facts",
        "Zebras have distinctive stripes.",
    ))
    .await
    .unwrap();

    // Script: (1) search_knowledge, (2) read_concept, (3) final answer.
    let (client, _mock) = mock_llm(vec![
        text_response(
            "Zebras are striped animals native to Africa.\n\nSources:\n- /notes/zebras.md",
        ),
        tool_call_response(
            "call-2",
            "read_concept",
            serde_json::json!({ "path": "/notes/zebras.md" }),
        ),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "zebra stripes" }),
        ),
    ])
    .await;

    let result = agent::run_query(&client, &cs, "What do you know about zebras?")
        .await
        .unwrap();
    assert!(result.answer.contains("striped"));
}

#[tokio::test]
async fn mutation_agent_writes_concept() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);

    // Script: (1) search (finds nothing), (2) write_concept, (3) summary.
    let (client, _mock) = mock_llm(vec![
        text_response("Recorded the billing API endpoint at /apis/billing.md."),
        tool_call_response(
            "call-2",
            "write_concept",
            serde_json::json!({
                "path": "/apis/billing.md",
                "frontmatter": { "type": "API Endpoint", "title": "Billing API" },
                "body": "The billing API charges customers."
            }),
        ),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "billing" }),
        ),
    ])
    .await;

    let result = agent::run_mutation(
        &client,
        &cs,
        "Record that the billing API charges customers.",
    )
    .await
    .unwrap();
    assert!(
        result
            .files_changed
            .contains(&"/apis/billing.md".to_string())
    );
    // The concept exists with the agent-chosen type.
    let written = cs.get("/apis/billing.md").await.unwrap();
    assert_eq!(written.frontmatter.concept_type, "API Endpoint");
}

#[tokio::test]
async fn query_agent_rejects_write_tools_in_readonly_mode() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);

    // Script: the model tries a write in query mode → the tool returns
    // an error result; the model then answers.
    let (client, _mock) = mock_llm(vec![
        text_response("I could not write in read-only mode."),
        tool_call_response(
            "call-1",
            "write_concept",
            serde_json::json!({
                "path": "/evil.md",
                "frontmatter": { "type": "Note" },
                "body": "should not be written"
            }),
        ),
    ])
    .await;

    let result = agent::run_query(&client, &cs, "write something")
        .await
        .unwrap();
    assert!(result.answer.contains("read-only"));
    // Nothing was written.
    assert!(cs.get("/evil.md").await.is_err());
}

#[tokio::test]
async fn unreachable_llm_errors_cleanly() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);
    let client = LlmClient::new(&LlmConfig {
        url: "http://127.0.0.1:1/v1".into(),
        model: "m".into(),
    });
    assert!(agent::run_query(&client, &cs, "anything").await.is_err());
}

#[tokio::test]
async fn system_prompt_contains_layout_and_types() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);
    cs.put(&concept(
        "/decisions/auth.md",
        "Auth Model",
        "how auth works",
    ))
    .await
    .unwrap();
    let prompt = agent::build_system_prompt(&cs, AgentMode::Query)
        .await
        .unwrap();
    assert!(prompt.contains("/decisions/auth.md"));
    assert!(prompt.contains("Note"));
    assert!(prompt.contains("QUERY (read-only)"));
    let mutate_prompt = agent::build_system_prompt(&cs, AgentMode::Mutate)
        .await
        .unwrap();
    assert!(mutate_prompt.contains("MUTATE"));
}

#[tokio::test]
async fn chat_mode_answers_with_history() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);

    let (client, _mock) = mock_llm(vec![text_response(
        "Hello! Ask me about your knowledge base.",
    )])
    .await;
    let history = vec![ConversationTurn::User("hi".into())];
    let reply = agent::run_chat(&client, &cs, &history).await.unwrap();
    assert!(reply.contains("Hello"));
}

#[tokio::test]
async fn index_md_regenerated_on_writes() {
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    cs.put(&concept("/notes/a.md", "Alpha", "alpha body"))
        .await
        .unwrap();
    cs.put(&concept("/notes/b.md", "Beta", "beta body"))
        .await
        .unwrap();

    // index.md is a raw FileRepo payload: readable via the repo, absent
    // from the registry.
    let repo = mycelium_store::FileRepo::new(store.user_dir(user));
    let bytes = repo
        .read("/index.md", &mycelium_store::Scope::User(master.clone()))
        .await
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("# Knowledge Base"));
    assert!(text.contains("[Alpha](/notes/a.md)"));
    assert!(text.contains("[Beta](/notes/b.md)"));
    let listing = cs.list().await.unwrap();
    assert!(
        !listing.iter().any(|e| e.path == "/index.md"),
        "index.md must never be a listed concept"
    );

    // Delete removes the entry from the index.
    cs.delete("/notes/a.md").await.unwrap();
    let bytes = repo
        .read("/index.md", &mycelium_store::Scope::User(master))
        .await
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("Alpha"));
    assert!(text.contains("Beta"));
}

#[tokio::test]
async fn service_scope_index_md_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service = ServiceKey::from_bytes(&[9u8; 32]).unwrap();
    let skills = ConceptStore::for_service(&store, service.clone(), &store.skills_dir(), "skills");
    skills
        .put(&concept("/deploy.md", "Deploy Skill", "deploy steps"))
        .await
        .unwrap();
    let repo = mycelium_store::FileRepo::new(store.skills_dir());
    let bytes = repo
        .read("/index.md", &mycelium_store::Scope::Service(service))
        .await
        .unwrap();
    assert!(String::from_utf8(bytes).unwrap().contains("Deploy Skill"));
}

#[tokio::test]
async fn conversation_protocol_tool_calls_precede_tool_results() {
    // OpenAI protocol: every role:"tool" message must directly follow
    // the assistant message carrying the tool_calls it responds to.
    // The mock captures request bodies; assert the shape on the
    // second round-trip of a two-step query.
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);
    cs.put(&concept("/notes/x.md", "X", "x body"))
        .await
        .unwrap();

    let (client, mock) = mock_llm(vec![
        text_response("done"),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "x" }),
        ),
    ])
    .await;
    agent::run_query(&client, &cs, "find x").await.unwrap();

    let guard = mock.lock().await;
    assert_eq!(guard.requests.len(), 2, "two round-trips");
    let second = &guard.requests[1];
    let messages = second["messages"].as_array().unwrap();
    // [system, user, assistant(tool_calls), tool]
    assert_eq!(messages.len(), 4, "message sequence: {messages:?}");
    assert_eq!(messages[2]["role"], "assistant");
    assert!(
        messages[2]["tool_calls"].is_array(),
        "assistant turn must carry tool_calls: {messages:?}"
    );
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(
        messages[3]["tool_call_id"], "call-1",
        "tool result must reference the call id"
    );
}

#[tokio::test]
async fn reserved_filenames_rejected_on_put() {
    // index.md/log.md/info.md are system-maintained — a concept there
    // would be silently clobbered by index regeneration.
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let cs = ConceptStore::for_user(&store, user, master);
    for path in ["/index.md", "/log.md", "/info.md", "/sub/index.md"] {
        let err = cs.put(&concept(path, "Reserved", "x")).await;
        assert!(
            matches!(err, Err(mycelium_store::ConceptStoreError::Reserved(_))),
            "{path} must be rejected"
        );
    }
    // Normal paths still work.
    cs.put(&concept("/notes/ok.md", "OK", "fine"))
        .await
        .unwrap();
}
