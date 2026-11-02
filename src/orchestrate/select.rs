use std::collections::HashMap;

use crate::config::providers::ProviderConfig;
use crate::config::tools::ToolConfig;
use crate::orchestrate::sorted_joined;

/// Pick which provider `coder run` should use (the explicitly requested one if given and
/// known, the sole configured provider if there's only one, otherwise an error listing the
/// configured provider names so the user can pass `--provider`), and check its wire format
/// matches `tool`'s. Deliberately one function, not two: a caller resolving a provider
/// without also checking wire-format compatibility would reintroduce the exact
/// mismatched-tool/provider bug class this exists to prevent.
///
/// # Errors
///
/// Returns an error if `requested` names an unknown provider, no provider is configured
/// and none was requested, or the resolved provider's wire format doesn't match `tool`'s.
///
/// # Panics
///
/// Never panics in practice: the only `.unwrap()` is guarded by `providers.len() == 1`.
pub fn resolve_provider_for_run(
    providers: &HashMap<String, ProviderConfig>,
    tool_name: &str,
    tool: &ToolConfig,
    requested: Option<&str>,
) -> anyhow::Result<String> {
    let name = match requested {
        Some(name) => {
            if providers.contains_key(name) {
                name.to_string()
            } else {
                let list = sorted_joined(providers.keys());
                anyhow::bail!("unknown provider `{name}`; configured providers: {list}")
            }
        }
        None => {
            if providers.len() == 1 {
                providers.keys().next().unwrap().clone()
            } else {
                let list = sorted_joined(providers.keys());
                anyhow::bail!("multiple providers configured ({list}): pass --provider")
            }
        }
    };

    let provider = &providers[&name];
    let tool_format = tool.native_wire_format()?;
    if tool_format != provider.wire_format {
        anyhow::bail!(
            "tool `{tool_name}` speaks {tool_format:?} but provider `{name}` speaks {:?}; \
             pick a provider with a matching wire_format",
            provider.wire_format
        );
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::WireFormat;
    use crate::config::tools::ProjectRoot;

    fn two_provider_fixture() -> HashMap<String, ProviderConfig> {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                upstream: "https://api.anthropic.com".to_string(),
                wire_format: WireFormat::AnthropicMessages,
                auth_header_name: "x-api-key".to_string(),
                auth_header_prefix: String::new(),
                path_override: None,
                default_model: None,
            },
        );
        providers.insert(
            "nim".to_string(),
            ProviderConfig {
                upstream: "https://integrate.api.nvidia.com/v1".to_string(),
                wire_format: WireFormat::OpenAiChatCompletions,
                auth_header_name: "Authorization".to_string(),
                auth_header_prefix: "Bearer".to_string(),
                path_override: None,
                default_model: None,
            },
        );
        providers
    }

    fn tool_with_no_default() -> ToolConfig {
        ToolConfig {
            image: "coder-sandbox".to_string(),
            binary: "aider".to_string(),
            project_root: ProjectRoot::Cwd,
            data_volume: None,
            extra_env: HashMap::new(),
            wire_format: None,
            model_args: None,
        }
    }

    #[test]
    fn mismatched_wire_formats_are_refused() {
        let providers = two_provider_fixture();
        let mut claude = tool_with_no_default();
        claude.binary = "claude".into();
        let err = resolve_provider_for_run(&providers, "claude", &claude, Some("nim"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("claude") && err.contains("nim"), "{err}");
        assert!(resolve_provider_for_run(&providers, "claude", &claude, Some("anthropic")).is_ok());
        let aider = tool_with_no_default();
        assert!(resolve_provider_for_run(&providers, "aider", &aider, Some("anthropic")).is_err());
        assert!(resolve_provider_for_run(&providers, "aider", &aider, Some("nim")).is_ok());
    }

    #[test]
    fn run_without_provider_and_multiple_providers_errors_clearly() {
        let err = resolve_provider_for_run(
            &two_provider_fixture(),
            "aider",
            &tool_with_no_default(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--provider"));
    }

    #[test]
    fn run_with_unknown_provider_name_lists_configured_providers() {
        let err = resolve_provider_for_run(
            &two_provider_fixture(),
            "aider",
            &tool_with_no_default(),
            Some("nope"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("anthropic") && msg.contains("nim"));
    }

    #[test]
    fn run_with_sole_provider_and_no_request_resolves_to_it() {
        // "nim" (not "anthropic"): the sole-provider fixture must still be wire-format
        // compatible with the default aider tool, since resolution and the compatibility
        // check are the same function now.
        let mut providers = HashMap::new();
        providers.insert("nim".to_string(), two_provider_fixture()["nim"].clone());
        let resolved =
            resolve_provider_for_run(&providers, "aider", &tool_with_no_default(), None).unwrap();
        assert_eq!(resolved, "nim");
    }

    #[test]
    fn run_with_known_requested_provider_resolves_to_it() {
        let resolved = resolve_provider_for_run(
            &two_provider_fixture(),
            "aider",
            &tool_with_no_default(),
            Some("nim"),
        )
        .unwrap();
        assert_eq!(resolved, "nim");
    }
}
