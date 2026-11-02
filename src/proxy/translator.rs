use http::header::{HeaderName, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use serde_json::Value;

use crate::config::providers::{ProviderConfig, WireFormat};

pub struct UpstreamRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

pub struct AgentRequest {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

pub trait Translator: Send + Sync {
    /// # Errors
    ///
    /// Returns an error if the request can't be translated to the upstream's wire format.
    fn to_upstream(
        &self,
        req: AgentRequest,
        provider: &ProviderConfig,
    ) -> anyhow::Result<UpstreamRequest>;
    // name mirrors the spec's Translator trait (§3.7); it transforms a response, not a constructor
    #[allow(clippy::wrong_self_convention)]
    /// # Errors
    ///
    /// Returns an error if the upstream response can't be translated back.
    fn from_upstream(&self, status: StatusCode, body: Value)
        -> anyhow::Result<(StatusCode, Value)>;
}

/// Inbound headers forwarded upstream. Everything else is dropped: `host`, `content-length`
/// (recomputed by reqwest), `accept-encoding` (reqwest has no decompression enabled),
/// hop-by-hop headers, and both token-carrying headers.
const FORWARDED_HEADERS: &[&str] = &["accept", "anthropic-version", "anthropic-beta"];

/// Headers a tool may carry its session token in; never forwarded upstream.
const TOKEN_HEADERS: &[&str] = &["authorization", "x-api-key"];

// builds the upstream path/headers/body shared by both translators; identity passthrough on body
fn build_upstream(req: AgentRequest, provider: &ProviderConfig) -> UpstreamRequest {
    let path = provider
        .path_override
        .clone()
        .unwrap_or_else(|| provider.wire_format.default_path().to_string());
    let mut headers = HeaderMap::new();
    for name in FORWARDED_HEADERS {
        for value in req.headers.get_all(*name) {
            headers.append(HeaderName::from_static(name), value.clone());
        }
    }
    // the allowlist already excludes these; removed explicitly so a future allowlist edit
    // can never leak a session token upstream
    for name in TOKEN_HEADERS {
        headers.remove(*name);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    UpstreamRequest {
        method: Method::POST,
        path,
        headers,
        body: req.body,
    }
}

// builds the real auth header value: "{prefix} {key}" trimmed, so an empty prefix yields a bare key
fn auth_header_value(provider: &ProviderConfig, real_key: &str) -> anyhow::Result<HeaderValue> {
    let value = format!("{} {}", provider.auth_header_prefix, real_key)
        .trim()
        .to_string();
    Ok(HeaderValue::from_str(&value)?)
}

/// Inject the provider's real credential, if one is loaded. Providers with no key (e.g. a
/// local Ollama, which ignores auth) are forwarded with no auth header at all.
///
/// # Errors
///
/// Returns an error if the provider's auth header name or value isn't a valid HTTP header.
pub fn apply_real_key(
    req: &mut UpstreamRequest,
    provider: &ProviderConfig,
    real_key: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(key) = real_key {
        req.headers.insert(
            provider.auth_header_name.parse::<HeaderName>()?,
            auth_header_value(provider, key)?,
        );
    }
    Ok(())
}

/// Join the provider's upstream base URL and the request path, collapsing a duplicated
/// `/v1` (upstreams are commonly configured as `https://host/v1`, paths start with `/v1/`).
#[must_use]
pub fn upstream_url(upstream: &str, path: &str) -> String {
    let base = upstream.trim_end_matches('/');
    match (base.strip_suffix("/v1"), path.strip_prefix("/v1/")) {
        (Some(_), Some(rest)) => format!("{base}/{rest}"),
        _ => format!("{base}{path}"),
    }
}

pub struct OpenAiTranslator;
pub struct AnthropicTranslator;

impl Translator for OpenAiTranslator {
    fn to_upstream(
        &self,
        req: AgentRequest,
        provider: &ProviderConfig,
    ) -> anyhow::Result<UpstreamRequest> {
        Ok(build_upstream(req, provider))
    }

    // no response transformation is required for this wire format in this design
    fn from_upstream(
        &self,
        status: StatusCode,
        body: Value,
    ) -> anyhow::Result<(StatusCode, Value)> {
        Ok((status, body))
    }
}

impl Translator for AnthropicTranslator {
    fn to_upstream(
        &self,
        req: AgentRequest,
        provider: &ProviderConfig,
    ) -> anyhow::Result<UpstreamRequest> {
        Ok(build_upstream(req, provider))
    }

    // no response transformation is required for this wire format in this design
    fn from_upstream(
        &self,
        status: StatusCode,
        body: Value,
    ) -> anyhow::Result<(StatusCode, Value)> {
        Ok((status, body))
    }
}

#[must_use]
pub fn translator_for(format: WireFormat) -> Box<dyn Translator> {
    match format {
        WireFormat::OpenAiChatCompletions => Box::new(OpenAiTranslator),
        WireFormat::AnthropicMessages => Box::new(AnthropicTranslator),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::AUTHORIZATION;
    use serde_json::json;

    fn test_provider(upstream: &str, wire_format: WireFormat) -> ProviderConfig {
        ProviderConfig {
            upstream: upstream.to_string(),
            wire_format,
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer".to_string(),
            path_override: None,
            default_model: None,
        }
    }

    #[test]
    fn openai_translator_passes_body_through_unchanged() {
        let provider = test_provider("https://up.example.com", WireFormat::OpenAiChatCompletions);
        let req = AgentRequest {
            path: "/v1/chat/completions".into(),
            headers: HeaderMap::new(),
            body: json!({"model": "x"}),
        };
        let out = OpenAiTranslator.to_upstream(req, &provider).unwrap();
        assert_eq!(out.body, json!({"model": "x"}));
        assert_eq!(out.path, "/v1/chat/completions");
    }

    #[test]
    fn translator_injects_real_auth_header() {
        let mut provider =
            test_provider("https://up.example.com", WireFormat::OpenAiChatCompletions);
        provider.auth_header_name = "Authorization".into();
        provider.auth_header_prefix = "Bearer".into();
        let req = AgentRequest {
            path: "/v1/chat/completions".into(),
            headers: HeaderMap::new(),
            body: json!({}),
        };
        let mut out = OpenAiTranslator.to_upstream(req, &provider).unwrap();
        apply_real_key(&mut out, &provider, Some("real-key-123")).unwrap();
        assert_eq!(
            out.headers.get("Authorization").unwrap(),
            "Bearer real-key-123"
        );
    }

    #[test]
    fn anthropic_translator_uses_x_api_key_with_no_prefix() {
        let mut provider =
            test_provider("https://api.anthropic.com", WireFormat::AnthropicMessages);
        provider.auth_header_name = "x-api-key".into();
        provider.auth_header_prefix = String::new();
        let req = AgentRequest {
            path: "/v1/messages".into(),
            headers: HeaderMap::new(),
            body: json!({}),
        };
        let mut out = AnthropicTranslator.to_upstream(req, &provider).unwrap();
        apply_real_key(&mut out, &provider, Some("real-key-456")).unwrap();
        assert_eq!(out.headers.get("x-api-key").unwrap(), "real-key-456");
    }

    #[test]
    fn translator_for_selects_openai_by_format() {
        let provider = test_provider("https://up.example.com", WireFormat::OpenAiChatCompletions);
        let req = AgentRequest {
            path: "/ignored".into(),
            headers: HeaderMap::new(),
            body: json!({}),
        };
        let out = translator_for(WireFormat::OpenAiChatCompletions)
            .to_upstream(req, &provider)
            .unwrap();
        assert_eq!(out.path, WireFormat::OpenAiChatCompletions.default_path());
    }

    #[test]
    fn translator_for_selects_anthropic_by_format() {
        let provider = test_provider("https://api.anthropic.com", WireFormat::AnthropicMessages);
        let req = AgentRequest {
            path: "/ignored".into(),
            headers: HeaderMap::new(),
            body: json!({}),
        };
        let out = translator_for(WireFormat::AnthropicMessages)
            .to_upstream(req, &provider)
            .unwrap();
        assert_eq!(out.path, WireFormat::AnthropicMessages.default_path());
    }

    #[test]
    fn both_token_headers_and_unsafe_headers_are_dropped() {
        let provider = test_provider("https://up.example.com", WireFormat::OpenAiChatCompletions);
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer session-tok"),
            ("x-api-key", "session-tok"),
            ("host", "tool-container"),
            ("content-length", "999"),
            ("accept-encoding", "gzip"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("accept", "application/json"),
            ("anthropic-version", "2023-06-01"),
        ] {
            headers.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let req = AgentRequest {
            path: "/v1/chat/completions".into(),
            headers,
            body: json!({}),
        };
        let out = OpenAiTranslator.to_upstream(req, &provider).unwrap();
        for dropped in [
            "authorization",
            "x-api-key",
            "host",
            "content-length",
            "accept-encoding",
            "connection",
            "transfer-encoding",
        ] {
            assert!(
                !out.headers.contains_key(dropped),
                "{dropped} was forwarded"
            );
        }
        assert_eq!(out.headers.get("accept").unwrap(), "application/json");
        assert_eq!(out.headers.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn no_real_key_means_no_auth_header() {
        let provider = test_provider("http://ollama:11434", WireFormat::OpenAiChatCompletions);
        let req = AgentRequest {
            path: "/v1/chat/completions".into(),
            headers: HeaderMap::new(),
            body: json!({}),
        };
        let mut out = OpenAiTranslator.to_upstream(req, &provider).unwrap();
        apply_real_key(&mut out, &provider, None).unwrap();
        assert!(!out.headers.contains_key(AUTHORIZATION));
    }

    #[test]
    fn upstream_url_collapses_duplicate_v1() {
        assert_eq!(
            upstream_url("http://h:11434/v1", "/v1/chat/completions"),
            "http://h:11434/v1/chat/completions"
        );
        assert_eq!(
            upstream_url("https://api.anthropic.com/", "/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            upstream_url("https://x.example/api", "/v1/chat/completions"),
            "https://x.example/api/v1/chat/completions"
        );
    }
}
