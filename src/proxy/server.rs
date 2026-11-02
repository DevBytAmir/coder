use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::providers::{ProviderConfig, WireFormat};
use crate::proxy::tokens::{Session, TokenStore};
use crate::proxy::translator::{apply_real_key, translator_for, upstream_url, AgentRequest};

pub struct ProxyState {
    pub providers: HashMap<String, ProviderConfig>,
    pub real_keys: HashMap<String, String>,
    pub tokens: TokenStore,
    /// Shared secret required (as `Authorization: Bearer`) on every admin call when set.
    pub admin_token: Option<String>,
    /// One pooled client for every proxied upstream request, so repeated calls to the
    /// same provider reuse a keep-alive connection instead of a fresh TCP+TLS handshake.
    pub http_client: reqwest::Client,
}

#[derive(Deserialize)]
struct MintRequest {
    provider: String,
    model: Option<String>,
}

#[derive(Serialize)]
struct MintResponse {
    token: String,
}

#[derive(Serialize)]
struct StatusResponse {
    providers_loaded: Vec<String>,
}

/// Sliding idle timeout: a token in active use never expires; one left unused this long
/// (e.g. after a crashed `coder run` that never revoked it) does.
const TOKEN_IDLE_TTL: Duration = Duration::from_hours(12);

async fn mint_token(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<MintRequest>,
) -> Response {
    if !state.providers.contains_key(&req.provider) {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown provider `{}`", req.provider),
        )
            .into_response();
    }
    let token = state.tokens.mint(req.provider, req.model, TOKEN_IDLE_TTL);
    (StatusCode::OK, Json(MintResponse { token })).into_response()
}

async fn revoke_token(
    State(state): State<Arc<ProxyState>>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    state.tokens.revoke(&token);
    StatusCode::NO_CONTENT
}

async fn status(State(state): State<Arc<ProxyState>>) -> impl IntoResponse {
    let providers_loaded: Vec<String> = state.real_keys.keys().cloned().collect();
    (StatusCode::OK, Json(StatusResponse { providers_loaded }))
}

// defense in depth on top of the loopback-only bind: admin calls must carry the admin secret
async fn require_admin_token(
    State(state): State<Arc<ProxyState>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.admin_token {
        let presented = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if presented != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(request).await
}

pub fn admin_router(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/admin/tokens", post(mint_token))
        .route("/admin/tokens/:token", delete(revoke_token))
        .route("/admin/status", get(status))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_token,
        ))
        .with_state(state)
}

/// Extracts the session token from either header a tool may send it in: `x-api-key`
/// (Claude Code with `ANTHROPIC_API_KEY`) or `Authorization: Bearer` (OpenAI-style clients,
/// Claude Code with `ANTHROPIC_AUTH_TOKEN`). Neither header is ever forwarded upstream.
fn extract_token(headers: &HeaderMap) -> Option<String> {
    let from_api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let from_bearer = || {
        headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
    };
    from_api_key.or_else(from_bearer)
}

fn is_event_stream(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

/// Resolves the request's session token to its provider config, and rejects a token used
/// against the wrong wire-format endpoint (no cross-format forwarding).
fn authorize<'a>(
    headers: &HeaderMap,
    route_format: WireFormat,
    state: &'a ProxyState,
) -> Result<(Session, &'a ProviderConfig), Box<Response>> {
    let Some(token) = extract_token(headers) else {
        return Err(Box::new(StatusCode::UNAUTHORIZED.into_response()));
    };
    let Some(session) = state.tokens.lookup(&token) else {
        return Err(Box::new(StatusCode::UNAUTHORIZED.into_response()));
    };
    let Some(provider) = state.providers.get(&session.provider) else {
        return Err(Box::new(StatusCode::UNAUTHORIZED.into_response()));
    };
    if provider.wire_format != route_format {
        return Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                format!(
                    "session token is for provider `{}` ({:?}), not this {:?} endpoint",
                    session.provider, provider.wire_format, route_format
                ),
            )
                .into_response(),
        ));
    }
    Ok((session, provider))
}

/// The token is the authorization for one specific model, so the session's model
/// overrides whatever the tool asked for (rather than rejecting): tools like opencode
/// send a fixed placeholder model name and rely on this.
fn apply_session_model(body: &mut Value, session: &Session) {
    if let (Some(model), Some(obj)) = (&session.model, body.as_object_mut()) {
        if obj.get("model") != Some(&Value::String(model.clone())) {
            eprintln!(
                "coder-proxy: session for `{}` overrides requested model with `{model}`",
                session.provider
            );
        }
        obj.insert("model".to_string(), Value::String(model.clone()));
    } else if let Some(Value::String(requested)) = body.get_mut("model") {
        // No model pinned: the client (e.g. opencode's /models picker) chooses freely.
        // Defensive only: coder's own config never writes a "coder/"-prefixed model id
        // into a request body, but strip it if some client sends its local namespaced
        // key verbatim instead of the bare upstream id, so it isn't rejected by mistake.
        if let Some(bare) = requested.strip_prefix("coder/") {
            *requested = bare.to_string();
        }
    }
}

async fn handle_public_request(
    state: Arc<ProxyState>,
    route_format: WireFormat,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (session, provider) = match authorize(&headers, route_format, &state) {
        Ok(pair) => pair,
        Err(resp) => return *resp,
    };

    let mut body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response()
        }
    };
    apply_session_model(&mut body, &session);
    let wants_stream = body.get("stream") == Some(&Value::Bool(true));

    let agent_req = AgentRequest {
        path: provider
            .path_override
            .clone()
            .unwrap_or_else(|| route_format.default_path().to_string()),
        headers,
        body,
    };

    let translator = translator_for(provider.wire_format);
    let mut upstream_req = match translator.to_upstream(agent_req, provider) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // providers with no CODER_KEY_* loaded (e.g. Ollama) are forwarded without auth
    let real_key = state.real_keys.get(&session.provider).map(String::as_str);
    if let Err(e) = apply_real_key(&mut upstream_req, provider, real_key) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    let url = upstream_url(&provider.upstream, &upstream_req.path);
    let body_bytes = match serde_json::to_vec(&upstream_req.body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let upstream_resp = match state
        .http_client
        .request(upstream_req.method, &url)
        .headers(upstream_req.headers)
        .body(body_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("coder-proxy: {} -> {url}: {e}", session.provider);
            return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
        }
    };

    let status = upstream_resp.status();
    let content_type = upstream_resp.headers().get(CONTENT_TYPE).cloned();
    eprintln!(
        "coder-proxy: {} -> {url}: {status} (stream requested: {wants_stream})",
        session.provider
    );

    // streamed responses (SSE) are relayed chunk by chunk, never buffered or JSON-parsed
    if is_event_stream(content_type.as_ref()) || (wants_stream && status.is_success()) {
        let mut builder = Response::builder().status(status).header(
            CONTENT_TYPE,
            content_type.unwrap_or_else(|| HeaderValue::from_static("text/event-stream")),
        );
        builder = builder.header(CACHE_CONTROL, "no-cache");
        return builder
            .body(Body::from_stream(upstream_resp.bytes_stream()))
            .unwrap_or_else(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            });
    }

    let bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
        return match translator.from_upstream(status, json) {
            Ok((out_status, out_body)) => (out_status, Json(out_body)).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }
    // non-JSON bodies (e.g. a plain-text upstream error) pass through untouched
    let mut resp = (status, bytes).into_response();
    if let Some(ct) = content_type {
        resp.headers_mut().insert(CONTENT_TYPE, ct);
    }
    resp
}

async fn handle_openai(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_public_request(state, WireFormat::OpenAiChatCompletions, headers, body).await
}

async fn handle_anthropic(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_public_request(state, WireFormat::AnthropicMessages, headers, body).await
}

/// Agent requests can carry large contexts; axum's 2 MB default is too small.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

pub fn public_router(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route(
            WireFormat::OpenAiChatCompletions.default_path(),
            post(handle_openai),
        )
        .route(
            WireFormat::AnthropicMessages.default_path(),
            post(handle_anthropic),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .with_state(state)
}

/// Resolves on SIGTERM (what `podman stop`/`coder proxy down` send) or SIGINT, so
/// `axum::serve` can shut down immediately instead of podman falling back to SIGKILL
/// after its default grace period. `proxy-serve` only ever actually runs inside the
/// Linux proxy container (even on a Windows or macOS host, since podman runs a Linux
/// VM), but the whole crate still has to compile for every host target the CLI
/// supports, so the SIGTERM branch is cfg(unix)-only.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("installing SIGTERM handler");
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// # Errors
///
/// Returns an error if either listener can't be bound.
pub async fn serve(
    state: ProxyState,
    admin_addr: SocketAddr,
    public_addr: SocketAddr,
) -> anyhow::Result<()> {
    let state = Arc::new(state);
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
    let public_listener = tokio::net::TcpListener::bind(public_addr).await?;

    let admin_app = admin_router(state.clone());
    let public_app = public_router(state);

    let admin_fut = async {
        axum::serve(admin_listener, admin_app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    };
    let public_fut = async {
        axum::serve(public_listener, public_app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    };

    tokio::try_join!(admin_fut, public_fut)?;
    Ok(())
}
