use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::providers::WireFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ProjectRoot {
    #[serde(rename = "cwd")]
    Cwd,
    #[serde(rename = "git-root-of-arg")]
    GitRootOfArg,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolConfig {
    pub image: String,
    pub binary: String,
    pub project_root: ProjectRoot,
    #[serde(default)]
    pub data_volume: Option<String>,
    #[serde(default)]
    pub extra_env: HashMap<String, String>,
    /// Wire format the tool's own LLM client speaks; inferred for known binaries if omitted.
    #[serde(default)]
    pub wire_format: Option<WireFormat>,
    /// Template for the args that pass the model to the tool (`{model}` is substituted,
    /// split on whitespace, empty = pass nothing); inferred for known binaries if omitted.
    #[serde(default)]
    pub model_args: Option<String>,
    /// Memory limit for the tool container, passed to podman as `--memory` (e.g.
    /// `"4g"`); unlimited if omitted.
    #[serde(default)]
    pub memory: Option<String>,
    /// CPU limit for the tool container, passed to podman as `--cpus` (e.g. `2` or
    /// `1.5`); unlimited if omitted.
    #[serde(default)]
    pub cpus: Option<f64>,
}

impl ToolConfig {
    /// The wire format this tool's client natively sends.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool binary has no known wire format and
    /// `wire_format` wasn't set explicitly in `tools.toml`.
    pub fn native_wire_format(&self) -> Result<WireFormat> {
        if let Some(format) = self.wire_format {
            return Ok(format);
        }
        match self.binary.as_str() {
            "claude" => Ok(WireFormat::AnthropicMessages),
            "aider" | "opencode" => Ok(WireFormat::OpenAiChatCompletions),
            other => anyhow::bail!(
                "tool binary `{other}` has no known wire format; set `wire_format` for it in tools.toml"
            ),
        }
    }

    /// Reject resource limits podman would refuse, so the error names the tool and field
    /// instead of surfacing as a podman failure at launch.
    fn validate_limits(&self) -> Result<()> {
        if let Some(memory) = &self.memory {
            if !is_podman_size(memory) {
                anyhow::bail!(
                    "invalid memory `{memory}`: expected a positive size like \"512m\" or \"4g\""
                );
            }
        }
        if let Some(cpus) = self.cpus {
            if !cpus.is_finite() || cpus <= 0.0 {
                anyhow::bail!("invalid cpus `{cpus}`: expected a positive number like 2 or 1.5");
            }
        }
        Ok(())
    }

    /// The CLI args that select `model` in this tool.
    #[must_use]
    pub fn model_cli_args(&self, model: &str) -> Vec<String> {
        let template = match &self.model_args {
            Some(t) => t.as_str(),
            None => match self.binary.as_str() {
                // litellm needs the openai/ prefix to route to OPENAI_API_BASE
                "aider" => "--model openai/{model}",
                // opencode's config pins the fixed `coder/default` model; the proxy enforces the real one
                "opencode" => "",
                _ => "--model {model}",
            },
        };
        template
            .split_whitespace()
            .map(|part| part.replace("{model}", model))
            .collect()
    }
}

/// Whether podman accepts `text` as a memory size and it is non-zero. Mirrors the
/// go-units `RAMInBytes` grammar podman uses: `<number>[ ][kmgtp][i][b]`, any case.
fn is_podman_size(text: &str) -> bool {
    let number_len = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(number_len);
    let valid_number = number
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    let suffix = suffix
        .strip_prefix(' ')
        .unwrap_or(suffix)
        .to_ascii_lowercase();
    let suffix = suffix
        .strip_prefix(['k', 'm', 'g', 't', 'p'])
        .unwrap_or(&suffix);
    let suffix = suffix.strip_prefix('i').unwrap_or(suffix);
    let suffix = suffix.strip_prefix('b').unwrap_or(suffix);
    valid_number && suffix.is_empty() && number.bytes().any(|b| (b'1'..=b'9').contains(&b))
}

#[derive(Deserialize)]
struct ToolsFile {
    #[serde(default)]
    tools: HashMap<String, toml::Value>,
}

/// # Errors
///
/// Returns an error if the file can't be read or doesn't parse as valid
/// `tools.toml`.
pub fn load_tools(path: &Path) -> Result<HashMap<String, ToolConfig>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_tools_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn parse_tools_str(text: &str) -> Result<HashMap<String, ToolConfig>> {
    let file: ToolsFile = toml::from_str(text).context("invalid tools.toml")?;
    file.tools
        .into_iter()
        .map(|(name, value)| {
            let cfg = value
                .try_into::<ToolConfig>()
                .with_context(|| format!("invalid tool `{name}`"))?;
            cfg.validate_limits()
                .with_context(|| format!("invalid tool `{name}`"))?;
            Ok((name, cfg))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_with_data_volume() {
        let toml = r#"
            [tools.claude]
            image = "coder-sandbox"
            binary = "claude"
            project_root = "cwd"
            data_volume = "claude-data:/home/coder/.claude"
        "#;
        let tools = parse_tools_str(toml).unwrap();
        assert_eq!(
            tools["claude"].data_volume.as_deref(),
            Some("claude-data:/home/coder/.claude")
        );
        assert_eq!(tools["claude"].project_root, ProjectRoot::Cwd);
    }

    #[test]
    fn parses_tool_without_data_volume() {
        let toml = r#"
            [tools.aider]
            image = "coder-sandbox"
            binary = "aider"
            project_root = "git-root-of-arg"
        "#;
        let tools = parse_tools_str(toml).unwrap();
        assert_eq!(tools["aider"].data_volume, None);
        assert_eq!(tools["aider"].project_root, ProjectRoot::GitRootOfArg);
    }

    fn tool(binary: &str) -> ToolConfig {
        ToolConfig {
            image: "coder-sandbox".to_string(),
            binary: binary.to_string(),
            project_root: ProjectRoot::Cwd,
            data_volume: None,
            extra_env: HashMap::new(),
            wire_format: None,
            model_args: None,
            memory: None,
            cpus: None,
        }
    }

    #[test]
    fn native_wire_format_is_inferred_for_known_binaries() {
        assert_eq!(
            tool("claude").native_wire_format().unwrap(),
            WireFormat::AnthropicMessages
        );
        assert_eq!(
            tool("aider").native_wire_format().unwrap(),
            WireFormat::OpenAiChatCompletions
        );
        assert!(tool("mystery").native_wire_format().is_err());
    }

    #[test]
    fn explicit_wire_format_wins_over_inference() {
        let mut t = tool("mystery");
        t.wire_format = Some(WireFormat::AnthropicMessages);
        assert_eq!(
            t.native_wire_format().unwrap(),
            WireFormat::AnthropicMessages
        );
    }

    #[test]
    fn model_cli_args_follow_template_or_binary_default() {
        assert_eq!(
            tool("aider").model_cli_args("m"),
            vec!["--model", "openai/m"]
        );
        assert_eq!(tool("opencode").model_cli_args("m"), Vec::<String>::new());
        assert_eq!(tool("claude").model_cli_args("m"), vec!["--model", "m"]);
        let mut t = tool("claude");
        t.model_args = Some("-m {model}".into());
        assert_eq!(t.model_cli_args("x"), vec!["-m", "x"]);
    }

    #[test]
    fn parses_wire_format_and_model_args_fields() {
        let toml = r#"
            [tools.custom]
            image = "img"
            binary = "custom"
            project_root = "cwd"
            wire_format = "anthropic_messages"
            model_args = ""
        "#;
        let tools = parse_tools_str(toml).unwrap();
        assert_eq!(
            tools["custom"].wire_format,
            Some(WireFormat::AnthropicMessages)
        );
        assert_eq!(tools["custom"].model_cli_args("x"), Vec::<String>::new());
    }

    #[test]
    fn missing_binary_errors_with_tool_name() {
        let toml = r#"
            [tools.broken]
            image = "coder-sandbox"
            project_root = "cwd"
        "#;
        let err = parse_tools_str(toml).unwrap_err().to_string();
        assert!(err.contains("broken"), "error should name the tool: {err}");
    }

    #[test]
    fn parses_resource_limits_with_integer_or_float_cpus() {
        let toml = r#"
            [tools.a]
            image = "img"
            binary = "aider"
            project_root = "cwd"
            memory = "4g"
            cpus = 2

            [tools.b]
            image = "img"
            binary = "aider"
            project_root = "cwd"
            cpus = 1.5
        "#;
        let tools = parse_tools_str(toml).unwrap();
        assert_eq!(tools["a"].memory.as_deref(), Some("4g"));
        assert_eq!(tools["a"].cpus, Some(2.0));
        assert_eq!(tools["b"].memory, None);
        assert_eq!(tools["b"].cpus, Some(1.5));
    }

    #[test]
    fn rejects_resource_limits_podman_would_refuse() {
        for (field, value) in [
            ("memory", r#""""#),
            ("memory", r#""0""#),
            ("memory", r#""0.0g""#),
            ("memory", r#""g""#),
            ("memory", r#""-1g""#),
            ("memory", r#""4gg""#),
            ("memory", r#""4x""#),
            ("memory", r#""1..5g""#),
            ("cpus", "0"),
            ("cpus", "-1"),
            ("cpus", "nan"),
            ("cpus", "inf"),
        ] {
            let toml = format!(
                "[tools.t]\nimage = \"i\"\nbinary = \"aider\"\nproject_root = \"cwd\"\n{field} = {value}\n"
            );
            let err = parse_tools_str(&toml).unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid tool `t`"),
                "{field} = {value}: {err:#}"
            );
        }
    }

    #[test]
    fn accepts_memory_with_or_without_a_unit() {
        for memory in [
            "536870912",
            "512m",
            "4g",
            "4G",
            "4gb",
            "4GiB",
            "1.5g",
            "2 g",
            "100b",
        ] {
            let toml = format!(
                "[tools.t]\nimage = \"i\"\nbinary = \"aider\"\nproject_root = \"cwd\"\nmemory = \"{memory}\"\n"
            );
            assert!(parse_tools_str(&toml).is_ok(), "should accept {memory}");
        }
    }
}
