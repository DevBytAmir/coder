use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use coder::config::providers::{ProviderConfig, WireFormat};
use coder::proxy::server::{admin_router, public_router, ProxyState};
use coder::proxy::tokens::TokenStore;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{any, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn openai_provider(upstream: &str) -> ProviderConfig {
    ProviderConfig {
        upstream: upstream.to_string(),
        wire_format: WireFormat::OpenAiChatCompletions,
        auth_header_name: "Authorization".to_string(),
        auth_header_prefix: "Bearer".to_string(),
        path_override: None,
        default_model: None,
    }
}

fn anthropic_provider(upstream: &str) -> ProviderConfig {
    ProviderConfig {
        upstream: upstream.to_string(),
        wire_format: WireFormat::AnthropicMessages,
        auth_header_name: "x-api-key".to_string(),
        auth_header_prefix: String::new(),
        path_override: None,
        default_model: None,
    }
}

fn state_with(providers: Vec<(&str, ProviderConfig, Option<&str>)>) -> ProxyState {
    let mut provider_map = HashMap::new();
    let mut real_keys = HashMap::new();
    for (name, cfg, key) in providers {
        provider_map.insert(name.to_string(), cfg);
        if let Some(k) = key {
            real_keys.insert(name.to_string(), k.to_string());
        }
    }
    ProxyState {
        providers: provider_map,
        real_keys,
        tokens: TokenStore::new(),
        admin_token: None,
        http_client: reqwest::Client::new(),
    }
}

fn test_state(upstream: &str, real_key: &str) -> ProxyState {
    state_with(vec![("test", openai_provider(upstream), Some(real_key))])
}

/// A mock that fails the test (on `MockServer` drop) if any request reaches the upstream.
async fn forbid_any_upstream_request(server: &MockServer) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(server)
        .await;
}

async fn post_json(
    app: Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, Vec<u8>, Option<String>) {
    let mut req = Request::post(uri).header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let response = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, bytes.to_vec(), content_type)
}

#[tokio::test]
async fn valid_token_forwards_with_real_key_injected() {
    let mock_upstream = MockServer::start().await;
    Mock::given(header("Authorization", "Bearer real-key-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&mock_upstream)
        .await;

    let state = test_state(&mock_upstream.uri(), "real-key-123");
    let token = state
        .tokens
        .mint("test".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));

    let (status, body, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "x"}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"ok": true})
    );
}

#[tokio::test]
async fn invalid_token_is_rejected_before_reaching_upstream() {
    let mock_upstream = MockServer::start().await;
    // wiremock 0.6 answers unmatched requests with 404 rather than panicking, so an
    // explicit expect(0) mock is what proves the upstream was never contacted
    forbid_any_upstream_request(&mock_upstream).await;
    let state = test_state(&mock_upstream.uri(), "real-key-123");
    let app = public_router(Arc::new(state));

    let (status, _, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", "Bearer not-a-real-token")],
        json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_mint_then_public_lookup_round_trip() {
    let mock_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", "Bearer k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "resp-1"})))
        .expect(1)
        .mount(&mock_upstream)
        .await;

    let state = Arc::new(test_state(&mock_upstream.uri(), "k"));
    let (status, body, _) = post_json(
        admin_router(state.clone()),
        "/admin/tokens",
        &[],
        json!({"provider": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body, _) = post_json(
        public_router(state),
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "x", "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["id"],
        "resp-1"
    );
}

#[tokio::test]
async fn admin_rejects_minting_a_token_for_an_unknown_provider() {
    let state = Arc::new(test_state("http://example.invalid", "k"));
    let (status, _, _) = post_json(
        admin_router(state),
        "/admin/tokens",
        &[],
        json!({"provider": "not-a-real-provider"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_requires_admin_token_when_configured() {
    let mut state = test_state("http://example.invalid", "k");
    state.admin_token = Some("admin-secret".into());
    let state = Arc::new(state);
    let body = json!({"provider": "test"});

    let (status, _, _) = post_json(
        admin_router(state.clone()),
        "/admin/tokens",
        &[],
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let wrong = [("Authorization", "Bearer wrong")];
    let (status, _, _) = post_json(
        admin_router(state.clone()),
        "/admin/tokens",
        &wrong,
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let right = [("Authorization", "Bearer admin-secret")];
    let (status, _, _) = post_json(admin_router(state), "/admin/tokens", &right, body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn admin_token_gates_status_and_revoke_too() {
    let mut state = test_state("http://example.invalid", "k");
    state.admin_token = Some("admin-secret".into());
    let state = Arc::new(state);
    let minted = state
        .tokens
        .mint("test".into(), None, Duration::from_secs(60));

    let call = |req: axum::http::request::Builder| {
        let app = admin_router(state.clone());
        async move {
            app.oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    let revoke_uri = format!("/admin/tokens/{minted}");

    assert_eq!(
        call(Request::get("/admin/status")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(Request::delete(revoke_uri.as_str())).await,
        StatusCode::UNAUTHORIZED
    );
    // an unauthorized revoke must not have revoked anything
    assert!(state.tokens.lookup(&minted).is_some());

    let auth = ("Authorization", "Bearer admin-secret");
    assert_eq!(
        call(Request::get("/admin/status").header(auth.0, auth.1)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(Request::delete(revoke_uri.as_str()).header(auth.0, auth.1)).await,
        StatusCode::NO_CONTENT
    );
    assert!(state.tokens.lookup(&minted).is_none());
}

#[tokio::test]
async fn anthropic_route_accepts_x_api_key_and_injects_real_key() {
    let mock_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "real-anthropic-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"type": "message"})))
        .expect(1)
        .mount(&mock_upstream)
        .await;

    let state = state_with(vec![(
        "anthropic",
        anthropic_provider(&mock_upstream.uri()),
        Some("real-anthropic-key"),
    )]);
    let token = state
        .tokens
        .mint("anthropic".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));

    // Claude Code posts to /v1/messages?beta=true
    let (status, body, _) = post_json(
        app,
        "/v1/messages?beta=true",
        &[("x-api-key", &token), ("anthropic-version", "2023-06-01")],
        json!({"model": "claude-x", "max_tokens": 10, "messages": []}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["type"],
        "message"
    );
    let received = mock_upstream.received_requests().await.unwrap();
    assert!(!received[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn expired_token_is_rejected_before_reaching_upstream() {
    let mock_upstream = MockServer::start().await;
    forbid_any_upstream_request(&mock_upstream).await;
    let state = test_state(&mock_upstream.uri(), "k");
    let token = state
        .tokens
        .mint("test".into(), None, Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let app = public_router(Arc::new(state));

    let (status, _, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_for_one_provider_is_rejected_on_another_formats_route() {
    let openai_upstream = MockServer::start().await;
    let anthropic_upstream = MockServer::start().await;
    forbid_any_upstream_request(&openai_upstream).await;
    forbid_any_upstream_request(&anthropic_upstream).await;
    let state = state_with(vec![
        ("ollama", openai_provider(&openai_upstream.uri()), None),
        (
            "anthropic",
            anthropic_provider(&anthropic_upstream.uri()),
            Some("real"),
        ),
    ]);
    let ollama_token = state
        .tokens
        .mint("ollama".into(), None, Duration::from_secs(60));
    let anthropic_token = state
        .tokens
        .mint("anthropic".into(), None, Duration::from_secs(60));
    let state = Arc::new(state);

    let (status, _, _) = post_json(
        public_router(state.clone()),
        "/v1/messages",
        &[("x-api-key", &ollama_token)],
        json!({"model": "m"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = post_json(
        public_router(state),
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {anthropic_token}"))],
        json!({"model": "m"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn both_token_headers_are_stripped_before_forwarding() {
    let mock_upstream = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&mock_upstream)
        .await;

    // openai-format provider whose real key goes in Authorization
    let state = test_state(&mock_upstream.uri(), "real-key");
    let token = state
        .tokens
        .mint("test".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));
    let (status, _, _) = post_json(
        app,
        "/v1/chat/completions",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("x-api-key", &token),
            ("host", "tool-container"),
            ("accept-encoding", "gzip"),
            ("connection", "keep-alive"),
        ],
        json!({"model": "m"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // anthropic-format provider whose real key goes in x-api-key
    let state = state_with(vec![(
        "anthropic",
        anthropic_provider(&mock_upstream.uri()),
        Some("real-anthropic"),
    )]);
    let token2 = state
        .tokens
        .mint("anthropic".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));
    let (status, _, _) = post_json(
        app,
        "/v1/messages",
        &[
            ("x-api-key", &token2),
            ("Authorization", &format!("Bearer {token2}")),
        ],
        json!({"model": "m"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let received = mock_upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);

    let first = &received[0].headers;
    assert_eq!(first.get("authorization").unwrap(), "Bearer real-key");
    assert!(!first.contains_key("x-api-key"));
    assert_ne!(first.get("host").unwrap(), "tool-container");
    assert!(!first.contains_key("accept-encoding"));

    let second = &received[1].headers;
    assert_eq!(second.get("x-api-key").unwrap(), "real-anthropic");
    assert!(!second.contains_key("authorization"));

    for req in &received {
        for (name, value) in &req.headers {
            let v = value.to_str().unwrap_or("");
            assert!(
                !v.contains(&token) && !v.contains(&token2),
                "session token leaked upstream in header {name}"
            );
        }
    }
}

#[tokio::test]
async fn provider_without_a_loaded_key_is_forwarded_without_auth() {
    let mock_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": 1})))
        .expect(1)
        .mount(&mock_upstream)
        .await;
    // upstream configured with a trailing /v1, as in the default ollama provider
    let state = state_with(vec![(
        "ollama",
        openai_provider(&format!("{}/v1", mock_upstream.uri())),
        None,
    )]);
    let token = state
        .tokens
        .mint("ollama".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));

    let (status, _, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "qwen"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let received = mock_upstream.received_requests().await.unwrap();
    assert!(!received[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn streaming_response_is_passed_through_not_json_parsed() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let mock_upstream = MockServer::start().await;
    Mock::given(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&mock_upstream)
        .await;
    let state = test_state(&mock_upstream.uri(), "k");
    let token = state
        .tokens
        .mint("test".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));

    let (status, body, content_type) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "m", "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.unwrap().starts_with("text/event-stream"));
    assert_eq!(String::from_utf8(body).unwrap(), sse);
}

#[tokio::test]
async fn non_json_upstream_error_is_passed_through_not_nulled() {
    let mock_upstream = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503).set_body_raw("upstream overloaded", "text/plain"))
        .mount(&mock_upstream)
        .await;
    let state = test_state(&mock_upstream.uri(), "k");
    let token = state
        .tokens
        .mint("test".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));

    let (status, body, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "m"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, b"upstream overloaded");
}

#[tokio::test]
async fn session_model_overrides_requested_model() {
    let mock_upstream = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&mock_upstream)
        .await;
    let state = test_state(&mock_upstream.uri(), "k");
    let token = state.tokens.mint(
        "test".into(),
        Some("authorized-model".into()),
        Duration::from_secs(60),
    );
    let app = public_router(Arc::new(state));

    let (status, _, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "coder/default", "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let received = mock_upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(sent["model"], "authorized-model");
}

#[tokio::test]
async fn unpinned_session_strips_a_coder_prefix_from_the_requested_model() {
    let mock_upstream = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&mock_upstream)
        .await;
    let state = test_state(&mock_upstream.uri(), "k");
    // No model pinned at mint time: the client (e.g. opencode) picks freely.
    let token = state
        .tokens
        .mint("test".into(), None, Duration::from_secs(60));
    let app = public_router(Arc::new(state));

    let (status, _, _) = post_json(
        app,
        "/v1/chat/completions",
        &[("Authorization", &format!("Bearer {token}"))],
        json!({"model": "coder/some/real-model", "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let received = mock_upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(sent["model"], "some/real-model");
}
