use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{bail, Context, Result};

use coder::config::providers::{load_providers, ProviderConfig};
use coder::orchestrate::podman::{key_env_var, REVOKE_TOKEN_ENV};
use coder::proxy::server::{serve, ProxyState};
use coder::proxy::tokens::TokenStore;

const PROVIDERS_PATH: &str = "/etc/coder/providers.toml";
// Loopback only: unreachable from coder-net. Rootless podman's `-p` forwards to the
// container's network IP (not its loopback), so the host reaches this via `podman exec`
// running `coder proxy-admin` inside the container instead.
const ADMIN_ADDR: &str = "127.0.0.1:8787";
const PUBLIC_ADDR: &str = "0.0.0.0:8080";
const ADMIN_TOKEN_VAR: &str = "CODER_ADMIN_TOKEN";

pub fn real_keys_from_env(providers: &HashMap<String, ProviderConfig>) -> HashMap<String, String> {
    providers
        .keys()
        .filter_map(|name| {
            std::env::var(key_env_var(name))
                .ok()
                .map(|value| (name.clone(), value))
        })
        .collect()
}

pub async fn run_proxy_serve() -> Result<()> {
    let providers = load_providers(Path::new(PROVIDERS_PATH))?;
    let real_keys = real_keys_from_env(&providers);
    let admin_token = std::env::var(ADMIN_TOKEN_VAR)
        .ok()
        .filter(|t| !t.is_empty())
        .with_context(|| format!("{ADMIN_TOKEN_VAR} must be set (coder proxy up sets it)"))?;

    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    let mut keyed: Vec<&String> = real_keys.keys().collect();
    keyed.sort();
    eprintln!("coder-proxy: providers configured: {names:?}; keys loaded: {keyed:?}");
    eprintln!("coder-proxy: public on {PUBLIC_ADDR}, admin on {ADMIN_ADDR}");

    let state = ProxyState {
        providers,
        real_keys,
        tokens: TokenStore::new(),
        admin_token: Some(admin_token),
        http_client: reqwest::Client::new(),
    };
    let admin_addr: SocketAddr = ADMIN_ADDR.parse()?;
    let public_addr: SocketAddr = PUBLIC_ADDR.parse()?;
    serve(state, admin_addr, public_addr).await
}

/// `coder proxy-admin`: runs inside the proxy container (via `podman exec`) and makes one
/// call to the loopback-only admin API, printing the response body to stdout.
pub fn run_proxy_admin(method: &str, path: &str, body: Option<&str>) -> Result<()> {
    let admin_token = std::env::var(ADMIN_TOKEN_VAR).context("CODER_ADMIN_TOKEN not set")?;
    let method: reqwest::Method = method.parse().context("invalid HTTP method")?;
    let client = reqwest::blocking::Client::new();
    let mut req = client
        .request(method, format!("http://{ADMIN_ADDR}{path}"))
        .bearer_auth(admin_token);
    if let Some(body) = body {
        req = req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    let resp = req.send().context("connecting to the proxy admin API")?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        bail!("admin API returned {status}: {text}");
    }
    print!("{text}");
    Ok(())
}

/// `coder proxy-admin-revoke-self`: revokes the session token named by `$CODER_REVOKE_TOKEN`.
/// A separate subcommand (rather than `proxy-admin DELETE /admin/tokens/<token>`) so the
/// token travels only via env, never as a literal argument in `podman exec`'s argv, which
/// `ps`/`/proc/<pid>/cmdline` would otherwise expose to any other local user.
pub fn run_proxy_admin_revoke_self() -> Result<()> {
    let admin_token = std::env::var(ADMIN_TOKEN_VAR).context("CODER_ADMIN_TOKEN not set")?;
    let revoke_token = std::env::var(REVOKE_TOKEN_ENV).context("CODER_REVOKE_TOKEN not set")?;
    let resp = reqwest::blocking::Client::new()
        .delete(format!("http://{ADMIN_ADDR}/admin/tokens/{revoke_token}"))
        .bearer_auth(admin_token)
        .send()
        .context("connecting to the proxy admin API")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("admin API returned {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use coder::config::providers::WireFormat;

    fn test_provider_config() -> ProviderConfig {
        ProviderConfig {
            upstream: "https://example.invalid/v1".to_string(),
            wire_format: WireFormat::AnthropicMessages,
            auth_header_name: "x-api-key".to_string(),
            auth_header_prefix: String::new(),
            path_override: None,
            default_model: None,
        }
    }

    #[test]
    fn listen_ports_match_network_isolation_design() {
        assert!(ADMIN_ADDR.ends_with(":8787"));
        assert!(PUBLIC_ADDR.ends_with(":8080"));
    }

    #[test]
    fn admin_listener_is_loopback_only() {
        let addr: SocketAddr = ADMIN_ADDR.parse().unwrap();
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn reads_provider_key_from_matching_env_var() {
        std::env::set_var("CODER_KEY_ANTHROPIC", "secret-value");
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), test_provider_config());
        let keys = real_keys_from_env(&providers);
        assert_eq!(keys.get("anthropic"), Some(&"secret-value".to_string()));
        std::env::remove_var("CODER_KEY_ANTHROPIC");
    }

    #[test]
    fn provider_with_no_env_var_is_absent_not_erroring() {
        let mut providers = HashMap::new();
        providers.insert("ollama".to_string(), test_provider_config());
        let keys = real_keys_from_env(&providers);
        assert!(!keys.contains_key("ollama"));
    }
}
