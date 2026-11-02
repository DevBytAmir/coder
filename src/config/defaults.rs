use std::path::Path;

use anyhow::{Context, Result};

const DEFAULT_PROVIDERS: &str = r#"[providers.anthropic]
upstream = "https://api.anthropic.com"
wire_format = "anthropic_messages"
auth_header_name = "x-api-key"      # default "Authorization" if omitted
auth_header_prefix = ""              # default "Bearer"; Anthropic uses the raw key, no prefix

[providers.nim]
upstream = "https://integrate.api.nvidia.com/v1"
wire_format = "openai_chat_completions"
default_model = "meta/muse-glimmer-30b"  # NVIDIA's catalog changes; run `coder models refresh nim`
                                          # and edit this if it ever 404s/410s

[providers.ollama]
upstream = "http://host.containers.internal:11434/v1"
wire_format = "openai_chat_completions"
default_model = "qwen3.5:4b"         # the model name as the upstream API expects it

[providers.openrouter]
upstream = "https://openrouter.ai/api/v1"
wire_format = "openai_chat_completions"
# no default_model: OpenRouter's catalog is huge and shifts constantly, run
# `coder models refresh openrouter` and pick one, or pass --model explicitly
"#;

const DEFAULT_TOOLS: &str = r#"# wire_format: the API shape the tool's client speaks; must match the provider's wire_format.
# model_args: how to pass the session model to the tool ({model} is substituted; "" = don't).

[tools.aider]
image = "coder-sandbox"
binary = "aider"
project_root = "git-root-of-arg"
wire_format = "openai_chat_completions"
model_args = "--model openai/{model}"   # litellm needs the openai/ prefix for OPENAI_API_BASE

[tools.opencode]
image = "coder-sandbox"
binary = "opencode"
project_root = "cwd"
data_volume = "opencode-data:/home/coder/.local/share/opencode"
wire_format = "openai_chat_completions"
model_args = ""                         # opencode.jsonc pins coder/default; the proxy enforces the real model

[tools.claude]
image = "coder-sandbox"
binary = "claude"
project_root = "cwd"
data_volume = "claude-data:/home/coder/.claude"
wire_format = "anthropic_messages"
model_args = "--model {model}"
"#;

/// # Errors
///
/// Returns an error if `path` can't be written to.
pub fn write_default_providers_if_missing(path: &Path) -> Result<()> {
    write_if_missing(path, DEFAULT_PROVIDERS)
}

/// # Errors
///
/// Returns an error if `path` can't be written to.
pub fn write_default_tools_if_missing(path: &Path) -> Result<()> {
    write_if_missing(path, DEFAULT_TOOLS)
}

fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::load_providers;
    use crate::config::tools::load_tools;

    #[test]
    fn creates_providers_file_with_parseable_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        write_default_providers_if_missing(&path).unwrap();
        let providers = load_providers(&path).unwrap();
        assert!(providers.contains_key("anthropic"));
        assert!(providers.contains_key("nim"));
        assert!(providers.contains_key("ollama"));
    }

    #[test]
    fn does_not_overwrite_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        std::fs::write(&path, "# custom content\n").unwrap();
        write_default_providers_if_missing(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# custom content\n"
        );
    }

    #[test]
    fn does_not_overwrite_invalid_existing_tools_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.toml");
        std::fs::write(&path, "[[[ not toml").unwrap();
        write_default_tools_if_missing(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[ not toml");
    }

    #[test]
    fn creates_tools_file_with_parseable_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.toml");
        write_default_tools_if_missing(&path).unwrap();
        let tools = load_tools(&path).unwrap();
        for name in ["aider", "opencode", "claude"] {
            assert!(tools.contains_key(name), "missing tool: {name}");
        }
    }
}
