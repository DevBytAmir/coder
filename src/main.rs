mod models;
mod proxy_entrypoint;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use coder::cli::{Cli, Command, ConfigAction, ModelsAction, ProxyAction, RunArgs};
use coder::config::defaults::{write_default_providers_if_missing, write_default_tools_if_missing};
use coder::config::providers::{load_providers, ProviderConfig};
use coder::config::tools::{load_tools, ToolConfig};
use coder::orchestrate::guard::guard_confirm;
use coder::orchestrate::podman::{
    build_proxy_image_args, build_sandbox_image_args, exec_podman, network_create_args,
    network_exists_args, network_is_internal_args, network_rm_args, proxy_admin_args,
    proxy_admin_revoke_args, proxy_down_args, proxy_status_args, proxy_up_args, proxy_up_env,
    run_podman_attached, run_tool_args, validate_bind_source, EGRESS_NETWORK, NETWORK,
    REVOKE_TOKEN_ENV,
};
use coder::orchestrate::project_root::{existing_file_arg, resolve_project_dir};
use coder::orchestrate::select::resolve_provider_for_run;
use coder::orchestrate::sorted_joined;
use coder::secrets::{FileSource, SecretsSource};

/// How long `coder run`/`coder proxy up` wait for a freshly started proxy to answer.
const PROXY_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PROXY_READY_POLL: std::time::Duration = std::time::Duration::from_millis(200);

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

fn config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config").join("coder"))
}

fn providers_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("providers.toml"))
}

fn tools_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("tools.toml"))
}

/// Where `--secrets-file` defaults to when not given explicitly.
fn default_secrets_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("secrets.env"))
}

/// Print podman's stdout/stderr through to ours, and turn a non-zero exit into an error.
fn print_and_check(output: &std::process::Output) -> Result<()> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        bail!("podman exited with status {}", output.status);
    }
    Ok(())
}

fn check_status(status: std::process::ExitStatus) -> Result<()> {
    if !status.success() {
        bail!("podman exited with status {status}");
    }
    Ok(())
}

fn load_or_init_providers() -> Result<HashMap<String, ProviderConfig>> {
    let path = providers_path()?;
    write_default_providers_if_missing(&path)?;
    load_providers(&path)
}

fn load_or_init_tools() -> Result<HashMap<String, ToolConfig>> {
    let path = tools_path()?;
    write_default_tools_if_missing(&path)?;
    load_tools(&path)
}

fn real_keys_for(
    providers: &HashMap<String, ProviderConfig>,
    secrets_file: Option<&Path>,
) -> Result<HashMap<String, String>> {
    let names: Vec<String> = providers.keys().cloned().collect();
    let path = match secrets_file {
        Some(p) => p.to_path_buf(),
        None => default_secrets_path()?,
    };
    FileSource { path }.load(&names)
}

/// Create `coder-net` (internal: no egress) and `coder-egress` (proxy-only egress) if
/// missing. A pre-existing non-internal `coder-net` from an older version is recreated.
fn ensure_networks() -> Result<()> {
    for (name, internal) in [(NETWORK, true), (EGRESS_NETWORK, false)] {
        if exec_podman(&network_exists_args(name), &[])?
            .status
            .success()
        {
            if !internal {
                continue;
            }
            let out = exec_podman(&network_is_internal_args(name), &[])?;
            if String::from_utf8_lossy(&out.stdout).trim() == "true" {
                continue;
            }
            eprintln!("recreating {name} as an --internal network (no internet egress)");
            let rm = exec_podman(&network_rm_args(name), &[])?;
            if !rm.status.success() {
                bail!(
                    "{name} exists without --internal and could not be removed ({}); \
                     stop containers using it and run `podman network rm {name}`",
                    String::from_utf8_lossy(&rm.stderr).trim()
                );
            }
        }
        print_and_check(&exec_podman(&network_create_args(name, internal), &[])?)?;
    }
    Ok(())
}

/// One admin-API call, made by `coder proxy-admin` inside the proxy container (the admin
/// listener is loopback-only there). Returns the response body.
fn admin_call(method: &str, path: &str, body: Option<&str>) -> Result<String> {
    let out = exec_podman(&proxy_admin_args(method, path, body), &[])?;
    if !out.status.success() {
        bail!(
            "proxy admin call {method} {path} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn wait_for_proxy_ready() -> Result<()> {
    let deadline = std::time::Instant::now() + PROXY_READY_TIMEOUT;
    loop {
        match admin_call("GET", "/admin/status", None) {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => {
                bail!(
                    "proxy did not become ready within {}s: {e:#}\n\
                     check `podman logs coder-proxy`",
                    PROXY_READY_TIMEOUT.as_secs()
                )
            }
            Err(_) => std::thread::sleep(PROXY_READY_POLL),
        }
    }
}

fn do_proxy_up(secrets_file: Option<&Path>) -> Result<()> {
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, secrets_file)?;

    // podman does no `~`/relative expansion for -v sources: pass absolute, canonical paths
    let providers_file = providers_path()?
        .canonicalize()
        .context("resolving providers.toml path")?;
    validate_bind_source(&providers_file)?;
    let secrets_file = match secrets_file {
        Some(p) => p.to_path_buf(),
        None => default_secrets_path()?,
    }
    .canonicalize()
    .context("resolving secrets file path")?;
    validate_bind_source(&secrets_file)?;

    ensure_networks()?;

    // shared secret for the admin API; lives only in the proxy container's environment
    let admin_token = uuid::Uuid::new_v4().to_string();
    let args = proxy_up_args(&real_keys, &providers_file, Some(&secrets_file));
    let env = proxy_up_env(&real_keys, &admin_token);
    print_and_check(&exec_podman(&args, &env)?)?;
    wait_for_proxy_ready()
}

fn proxy_is_running() -> bool {
    admin_call("GET", "/admin/status", None).is_ok()
}

/// Revokes a session token. Not `admin_call`: the token must never appear as a literal
/// argument in podman exec's argv (ps/cmdline-visible to other local users), it
/// travels by env only. Shared by `TokenGuard::drop` (normal/panicking exit) and the
/// Ctrl-C handler in `do_run` (SIGINT/SIGTERM kill the process directly, bypassing
/// Rust's normal unwind-and-drop; Drop alone would never run in that case).
fn revoke_token(token: &str) {
    let env = [(REVOKE_TOKEN_ENV.to_string(), token.to_string())];
    match exec_podman(&proxy_admin_revoke_args(), &env) {
        Ok(out) if out.status.success() => {}
        Ok(out) => eprintln!(
            "warning: could not revoke session token: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => eprintln!("warning: could not revoke session token: {e:#}"),
    }
}

/// Revokes the session token on drop, so a tool crash still revokes it.
struct TokenGuard {
    token: String,
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        revoke_token(&self.token);
    }
}

fn combine_extra_args(rewritten: Vec<String>, extra_args: &[String]) -> Vec<String> {
    if rewritten.is_empty() {
        extra_args.to_vec()
    } else {
        let mut combined = rewritten;
        combined.extend(extra_args.iter().skip(1).cloned());
        combined
    }
}

/// `cli_model` is what we hand the tool's own --model flag. `session_model` is what
/// the proxy enforces on every request in this session. For a tool whose CLI model
/// flag needs its own wrapping (aider's litellm `openai/{model}` routing prefix,
/// tools.toml's `model_cli_args` template) that wrapped string must NOT leak into the
/// real wire request: litellm doesn't always strip it when forwarding to a custom
/// base URL, which upstream then rejects as an unknown model. So any tool that takes
/// a model flag at all gets `session_model` pinned to the same bare value regardless
/// of whether --model was explicit, and the proxy corrects the wire body to match. A
/// tool whose template is empty (opencode: it manages its own model/options UI, no
/// CLI flag at all) only gets `session_model` pinned if the user explicitly asked for
/// one; otherwise its own /models picker chooses freely without being overridden.
fn resolve_models(
    args: &RunArgs,
    provider: &ProviderConfig,
    tool: &ToolConfig,
) -> (Option<String>, Option<String>) {
    let cli_model = args
        .model
        .clone()
        .or_else(|| provider.default_model.clone());
    let tool_has_model_flag = !tool.model_cli_args("x").is_empty();
    let session_model = if tool_has_model_flag {
        cli_model.clone()
    } else {
        args.model.clone()
    };
    (cli_model, session_model)
}

/// Returns the tool container's exit code.
fn do_run(args: &RunArgs) -> Result<i32> {
    let providers = load_or_init_providers()?;
    let tools = load_or_init_tools()?;

    let tool = tools.get(&args.tool).ok_or_else(|| {
        let list = sorted_joined(tools.keys());
        anyhow::anyhow!("unknown tool `{}`; configured tools: {list}", args.tool)
    })?;

    // resolves the provider and refuses a mismatched tool/provider pairing, one function,
    // deliberately, before prompting or touching podman
    let provider_name =
        resolve_provider_for_run(&providers, &args.tool, tool, args.provider.as_deref())?;
    let provider = &providers[&provider_name];

    let cwd = std::env::current_dir().context("determining current directory")?;
    // canonical once, up front: a relative -C must not become a podman named volume
    let effective_cwd = args
        .dir
        .as_deref()
        .unwrap_or(&cwd)
        .canonicalize()
        .context("resolving -C directory")?;
    let root_arg = existing_file_arg(&effective_cwd, args.extra_args.first().map(String::as_str));
    let (project_dir, rewritten) =
        resolve_project_dir(tool.project_root, &effective_cwd, root_arg.as_deref())?;
    validate_bind_source(&project_dir)?;

    if args.dry_run {
        let extra_args = combine_extra_args(rewritten, &args.extra_args);
        let (cli_model, _) = resolve_models(args, provider, tool);
        let (mut podman_args, podman_env) = run_tool_args(
            tool,
            &project_dir,
            "<session-token>",
            provider,
            cli_model.as_deref(),
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        );
        podman_args.extend(extra_args);
        println!("podman {}", podman_args.join(" "));
        for (name, value) in &podman_env {
            println!("env: {name}={value}");
        }
        return Ok(0);
    }

    let home = home_dir()?;
    if !guard_confirm(&project_dir, &home)? {
        bail!("aborted: not sandboxing {}", project_dir.display());
    }

    let extra_args = combine_extra_args(rewritten, &args.extra_args);
    let (cli_model, session_model) = resolve_models(args, provider, tool);

    if !proxy_is_running() {
        do_proxy_up(None)?;
    }

    let mint_body = serde_json::json!({
        "provider": provider_name,
        "model": session_model,
    });
    let mint_resp: serde_json::Value = serde_json::from_str(
        &admin_call("POST", "/admin/tokens", Some(&mint_body.to_string()))
            .context("minting session token from proxy")?,
    )
    .context("parsing token response from proxy")?;
    let token = mint_resp["token"]
        .as_str()
        .context("proxy did not return a token")?
        .to_string();

    let _guard = TokenGuard {
        token: token.clone(),
    };

    // Ctrl-C (SIGINT) or SIGTERM kill the process directly rather than unwinding the
    // stack, so TokenGuard::drop above would never run; revoke from the signal handler
    // too. Best-effort: if installing it fails, the normal Drop path still covers the
    // far more common clean-exit and panic cases.
    {
        let token_for_signal = token.clone();
        let _ = ctrlc::set_handler(move || {
            revoke_token(&token_for_signal);
            std::process::exit(130);
        });
    }

    let (mut podman_args, podman_env) = run_tool_args(
        tool,
        &project_dir,
        &token,
        provider,
        cli_model.as_deref(),
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    );
    podman_args.extend(extra_args);

    // interactive session: inherit the terminal rather than capturing output
    let status = run_podman_attached(&podman_args, &podman_env)?;
    Ok(status.code().unwrap_or(1))
}

/// Parse providers.toml/tools.toml (deserialization itself already rejects an invalid
/// `wire_format` or a missing required field) and sanity-check each provider's upstream URL.
/// No live key or network call needed.
fn do_config_check() -> Result<()> {
    let providers = load_or_init_providers()?;
    let tools = load_or_init_tools()?;
    let mut problems = Vec::new();

    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    for name in &names {
        let provider = &providers[*name];
        match reqwest::Url::parse(&provider.upstream) {
            Ok(_) => println!("provider `{name}`: ok ({})", provider.upstream),
            Err(e) => {
                problems.push(format!("provider `{name}`: invalid upstream URL: {e}"));
            }
        }
    }

    let mut tool_names: Vec<&String> = tools.keys().collect();
    tool_names.sort();
    for name in &tool_names {
        println!("tool `{name}`: ok (image {})", tools[*name].image);
    }

    if problems.is_empty() {
        println!(
            "{} providers, {} tools, all valid",
            names.len(),
            tool_names.len()
        );
        Ok(())
    } else {
        for p in &problems {
            eprintln!("{p}");
        }
        bail!("{} problem(s) found", problems.len());
    }
}

/// Check the local environment is actually ready to use `coder`: catches the kind of
/// thing that otherwise only surfaces as a confusing failure deep into `coder run`
/// (a missing secrets file, a missing image, a `coder` binary not on PATH).
fn do_doctor() -> Result<()> {
    let mut problems = Vec::new();

    match std::process::Command::new("podman")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            println!("podman: {}", String::from_utf8_lossy(&out.stdout).trim());
        }
        _ => problems.push("podman: not found on PATH".to_string()),
    }

    for image in ["coder-sandbox", "coder-proxy"] {
        let out = std::process::Command::new("podman")
            .args(["images", "-q", image])
            .output();
        match out {
            Ok(o) if !o.stdout.is_empty() => println!("image `{image}`: built"),
            _ => problems.push(format!("image `{image}`: not built, run `coder build`")),
        }
    }

    if proxy_is_running() {
        println!("proxy: running");
        let ports = std::process::Command::new("podman")
            .args(["port", "coder-proxy"])
            .output();
        match ports {
            Ok(o) if o.stdout.is_empty() => println!(
                "proxy admin API: not published to the host (expected, reached via \
                 `podman exec` only)"
            ),
            Ok(o) => problems.push(format!(
                "proxy: unexpectedly has a published port: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            )),
            Err(e) => problems.push(format!("proxy: could not check published ports: {e}")),
        }
    } else {
        println!("proxy: not running (run `coder proxy up` when you need it)");
    }

    match default_secrets_path() {
        Ok(path) if path.exists() => println!("secrets file: found at {}", path.display()),
        Ok(path) => println!(
            "secrets file: none at {} (pass --secrets-file, or create it, before `coder \
             proxy up`)",
            path.display()
        ),
        Err(e) => problems.push(format!("secrets file: could not resolve default path: {e}")),
    }

    match std::env::current_exe() {
        Ok(exe) => {
            let on_path = std::env::var("PATH")
                .ok()
                .and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|dir| dir.join("coder"))
                        .find(|candidate| candidate.exists())
                })
                .is_some();
            if on_path {
                println!("coder binary: on PATH ({})", exe.display());
            } else {
                problems.push(format!(
                    "coder binary: not found on PATH (currently running from {})",
                    exe.display()
                ));
            }
        }
        Err(e) => problems.push(format!("coder binary: could not determine location: {e}")),
    }

    if problems.is_empty() {
        println!("all checks passed");
        Ok(())
    } else {
        eprintln!();
        for p in &problems {
            eprintln!("problem: {p}");
        }
        bail!("{} problem(s) found", problems.len());
    }
}

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Interactive first-run setup: ensures default config exists, then walks through which
/// providers to store a key for now, writing them into the default secrets file.
fn do_init() -> Result<()> {
    let providers = load_or_init_providers()?;
    load_or_init_tools()?;
    println!(
        "providers.toml / tools.toml are ready at {}",
        config_dir()?.display()
    );

    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    println!(
        "\nConfigured providers: {}",
        sorted_joined(providers.keys())
    );
    println!("(ollama needs no key; add others to providers.toml later if you need more)");

    let path = default_secrets_path()?;
    println!(
        "\nKeys you set now are written to a plaintext file: {}",
        path.display()
    );

    let mut lines = Vec::new();
    for name in &names {
        let answer = prompt_line(&format!("\nSet an API key for `{name}` now? [y/N] "))?;
        if !matches!(answer.as_str(), "y" | "Y" | "yes" | "YES") {
            continue;
        }
        let key = rpassword::prompt_password(format!("API key for {name}: "))?;
        if key.is_empty() {
            println!("(empty, skipping)");
            continue;
        }
        lines.push(format!("{}_API_KEY={key}", name.to_uppercase()));
    }

    if !lines.is_empty() {
        let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
        if !existing.is_empty() && !existing.ends_with('\n') {
            existing.push('\n');
        }
        existing.push_str(&lines.join("\n"));
        existing.push('\n');
        std::fs::write(&path, existing)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        println!("\nwrote {}", path.display());
    }

    println!("\nNext steps:");
    println!("  coder build");
    println!("  coder proxy up");
    println!("  coder run <aider|opencode|claude> -C <project> --provider <name>");
    Ok(())
}

/// Path to the sandbox image's opencode config, regenerated by `coder models refresh`.
/// Resolved relative to the repo root since `coder build`/`run` already require that cwd.
fn opencode_jsonc_path() -> PathBuf {
    PathBuf::from("containers/sandbox/opencode.jsonc")
}

fn do_models_refresh(provider_name: &str, secrets_file: Option<&Path>) -> Result<()> {
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, secrets_file)?;
    let dir = config_dir()?;
    models::refresh_one_provider(provider_name, &providers, &real_keys, &dir)?;
    models::regenerate_opencode_and_report(&dir, &opencode_jsonc_path())
}

fn do_models_refresh_all(secrets_file: Option<&Path>) -> Result<()> {
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, secrets_file)?;
    let dir = config_dir()?;
    models::refresh_all_providers(&providers, &real_keys, &dir, &opencode_jsonc_path())
}

fn do_models_list(provider_name: &str) -> Result<()> {
    let dir = config_dir()?;
    let path = models::shared_models_path(&dir, provider_name);
    for id in models::read_shared_list(&path)? {
        println!("{id}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build => {
            check_status(run_podman_attached(&build_sandbox_image_args(), &[])?)?;
            check_status(run_podman_attached(&build_proxy_image_args(), &[])?)
        }
        Command::Proxy { action } => match action {
            ProxyAction::Up { secrets_file } => do_proxy_up(secrets_file.as_deref()),
            ProxyAction::Down => print_and_check(&exec_podman(&proxy_down_args(), &[])?),
            ProxyAction::Status => {
                print_and_check(&exec_podman(&proxy_status_args(), &[])?)?;
                if let Ok(status) = admin_call("GET", "/admin/status", None) {
                    println!("{status}");
                }
                Ok(())
            }
        },
        Command::Models { action } => match action {
            ModelsAction::Refresh {
                provider,
                all,
                secrets_file,
            } => {
                if all {
                    do_models_refresh_all(secrets_file.as_deref())
                } else {
                    let provider =
                        provider.context("a provider is required unless --all is given")?;
                    do_models_refresh(&provider, secrets_file.as_deref())
                }
            }
            ModelsAction::List { provider } => do_models_list(&provider),
        },
        Command::Config { action } => match action {
            ConfigAction::Check => do_config_check(),
        },
        Command::Doctor => do_doctor(),
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut <Cli as clap::CommandFactory>::command(),
                "coder",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Command::Init => do_init(),
        Command::Run(args) => {
            let code = do_run(&args)?;
            std::process::exit(code)
        }
        Command::ProxyServe => {
            tokio::runtime::Runtime::new()?.block_on(proxy_entrypoint::run_proxy_serve())
        }
        Command::ProxyAdmin { method, path, body } => {
            proxy_entrypoint::run_proxy_admin(&method, &path, body.as_deref())
        }
        Command::ProxyAdminRevokeSelf => proxy_entrypoint::run_proxy_admin_revoke_self(),
    }
}
