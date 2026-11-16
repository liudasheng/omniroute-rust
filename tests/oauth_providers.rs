//! Subscription provider contracts use local mock servers and synthetic tokens.
use axum::{routing::{get, post}, Router, Json, extract::{State, Query, Path}, http::HeaderMap, response::IntoResponse};
use omniroute_rust::{oauth::OAuthCredentials, server::providers_admin::ProviderConnection, state::AppState};
use serde_json::{json, Value};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

#[derive(Clone, Default)]
struct Mock {
    refreshes: Arc<AtomicUsize>,
    invalid_catalog: Arc<AtomicUsize>,
    mislabeled_sse: Arc<AtomicUsize>,
    limited_model: Arc<std::sync::Mutex<Option<String>>>,
    requests: Arc<std::sync::Mutex<Vec<(HeaderMap, Value)>>>,
}
async fn token(State(mock): State<Mock>, body: String) -> Json<Value> {
    mock.refreshes.fetch_add(1, Ordering::SeqCst);
    assert!(body.contains("grant_type=refresh_token"));
    assert!(!body.contains("scope="), "Codex refresh must not re-scope token families");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    Json(json!({"access_token":"new-access", "refresh_token":"rotated-refresh", "expires_in":3600}))
}
async fn codex(State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> axum::response::Response {
    mock.requests.lock().unwrap().push((headers.clone(), body));
    if headers.get("authorization").unwrap() != "Bearer new-access" {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    let events = [
        json!({"type":"response.created","response":{"id":"resp_test","model":"gpt-5.4"}}),
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}}),
    ];
    let content_type = if mock.mislabeled_sse.load(Ordering::SeqCst) > 0 { "application/json" } else { "text/event-stream" };
    let chunks = events.iter().map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())).collect::<String>();
    ([("content-type",content_type)], chunks).into_response()
}
async fn antigravity(State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> axum::response::Response {
    let limited = mock.limited_model.lock().unwrap().as_deref()
        .is_some_and(|model| body["model"].as_str() == Some(model));
    mock.requests.lock().unwrap().push((headers, body));
    if limited {
        return (axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Resource has been exhausted"}}))).into_response();
    }
    let frame = json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}});
    ([("content-type","text/event-stream")], format!("data: {frame}\n\n")).into_response()
}
async fn codex_models(State(mock): State<Mock>, Query(query): Query<std::collections::HashMap<String, String>>, headers: HeaderMap) -> axum::response::Response {
    assert!(query.get("client_version").is_some_and(|v| !v.is_empty()));
    assert_eq!(headers["originator"], "codex_cli_rs");
    assert_eq!(headers["chatgpt-account-id"], "workspace-test");
    mock.requests.lock().unwrap().push((headers.clone(), json!({"discovery":true})));
    if headers["authorization"] != "Bearer new-access" {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    if mock.invalid_catalog.load(Ordering::SeqCst) > 0 {
        return Json(json!({"detail":"not a catalog"})).into_response();
    }
    Json(json!({"models":[{"slug":"gpt-6.1-sol","context_window":272000},
        {"slug":"not-api","supported_in_api":false}]})).into_response()
}
async fn antigravity_models(State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer "));
    assert!(headers.get("x-goog-api-key").is_none());
    assert!(headers["user-agent"].to_str().unwrap().starts_with("antigravity/"));
    assert_eq!(body["project"], "test-project");
    mock.requests.lock().unwrap().push((headers, body));
    if mock.invalid_catalog.load(Ordering::SeqCst) > 0 {
        return Json(json!({"error":{"message":"unavailable"}}));
    }
    Json(json!({"models":{"gemini-new-live":{"displayName":"New Gemini","inputTokenLimit":1048576}}}))
}
async fn setup(provider: &str, expired: bool) -> (Arc<AppState>, tempfile::TempDir, Mock, tokio::task::JoinHandle<()>) {
    let mock = Mock::default();
    let app = Router::new().route("/token", post(token)).route("/responses",post(codex))
        .route("/v1internal:streamGenerateContent",post(antigravity))
        .route("/models",get(codex_models))
        .route("/v1internal:fetchAvailableModels",post(antigravity_models)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}",listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::for_tests(vec![],Some(dir.path().to_path_buf())));
    state.provider_connections.upsert(ProviderConnection {
        id:"account".into(), provider:provider.into(), enabled:true, base_url:Some(base.clone()),
        oauth:OAuthCredentials {
            access_token:Some("old-access".into()), refresh_token:Some("old-refresh".into()),
            expires_at:if expired {Some(1)} else {None}, project_id:Some("test-project".into()),
            provider_specific_data:Some(json!({"workspaceId":"workspace-test", "tokenUrl":format!("{base}/token")})),
            ..Default::default()
        }, ..Default::default()
    });
    omniroute_rust::server::admin::refresh_account_pool(&state,provider);
    (state,dir,mock,task)
}

async fn chat(state: Arc<AppState>, model: &str, stream: bool) -> axum::response::Response {
    omniroute_rust::core::chat_core::handle_chat(state,
        omniroute_rust::core::chat_core::ChatRequest {
            inbound_format: omniroute_rust::format::Format::OpenAI,
            body: json!({"model":model,"messages":[{"role":"user","content":"ping"}],"stream":stream}),
            model_str:model.into(), stream, endpoint:"/v1/chat/completions".into(),
            client_ip:None, reasoning_effort:None, requester:None, api_key:None, compression_header:None,
        }).await
}

#[tokio::test]
async fn subscription_model_import_persists_catalog_and_routes_new_models() {
    for (provider, model) in [("codex", "cx/gpt-6.1-sol"), ("antigravity", "agy/gemini-new-live")] {
        let (state, _dir, mock, task) = setup(provider, false).await;
        let session = state.auth.new_session();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {session}").parse().unwrap());
        let response = omniroute_rust::server::admin::provider_connections_sync_models(
            State(state.clone()), headers, Path("account".into())).await;
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 1024*1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true, "{provider}: {body}");
        assert_eq!(body["synced"], 1);
        let connection = state.provider_connections.get("account").unwrap();
        assert_eq!(connection.synced_models, vec![model.split_once('/').unwrap().1]);
        assert!(connection.synced_at_ms > 0);
        if provider == "codex" {
            assert_eq!(connection.oauth.refresh_token.as_deref(), Some("rotated-refresh"));
            assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
        }
        mock.invalid_catalog.store(1, Ordering::SeqCst);
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {session}").parse().unwrap());
        let response = omniroute_rust::server::admin::provider_connections_sync_models(
            State(state.clone()), headers, Path("account".into())).await;
        let bytes = axum::body::to_bytes(response.into_body(), 1024*1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], false, "malformed catalogs must not succeed");
        assert!(body["detail"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(state.provider_connections.get("account").unwrap().synced_models, connection.synced_models);
        let response = chat(state, model, false).await;
        assert_eq!(response.status(), 200, "new model must route: {model}");
        task.abort();
    }
}

#[tokio::test]
async fn codex_handles_mislabeled_sse_for_streaming_and_json_clients() {
    for stream in [false, true] {
        let (state, _dir, mock, task) = setup("codex", true).await;
        mock.mislabeled_sse.store(1, Ordering::SeqCst);
        let response = chat(state, "codex/gpt-5.4", stream).await;
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 1024*1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hello"), "{text}");
        if stream { assert!(text.contains("[DONE]")); }
        else { assert!(serde_json::from_str::<Value>(&text).is_ok()); }
        task.abort();
    }
}

#[tokio::test]
async fn subscription_quota_does_not_cool_other_models_on_sibling_accounts() {
    let (state, _dir, mock, task) = setup("antigravity", false).await;
    let first = state.provider_connections.get("account").unwrap();
    for id in ["sibling-1", "sibling-2"] {
        let mut connection = first.clone();
        connection.id = id.into();
        state.provider_connections.upsert(connection);
    }
    omniroute_rust::server::admin::refresh_account_pool(&state, "antigravity");
    *mock.limited_model.lock().unwrap() = Some("gemini-3-flash".into());
    let response = chat(state.clone(), "antigravity/gemini-3-flash", false).await;
    assert_eq!(response.status(), 429);
    assert_eq!(mock.requests.lock().unwrap().len(), 3, "try each account exactly once");
    for account in state.accounts_for("antigravity") {
        let key = account.circuit_key("antigravity");
        assert_eq!(state.circuits.cooldown_remaining_ms(&key), 0, "quota must not cool the whole account");
        assert!(state.circuits.account_model_cooldown_remaining_ms("antigravity", "gemini-3-flash", &key) > 0);
    }
    let response = chat(state, "antigravity/claude-sonnet-4-6", false).await;
    assert_eq!(response.status(), 200, "a different model must remain available");
    task.abort();
}

#[tokio::test]
async fn gateway_folds_codex_and_antigravity_streams_for_nonstream_clients() {
    for (provider,model) in [("codex","gpt-5.4"),("antigravity","gemini-3-flash")] {
        let (state,_dir,_mock,task)=setup(provider,true).await;
        let response=chat(state,&format!("{provider}/{model}"),false).await;
        assert_eq!(response.status(),200,"{provider}");
        let bytes=axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap();
        let body:Value=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"],"hello","{provider}: {body}");
        task.abort();
    }
}

#[tokio::test]
async fn combo_falls_back_from_codex_to_antigravity() {
    let (original,dir,mock,task)=setup("antigravity",false).await;
    let base=original.provider_connections.get("account").unwrap().base_url.unwrap();
    original.provider_connections.upsert(ProviderConnection {
        id:"codex-rejected".into(),provider:"codex".into(),enabled:true,base_url:Some(base),
        oauth:OAuthCredentials{access_token:Some("rejected".into()),..Default::default()},
        ..Default::default()
    });
    drop(original);
    let state=Arc::new(AppState::for_tests(vec![omniroute_rust::config::ComboConfig {
        name:"subscription-fallback".into(),strategy:Some("priority".into()),
        providers:vec!["codex/gpt-5.4".into(),"antigravity/gemini-3-flash".into()],models:vec![],
    }],Some(dir.path().to_path_buf())));
    let response=chat(state,"subscription-fallback",false).await;
    assert_eq!(response.status(),200);
    let bytes=axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap();
    let body:Value=serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"],"hello");
    assert!(mock.requests.lock().unwrap().len()>=2);
    task.abort();
}

#[tokio::test]
async fn subscription_account_failure_tries_sibling_before_failing_combo() {
    let (state,_dir,mock,task)=setup("codex",false).await;
    let base=state.provider_connections.get("account").unwrap().base_url.unwrap();
    // The first account has no refresh token and is rejected. The sibling
    // already has a valid access token, so provider-level failover is needless.
    let mut first=state.provider_connections.get("account").unwrap();
    first.oauth.refresh_token=None;
    state.provider_connections.upsert(first);
    state.provider_connections.upsert(ProviderConnection {
        id:"sibling".into(),provider:"codex".into(),enabled:true,base_url:Some(base),
        oauth:OAuthCredentials{access_token:Some("new-access".into()),..Default::default()},
        ..Default::default()
    });
    omniroute_rust::server::admin::refresh_account_pool(&state,"codex");
    let response=chat(state,"codex/gpt-5.4",false).await;
    assert_eq!(response.status(),200);
    assert_eq!(mock.refreshes.load(Ordering::SeqCst),0);
    assert_eq!(mock.requests.lock().unwrap().len(),2);
    task.abort();
}

#[tokio::test]
async fn gateway_streams_both_subscription_protocols_as_chat_sse() {
    for (provider,model) in [("codex","gpt-5.4"),("antigravity","gemini-3-flash")] {
        let (state,_dir,_mock,task)=setup(provider,true).await;
        let response=chat(state,&format!("{provider}/{model}"),true).await;
        assert_eq!(response.status(),200);
        let bytes=axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap();
        let text=String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hello"),"{provider}: {text}");
        assert!(text.contains("[DONE]"),"{provider}: {text}");
        task.abort();
    }
}

#[tokio::test]
async fn quota_page_lists_real_oauth_accounts_without_exposing_tokens() {
    let dir=tempfile::tempdir().unwrap();
    let state=Arc::new(AppState::for_tests(vec![],Some(dir.path().to_path_buf())));
    for (provider,count) in [("antigravity",18),("codex",1)] {
        for i in 0..count {
            state.provider_connections.upsert(ProviderConnection {
                id:format!("{provider}-{i}"),name:format!("Account {i}"),provider:provider.into(),enabled:true,
                oauth:OAuthCredentials{refresh_token:Some("private-test-refresh".into()),..Default::default()},
                ..Default::default()
            });
        }
    }
    let session=state.auth.new_session();
    let mut headers=HeaderMap::new();
    headers.insert("authorization",format!("Bearer {session}").parse().unwrap());
    let response=omniroute_rust::server::dashboard::provider_quotas(
        State(state),headers).await.into_response();
    assert_eq!(response.status(),200);
    let bytes=axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap();
    let text=String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!text.contains("private-test-refresh"));
    let value:Value=serde_json::from_str(&text).unwrap();
    assert_eq!(value["summary"]["total"],19);
    let accounts=value["accounts"].as_array().unwrap();
    assert_eq!(accounts.iter().filter(|a| a["provider"]=="antigravity").count(),18);
    assert!(accounts.iter().all(|a| a["active"]==true && a["authKind"]=="oauth" && a["quotaAvailable"]==false));
}

#[tokio::test]
async fn concurrent_refresh_is_single_flight_and_persists_rotated_token() {
    let (state,dir,mock,task) = setup("codex",true).await;
    let jobs = (0..12).map(|_| {let state=state.clone();tokio::spawn(async move {
        state.oauth.credentials(&state,"account",None).await.unwrap()
    })});
    for job in futures::future::join_all(jobs).await {assert_eq!(job.unwrap().access_token.as_deref(),Some("new-access"));}
    assert_eq!(mock.refreshes.load(Ordering::SeqCst),1);
    let store=omniroute_rust::server::providers_admin::ProviderConnectionStore::new(dir.path());
    assert_eq!(store.get("account").unwrap().oauth.refresh_token.as_deref(),Some("rotated-refresh"));
    let masked=serde_json::to_string(&store.list(true)).unwrap();
    assert!(!masked.contains("new-access") && !masked.contains("rotated-refresh") && !masked.contains("tokenUrl"));
    assert!(state.providers_with_keys().contains(&"codex".into()));
    task.abort();
}

#[tokio::test]
async fn codex_forces_responses_shape_and_refreshes_once_after_401() {
    let (state,_dir,mock,task)=setup("codex",false).await;
    let base=state.provider_connections.get("account").unwrap().base_url.unwrap();
    let response=omniroute_rust::upstream::subscription::execute(&state,"codex","account","gpt-5.4",&base,
        json!({"input":"ping","stream":false,"temperature":0.5,"max_output_tokens":10})).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(mock.refreshes.load(Ordering::SeqCst),1);
    let requests=mock.requests.lock().unwrap();assert_eq!(requests.len(),2);
    let (headers,body)=requests.last().unwrap();
    assert_eq!(headers["chatgpt-account-id"],"workspace-test");
    assert_eq!(headers["originator"],"codex_cli_rs");
    assert_eq!(body["stream"],true);assert_eq!(body["store"],false);
    assert!(body.get("temperature").is_none() && body.get("max_output_tokens").is_none());
    task.abort();
}

#[tokio::test]
async fn antigravity_wraps_gemini_request_and_requires_project() {
    let (state,_dir,mock,task)=setup("antigravity",false).await;
    let base=state.provider_connections.get("account").unwrap().base_url.unwrap();
    let response=omniroute_rust::upstream::subscription::execute(&state,"antigravity","account","gemini-3-flash",&base,
        json!({"contents":[{"role":"user","parts":[{"text":"ping"}]}]})).await.unwrap();
    assert!(response.status().is_success());
    let requests=mock.requests.lock().unwrap();let (headers,body)=requests.last().unwrap();
    assert_eq!(headers["authorization"],"Bearer old-access");
    assert!(headers.get("x-goog-api-key").is_none());
    assert_eq!(body["project"],"test-project");assert_eq!(body["requestType"],"agent");
    assert!(body["request"]["contents"].is_array());
    let mut aliased=json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"read","args":{}}}]}]});
    let oauth=state.provider_connections.get("account").unwrap().oauth;
    omniroute_rust::upstream::subscription::shape_antigravity(&mut aliased,"gemini-3.1-pro-high",&oauth,"account").unwrap();
    assert_eq!(aliased["model"],"gemini-pro-agent");
    assert_eq!(aliased["request"]["contents"][0]["parts"][0]["thoughtSignature"],"skip_thought_signature_validator");
    let schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
        "properties":{"timeout":{"type":"integer","exclusiveMinimum":0,"minimum":0},
            "$schema":{"type":"string"}, "nested":{"type":"array","items":{"type":"object",
                "properties":{"value":{"type":"number","exclusiveMinimum":1}}}}},
        "required":["timeout"]});
    let translated = omniroute_rust::translate::gemini::openai_request_to_gemini(&json!({
        "messages":[{"role":"user","content":"test"}],
        "tools":[{"type":"function","function":{"name":"probe","parameters":schema}}]}));
    let clean = &translated["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(clean.get("$schema").is_none());
    assert!(clean["properties"]["timeout"].get("exclusiveMinimum").is_none());
    assert_eq!(clean["properties"]["timeout"]["minimum"], 0);
    assert_eq!(clean["properties"]["$schema"]["type"], "string");
    assert!(clean["properties"]["nested"]["items"]["properties"]["value"].get("exclusiveMinimum").is_none());
    let mut native = json!({"contents":[], "tools":[{"functionDeclarations":[{"name":"probe","parameters":schema}]}]});
    omniroute_rust::upstream::subscription::shape_antigravity(&mut native,"gemini-3-flash",&oauth,"account").unwrap();
    assert_eq!(&native["request"]["tools"][0]["functionDeclarations"][0]["parameters"], clean);
    let user_text = "Oh My Pi <critical>user text must stay unchanged</critical>";
    let mut branded = json!({"contents":[{"role":"user","parts":[{"text":user_text}]}],
        "systemInstruction":{"parts":[{"text":"Keep this instruction.\nYou are Claude Code, a coding agent.\n<system-conventions>Oh My Pi coding harness</system-conventions>\n<critical>Keep this rule</critical>"}]}});
    omniroute_rust::upstream::subscription::shape_antigravity(&mut branded,"gemini-3-flash",&oauth,"account").unwrap();
    let system = branded["request"]["systemInstruction"]["parts"][0]["text"].as_str().unwrap();
    assert!(system.contains("Keep this instruction."));
    assert!(!system.contains("Claude Code"));
    assert!(system.contains("<conventions>AI coding assistant</conventions>"));
    assert!(system.contains("<important>Keep this rule</important>"));
    assert_eq!(branded["request"]["contents"][0]["parts"][0]["text"], user_text);
    let mut body=json!({"contents":[]});
    assert_eq!(omniroute_rust::upstream::subscription::shape_antigravity(&mut body,"gemini-3-flash",&OAuthCredentials::default(),"account").unwrap_err().status,422);
    task.abort();
}
