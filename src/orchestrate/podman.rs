use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};

use crate::config::providers::{ProviderConfig, WireFormat};
use crate::config::tools::ToolConfig;

/// Internal network shared by tool containers and the proxy: no egress.
pub const NETWORK: &str = "coder-net";
/// Normal bridge network only the proxy joins, for its upstream egress.
pub const EGRESS_NETWORK: &str = "coder-egress";
const PROXY_CONTAINER_NAME: &str = "coder-proxy";
const PROXY_URL_BASE: &str = "http://coder-proxy:8080";

/// Build args for building the sandbox tool image.
#[must_use]
pub fn build_sandbox_image_args() -> Vec<String> {
    vec![
        "build".to_string(),
        "-t".to_string(),
        "coder-sandbox".to_string(),
        "-f".to_string(),
        "containers/sandbox/Containerfile".to_string(),
        "containers/sandbox".to_string(),
    ]
}

/// Build args for building the proxy image.
#[must_use]
pub fn build_proxy_image_args() -> Vec<String> {
    vec![
        "build".to_string(),
        "-t".to_string(),
        "coder-proxy".to_string(),
        "-f".to_string(),
        "containers/proxy/Containerfile".to_string(),
        ".".to_string(),
    ]
}

/// Build args to create a podman network. `internal` networks have no route off the
/// network at all (no NAT egress, no external DNS).
#[must_use]
pub fn network_create_args(name: &str, internal: bool) -> Vec<String> {
    let mut args = vec!["network".to_string(), "create".to_string()];
    if internal {
        args.push("--internal".to_string());
    }
    args.push(name.to_string());
    args
}

/// Build args to check whether a network exists (exit status 0 if so).
#[must_use]
pub fn network_exists_args(name: &str) -> Vec<String> {
    vec![
        "network".to_string(),
        "exists".to_string(),
        name.to_string(),
    ]
}

/// Build args that print `true`/`false` for whether a network is `--internal`.
#[must_use]
pub fn network_is_internal_args(name: &str) -> Vec<String> {
    vec![
        "network".to_string(),
        "inspect".to_string(),
        name.to_string(),
        "--format".to_string(),
        "{{.Internal}}".to_string(),
    ]
}

/// Build args to remove a network.
#[must_use]
pub fn network_rm_args(name: &str) -> Vec<String> {
    vec!["network".to_string(), "rm".to_string(), name.to_string()]
}

/// Refuse host paths podman's `-v src:dst[:opts]` syntax can't represent as a bind mount.
/// # Errors
///
/// Returns an error if `path` isn't absolute or contains a `:` outside a Windows
/// drive letter.
pub fn validate_bind_source(path: &Path) -> anyhow::Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path {} is not valid UTF-8", path.display()))?;
    if !path.is_absolute() {
        anyhow::bail!("bind-mount source {text} must be an absolute path");
    }
    // ':' would otherwise collide with podman's `source:dest` bind-mount separator, but a
    // Windows drive letter ("C:\...") is exactly one colon at index 1; allow only that.
    let has_drive_letter = text.len() > 1 && text.as_bytes()[1] == b':';
    let rest = if has_drive_letter { &text[2..] } else { text };
    if rest.contains(':') {
        anyhow::bail!("paths containing ':' are not supported for bind mounts: {text}");
    }
    Ok(())
}

/// The env var a provider's real key is passed to the proxy container in.
#[must_use]
pub fn key_env_var(provider: &str) -> String {
    format!("CODER_KEY_{}", provider.to_uppercase())
}

/// Env var holding the proxy's admin-API shared secret.
pub const ADMIN_TOKEN_ENV: &str = "CODER_ADMIN_TOKEN";

/// Build args to start the proxy container. Real keys and the admin token are passed by
/// name only (`-e NAME`, which podman fills from its own environment); the values travel in
/// the env returned by [`proxy_up_env`], never in argv, so they don't show up in `ps`.
///
/// The proxy joins `coder-net` (internal; where tool containers reach it) and
/// `coder-egress` (a normal bridge; its only route to upstream providers). No port is
/// published: the admin API listens on the container's loopback and is reached via
/// `podman exec` (see [`proxy_admin_args`]).
#[must_use]
pub fn proxy_up_args(
    real_keys: &HashMap<String, String>,
    providers_file: &Path,
    secrets_file: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        PROXY_CONTAINER_NAME.to_string(),
        "--network".to_string(),
        NETWORK.to_string(),
        "--network".to_string(),
        EGRESS_NETWORK.to_string(),
        "-v".to_string(),
        format!("{}:/etc/coder/providers.toml:ro", providers_file.display()),
    ];

    if let Some(path) = secrets_file {
        args.push("-v".to_string());
        args.push(format!("{}:/run/secrets/coder-secrets", path.display()));
    }

    args.push("-e".to_string());
    args.push(ADMIN_TOKEN_ENV.to_string());
    let mut names: Vec<&String> = real_keys.keys().collect();
    names.sort();
    for name in names {
        args.push("-e".to_string());
        args.push(key_env_var(name));
    }

    args.push("coder-proxy".to_string());
    args
}

/// The env to set on the `podman run` process from [`proxy_up_args`].
#[must_use]
pub fn proxy_up_env(
    real_keys: &HashMap<String, String>,
    admin_token: &str,
) -> Vec<(String, String)> {
    let mut env = vec![(ADMIN_TOKEN_ENV.to_string(), admin_token.to_string())];
    let mut keys: Vec<_> = real_keys.iter().collect();
    keys.sort();
    for (name, value) in keys {
        env.push((key_env_var(name), value.clone()));
    }
    env
}

/// Build args to make one admin-API call by running `coder proxy-admin` inside the proxy
/// container, which reaches the loopback-only admin listener and prints the response.
#[must_use]
pub fn proxy_admin_args(method: &str, path: &str, body: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        PROXY_CONTAINER_NAME.to_string(),
        "coder".to_string(),
        "proxy-admin".to_string(),
        method.to_string(),
        path.to_string(),
    ];
    if let Some(body) = body {
        args.push("--body".to_string());
        args.push(body.to_string());
    }
    args
}

/// Env var the session token to revoke travels in. Never appears in argv, see
/// [`proxy_admin_revoke_args`].
pub const REVOKE_TOKEN_ENV: &str = "CODER_REVOKE_TOKEN";

/// Build args to revoke a session token without the token ever appearing in `podman exec`'s
/// argv (visible via `ps`/`/proc/<pid>/cmdline`): it's passed by name only (`-e NAME`), with
/// the value set via [`exec_podman`]'s `env` parameter instead.
#[must_use]
pub fn proxy_admin_revoke_args() -> Vec<String> {
    vec![
        "exec".to_string(),
        "-e".to_string(),
        REVOKE_TOKEN_ENV.to_string(),
        PROXY_CONTAINER_NAME.to_string(),
        "coder".to_string(),
        "proxy-admin-revoke-self".to_string(),
    ]
}

/// Build args to stop the proxy container.
#[must_use]
pub fn proxy_down_args() -> Vec<String> {
    vec!["stop".to_string(), PROXY_CONTAINER_NAME.to_string()]
}

/// Build args to check whether the proxy container is running.
#[must_use]
pub fn proxy_status_args() -> Vec<String> {
    vec![
        "ps".to_string(),
        "--filter".to_string(),
        format!("name={}", PROXY_CONTAINER_NAME),
    ]
}

/// Env var names a tool reads its base URL and API key from, per wire format.
fn tool_env_names(format: WireFormat) -> (&'static [&'static str], &'static str) {
    match format {
        // litellm (aider) reads OPENAI_API_BASE; newer OpenAI SDKs read OPENAI_BASE_URL
        WireFormat::OpenAiChatCompletions => {
            (&["OPENAI_API_BASE", "OPENAI_BASE_URL"], "OPENAI_API_KEY")
        }
        // Claude Code sends ANTHROPIC_API_KEY as `x-api-key`
        WireFormat::AnthropicMessages => (&["ANTHROPIC_BASE_URL"], "ANTHROPIC_API_KEY"),
    }
}

/// Build args to run a tool container against the shared network and proxy.
/// Returns `(argv, env)`: the session token is passed by name only in argv (`-e NAME`) and
/// its value travels in `env`, which the caller sets on the podman process, keeping it out
/// of the host's visible command line.
#[must_use]
pub fn run_tool_args(
    tool: &ToolConfig,
    project_dir: &Path,
    session_token: &str,
    provider: &ProviderConfig,
    model: Option<&str>,
    tty: bool,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        // -t only when attached to a real terminal; podman errors on -t without one
        if tty { "-it" } else { "-i" }.to_string(),
        "--userns=keep-id:uid=1001,gid=1001".to_string(),
        // The tool is an autonomous agent: drop every Linux capability it doesn't need
        // (it only reads/writes files and makes outbound HTTP calls), forbid it from
        // gaining new privileges via a setuid/setgid binary, and cap how many processes
        // it can fork so a runaway/malicious agent can't fork-bomb the host.
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--pids-limit=256".to_string(),
        "--network".to_string(),
        NETWORK.to_string(),
        "-v".to_string(),
        format!("{}:/workspace", project_dir.display()),
    ];

    if let Some(data_volume) = &tool.data_volume {
        args.push("-v".to_string());
        args.push(data_volume.clone());
    }

    let (base_url_vars, key_var) = tool_env_names(provider.wire_format);
    let base_url = match provider.wire_format {
        WireFormat::OpenAiChatCompletions => format!("{PROXY_URL_BASE}/v1"),
        WireFormat::AnthropicMessages => PROXY_URL_BASE.to_string(),
    };
    for var in base_url_vars {
        args.push("-e".to_string());
        args.push(format!("{var}={base_url}"));
    }
    args.push("-e".to_string());
    args.push(key_var.to_string());
    let env = vec![(key_var.to_string(), session_token.to_string())];

    let mut extra_env: Vec<_> = tool.extra_env.iter().collect();
    extra_env.sort_by_key(|(k, _): &(&String, &String)| (*k).clone());
    for (key, value) in extra_env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }

    args.push(tool.image.clone());
    args.push(tool.binary.clone());

    if let Some(model) = model {
        args.extend(tool.model_cli_args(model));
    }

    (args, env)
}

/// Run podman non-interactively and capture its output (never through a shell).
/// `env` is set on the podman child process only, so values never appear in argv.
///
/// # Errors
///
/// Returns an error if `podman` can't be spawned.
pub fn exec_podman(args: &[String], env: &[(String, String)]) -> anyhow::Result<Output> {
    let output = Command::new("podman")
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()?;
    Ok(output)
}

/// Run podman attached to this terminal (inherited stdin/stdout/stderr), for interactive
/// tool sessions and long-running builds whose progress should stream live.
///
/// # Errors
///
/// Returns an error if `podman` can't be spawned.
pub fn run_podman_attached(
    args: &[String],
    env: &[(String, String)],
) -> anyhow::Result<ExitStatus> {
    let status = Command::new("podman")
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .status()?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tools::ProjectRoot;
    use std::collections::HashMap;

    fn test_tool_config() -> ToolConfig {
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

    fn test_provider_config() -> ProviderConfig {
        ProviderConfig {
            upstream: "https://api.openai.com/v1".to_string(),
            wire_format: WireFormat::OpenAiChatCompletions,
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer".to_string(),
            path_override: None,
            default_model: None,
        }
    }

    #[test]
    fn run_tool_args_uses_coder_net_not_host() {
        let tool = test_tool_config();
        let provider = test_provider_config();
        let (args, _) = run_tool_args(
            &tool,
            Path::new("/home/amir/proj"),
            "tok-123",
            &provider,
            None,
            true,
        );
        assert!(args.iter().any(|a| a == "coder-net"));
        assert!(!args.iter().any(|a| a == "host"));
    }

    #[test]
    fn run_tool_args_hardens_the_container() {
        let tool = test_tool_config();
        let provider = test_provider_config();
        let (args, _) = run_tool_args(
            &tool,
            Path::new("/home/amir/proj"),
            "tok-123",
            &provider,
            None,
            true,
        );
        assert!(args.iter().any(|a| a == "--cap-drop=ALL"));
        assert!(args.iter().any(|a| a == "--security-opt=no-new-privileges"));
        assert!(args.iter().any(|a| a == "--pids-limit=256"));
    }

    #[test]
    fn run_tool_args_mounts_project_dir_and_data_volume() {
        let mut tool = test_tool_config();
        tool.data_volume = Some("claude-data:/home/coder/.claude".into());
        let provider = test_provider_config();
        let (args, _) = run_tool_args(
            &tool,
            Path::new("/home/amir/proj"),
            "tok-123",
            &provider,
            None,
            true,
        );
        assert!(args
            .iter()
            .any(|a| a.contains("/home/amir/proj:/workspace")));
        assert!(args.iter().any(|a| a == "claude-data:/home/coder/.claude"));
    }

    #[test]
    fn run_tool_args_keeps_session_token_out_of_argv() {
        let tool = test_tool_config();
        let provider = test_provider_config();
        let (args, env) = run_tool_args(&tool, Path::new("/p"), "tok-xyz", &provider, None, true);
        assert!(!args.iter().any(|a| a.contains("tok-xyz")));
        assert!(args.iter().any(|a| a == "OPENAI_API_KEY"));
        assert_eq!(
            env,
            vec![("OPENAI_API_KEY".to_string(), "tok-xyz".to_string())]
        );
    }

    #[test]
    fn run_tool_args_sends_token_as_anthropic_api_key_for_anthropic() {
        let tool = test_tool_config();
        let mut provider = test_provider_config();
        provider.wire_format = WireFormat::AnthropicMessages;
        let (args, env) = run_tool_args(&tool, Path::new("/p"), "tok-a", &provider, None, true);
        assert!(args.iter().any(|a| a == "ANTHROPIC_API_KEY"));
        assert_eq!(
            env,
            vec![("ANTHROPIC_API_KEY".to_string(), "tok-a".to_string())]
        );
    }

    #[test]
    fn run_tool_args_passes_model_via_tool_template() {
        let tool = test_tool_config(); // aider
        let provider = test_provider_config();
        let (args, _) = run_tool_args(&tool, Path::new("/p"), "t", &provider, Some("m1"), true);
        let tail: Vec<&str> = args
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(tail, vec!["--model", "openai/m1"]);
    }

    #[test]
    fn proxy_up_args_passes_keys_by_name_only_and_values_via_env() {
        let mut keys = HashMap::new();
        keys.insert("anthropic".to_string(), "abc".to_string());
        let args = proxy_up_args(&keys, Path::new("/h/providers.toml"), None);
        assert!(args.iter().any(|a| a == "CODER_KEY_ANTHROPIC"));
        assert!(args.iter().any(|a| a == "CODER_ADMIN_TOKEN"));
        assert!(!args.iter().any(|a| a.contains("abc")));
        let env = proxy_up_env(&keys, "admin-tok");
        assert!(env.contains(&("CODER_KEY_ANTHROPIC".to_string(), "abc".to_string())));
        assert!(env.contains(&("CODER_ADMIN_TOKEN".to_string(), "admin-tok".to_string())));
    }

    #[test]
    fn proxy_up_args_mounts_host_providers_file_read_only() {
        let args = proxy_up_args(&HashMap::new(), Path::new("/h/providers.toml"), None);
        assert!(args
            .iter()
            .any(|a| a == "/h/providers.toml:/etc/coder/providers.toml:ro"));
    }

    #[test]
    fn proxy_joins_internal_and_egress_networks_tools_only_internal() {
        let args = proxy_up_args(&HashMap::new(), Path::new("/p.toml"), None);
        assert!(args.iter().any(|a| a == NETWORK));
        assert!(args.iter().any(|a| a == EGRESS_NETWORK));
        let (tool_args, _) = run_tool_args(
            &test_tool_config(),
            Path::new("/p"),
            "t",
            &test_provider_config(),
            None,
            true,
        );
        assert!(tool_args.iter().any(|a| a == NETWORK));
        assert!(!tool_args.iter().any(|a| a == EGRESS_NETWORK));
    }

    #[test]
    fn shared_network_is_created_internal() {
        assert_eq!(
            network_create_args(NETWORK, true),
            vec!["network", "create", "--internal", "coder-net"]
        );
        assert_eq!(
            network_create_args(EGRESS_NETWORK, false),
            vec!["network", "create", "coder-egress"]
        );
    }

    #[test]
    fn validate_bind_source_rejects_colons_and_relative_paths() {
        #[cfg(unix)]
        {
            assert!(validate_bind_source(Path::new("/home/a/proj")).is_ok());
            assert!(validate_bind_source(Path::new("/home/a/my:proj")).is_err());
        }
        #[cfg(windows)]
        {
            assert!(validate_bind_source(Path::new("C:\\Users\\a\\proj")).is_ok());
            assert!(validate_bind_source(Path::new("C:\\Users\\a\\my:proj")).is_err());
        }
        assert!(validate_bind_source(Path::new("myproj")).is_err());
    }

    #[test]
    fn proxy_admin_args_execs_inside_proxy_container() {
        assert_eq!(
            proxy_admin_args("POST", "/admin/tokens", Some("{}")),
            vec![
                "exec",
                "coder-proxy",
                "coder",
                "proxy-admin",
                "POST",
                "/admin/tokens",
                "--body",
                "{}"
            ]
        );
    }

    #[test]
    fn proxy_admin_revoke_args_never_contains_a_token() {
        let args = proxy_admin_revoke_args();
        assert_eq!(
            args,
            vec![
                "exec",
                "-e",
                REVOKE_TOKEN_ENV,
                "coder-proxy",
                "coder",
                "proxy-admin-revoke-self"
            ]
        );
        // No argument is a bare UUID-shaped string or otherwise secret-looking; the
        // only thing naming the token at all is the env var's NAME, never a value.
        assert!(args.iter().all(|a| a != "fake-session-token-1234"));
    }

    #[test]
    fn run_tool_args_uses_coder_proxy_hostname_for_openai() {
        let tool = test_tool_config();
        let provider = test_provider_config();
        let (args, _) = run_tool_args(&tool, Path::new("/p"), "tok-123", &provider, None, true);
        assert!(args
            .iter()
            .any(|a| a == "OPENAI_API_BASE=http://coder-proxy:8080/v1"));
    }

    #[test]
    fn run_tool_args_uses_coder_proxy_hostname_for_anthropic() {
        let tool = test_tool_config();
        let mut provider = test_provider_config();
        provider.wire_format = WireFormat::AnthropicMessages;
        let (args, _) = run_tool_args(&tool, Path::new("/p"), "tok-123", &provider, None, true);
        assert!(args
            .iter()
            .any(|a| a == "ANTHROPIC_BASE_URL=http://coder-proxy:8080"));
    }

    #[test]
    fn run_tool_args_omits_tty_flag_without_a_terminal() {
        let tool = test_tool_config();
        let provider = test_provider_config();
        let (args, _) = run_tool_args(&tool, Path::new("/p"), "t", &provider, None, false);
        assert!(args.iter().any(|a| a == "-i"));
        assert!(!args.iter().any(|a| a == "-it"));
    }

    #[test]
    fn proxy_up_args_does_not_use_host_network() {
        let args = proxy_up_args(&HashMap::new(), Path::new("/p.toml"), None);
        assert!(!args.iter().any(|a| a == "host"));
    }

    #[test]
    fn build_sandbox_image_args_uses_sandbox_containerfile_and_context() {
        let args = build_sandbox_image_args();
        let f_pos = args
            .iter()
            .position(|a| a == "-f")
            .expect("missing -f flag");
        assert_eq!(
            args.get(f_pos + 1).map(String::as_str),
            Some("containers/sandbox/Containerfile")
        );
        assert_eq!(args.last().map(String::as_str), Some("containers/sandbox"));
    }

    #[test]
    fn build_proxy_image_args_uses_proxy_containerfile_and_repo_root_context() {
        let args = build_proxy_image_args();
        let f_pos = args
            .iter()
            .position(|a| a == "-f")
            .expect("missing -f flag");
        assert_eq!(
            args.get(f_pos + 1).map(String::as_str),
            Some("containers/proxy/Containerfile")
        );
        assert_eq!(args.last().map(String::as_str), Some("."));
    }

    #[test]
    fn proxy_up_args_publishes_no_ports() {
        let args = proxy_up_args(&HashMap::new(), Path::new("/p.toml"), None);
        assert!(!args.iter().any(|a| a == "-p" || a == "--publish"));
    }
}
