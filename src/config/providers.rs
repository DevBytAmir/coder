use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl WireFormat {
    #[must_use]
    pub fn default_path(&self) -> &'static str {
        match self {
            WireFormat::OpenAiChatCompletions => "/v1/chat/completions",
            WireFormat::AnthropicMessages => "/v1/messages",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProviderConfig {
    pub upstream: String,
    pub wire_format: WireFormat,
    #[serde(default = "default_auth_header_name")]
    pub auth_header_name: String,
    #[serde(default = "default_auth_header_prefix")]
    pub auth_header_prefix: String,
    #[serde(default)]
    pub path_override: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
}

fn default_auth_header_name() -> String {
    "Authorization".to_string()
}

fn default_auth_header_prefix() -> String {
    "Bearer".to_string()
}

#[derive(Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    providers: HashMap<String, toml::Value>,
}

/// # Errors
///
/// Returns an error if the file can't be read or doesn't parse as valid
/// `providers.toml`.
pub fn load_providers(path: &Path) -> Result<HashMap<String, ProviderConfig>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_providers_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn parse_providers_str(text: &str) -> Result<HashMap<String, ProviderConfig>> {
    let file: ProvidersFile = toml::from_str(text).context("invalid providers.toml")?;
    file.providers
        .into_iter()
        .map(|(name, value)| {
            let cfg = value
                .try_into::<ProviderConfig>()
                .with_context(|| format!("invalid provider `{name}`"))?;
            Ok((name, cfg))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_provider() {
        let toml = r#"
            [providers.nim]
            upstream = "https://integrate.api.nvidia.com/v1"
            wire_format = "openai_chat_completions"
        "#;
        let providers = parse_providers_str(toml).unwrap();
        let nim = &providers["nim"];
        assert_eq!(nim.upstream, "https://integrate.api.nvidia.com/v1");
        assert_eq!(nim.auth_header_name, "Authorization");
        assert_eq!(nim.auth_header_prefix, "Bearer");
    }

    #[test]
    fn anthropic_overrides_defaults() {
        let toml = r#"
            [providers.anthropic]
            upstream = "https://api.anthropic.com"
            wire_format = "anthropic_messages"
            auth_header_name = "x-api-key"
            auth_header_prefix = ""
        "#;
        let providers = parse_providers_str(toml).unwrap();
        assert_eq!(providers["anthropic"].auth_header_name, "x-api-key");
        assert_eq!(providers["anthropic"].auth_header_prefix, "");
    }

    #[test]
    fn missing_upstream_errors_with_provider_name() {
        let toml = r#"
            [providers.broken]
            wire_format = "openai_chat_completions"
        "#;
        let err = parse_providers_str(toml).unwrap_err().to_string();
        assert!(
            err.contains("broken"),
            "error should name the provider: {err}"
        );
    }

    #[test]
    fn default_paths_match_spec() {
        assert_eq!(
            WireFormat::OpenAiChatCompletions.default_path(),
            "/v1/chat/completions"
        );
        assert_eq!(WireFormat::AnthropicMessages.default_path(), "/v1/messages");
    }

    #[test]
    fn unknown_wire_format_errors() {
        let toml = r#"
            [providers.broken]
            upstream = "https://example.com"
            wire_format = "something_else"
        "#;
        assert!(parse_providers_str(toml).is_err());
    }
}
