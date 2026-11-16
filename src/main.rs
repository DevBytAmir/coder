mod models;
mod proxy_entrypoint;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use coder::agent_hook;
use coder::cli::{
    completion_command, Cli, Command, ConfigAction, HookEvent, ModelsAction, ProxyAction, RunArgs,
};
use coder::config::defaults::{write_default_providers_if_missing, write_default_tools_if_missing};
use coder::config::providers::{load_providers, ProviderConfig};
use coder::config::tools::{load_tools, ToolConfig};
use coder::exit::{self, ExitError};
use coder::orchestrate::guard::{guard_confirm, is_refused_dir, Confirmation};
use coder::orchestrate::podman::{
    build_proxy_image_args, build_sandbox_image_args, exec_podman, network_create_args,
    network_exists_args, network_is_internal_args, network_rm_args, parse_probe_output,
    provider_probe_args, proxy_admin_args, proxy_admin_revoke_args, proxy_down_args,
    proxy_exists_args, proxy_status_args, proxy_up_args, proxy_up_env, run_podman_attached,
    run_podman_attached_with_timeout, run_tool_args, stop_container_args, validate_bind_source,
    RunContainer, RunOutcome, EGRESS_NETWORK, NETWORK, REVOKE_TOKEN_ENV, STOP_GRACE,
};
use coder::orchestrate::project_root::{existing_file_arg, resolve_project_dir};
use coder::orchestrate::select::resolve_provider_for_run;
use coder::orchestrate::sorted_joined;
use coder::paths;
use coder::secrets::{FileSource, SecretsSource};
use coder::source_fetch::{fetch_source, find_existing_checkout, looks_like_source_checkout};

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
    if proxy_is_running() {
        println!(
            "proxy already running; run `coder proxy down` first to restart it (e.g. after \
             changing keys)"
        );
        return Ok(());
    }
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, secrets_file)?;

    // podman does no `~`/relative expansion for -v sources: pass absolute, canonical paths
    let providers_file =
        paths::canonicalize(&providers_path()?).context("resolving providers.toml path")?;
    validate_bind_source(&providers_file)?;
    let secrets_file = match secrets_file {
        Some(p) => p.to_path_buf(),
        None => default_secrets_path()?,
    };
    let secrets_file = paths::canonicalize(&secrets_file).context("resolving secrets file path")?;
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

/// Ask the proxy for a session token scoped to `provider` (and `model`, if pinned).
fn mint_session_token(provider: &str, model: Option<&str>) -> Result<String> {
    let mint_body = serde_json::json!({
        "provider": provider,
        "model": model,
    });
    let mint_resp: serde_json::Value = serde_json::from_str(
        &admin_call("POST", "/admin/tokens", Some(&mint_body.to_string()))
            .context("minting session token from proxy")?,
    )
    .context("parsing token response from proxy")?;
    Ok(mint_resp["token"]
        .as_str()
        .context("proxy did not return a token")?
        .to_string())
}

/// SIGINT/SIGTERM/SIGHUP end the process without unwinding, so `TokenGuard::drop` never
/// runs: revoke here too, and stop the container so a run killed by its caller doesn't
/// carry on in the background. Best-effort; the Drop path still covers normal exits.
fn install_interrupt_handler(token: &str, container_name: &str) {
    let token = token.to_string();
    let container_name = container_name.to_string();
    let _ = ctrlc::set_handler(move || {
        revoke_token(&token);
        let _ = exec_podman(&stop_container_args(&container_name, STOP_GRACE), &[]);
        std::process::exit(exit::INTERRUPTED);
    });
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
    let effective_cwd = paths::canonicalize(args.dir.as_deref().unwrap_or(&cwd))
        .context("resolving -C directory")?;
    let root_arg = existing_file_arg(&effective_cwd, args.extra_args.first().map(String::as_str));
    let (project_dir, rewritten) =
        resolve_project_dir(tool.project_root, &effective_cwd, root_arg.as_deref())?;

    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let container = RunContainer::new(&args.tool, &run_id[..12], args.timeout);

    // refuse before --dry-run, so a dry run can't okay what a real run refuses
    let home = home_dir()?;
    if is_refused_dir(&project_dir, &home) {
        return Err(ExitError::new(
            exit::NOT_PERMITTED,
            format!("aborted: not sandboxing {}", project_dir.display()),
        )
        .into());
    }
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
            &container,
        );
        podman_args.extend(extra_args);
        println!("podman {}", podman_args.join(" "));
        for (name, value) in &podman_env {
            println!("env: {name}={value}");
        }
        return Ok(0);
    }

    let confirmation = if args.yes {
        Confirmation::AssumeYes
    } else {
        Confirmation::Prompt
    };
    if !guard_confirm(
        &project_dir,
        &home,
        confirmation,
        std::io::stdin().is_terminal(),
    )? {
        return Err(ExitError::new(
            exit::NOT_PERMITTED,
            format!("aborted: not sandboxing {}", project_dir.display()),
        )
        .into());
    }

    let extra_args = combine_extra_args(rewritten, &args.extra_args);
    let (cli_model, session_model) = resolve_models(args, provider, tool);

    if !proxy_is_running() {
        do_proxy_up(None)?;
    }

    let token = mint_session_token(&provider_name, session_model.as_deref())?;

    let _guard = TokenGuard {
        token: token.clone(),
    };

    install_interrupt_handler(&token, &container.name);

    let (mut podman_args, podman_env) = run_tool_args(
        tool,
        &project_dir,
        &token,
        provider,
        cli_model.as_deref(),
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        &container,
    );
    podman_args.extend(extra_args);

    // inherit the terminal rather than capturing output
    match run_podman_attached_with_timeout(&podman_args, &podman_env, &container)? {
        RunOutcome::Exited(status) => Ok(exit::from_status(status)),
        RunOutcome::TimedOut => {
            eprintln!("coder: timed out, stopped {}", container.name);
            Ok(exit::TIMED_OUT)
        }
    }
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

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Ok,
    /// Not ready, but fine as is (e.g. the proxy isn't running yet).
    Info,
    Problem,
}

#[derive(serde::Serialize)]
struct Check {
    name: String,
    status: CheckStatus,
    detail: String,
}

impl Check {
    fn new(name: impl Into<String>, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }
}

/// Which configured providers have a key loaded from the default secrets file, for
/// `coder doctor`. A missing key isn't a problem: some providers (Ollama) legitimately
/// need none.
fn key_status_checks() -> Result<Vec<Check>> {
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, None)?;
    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| {
            let check_name = format!("key for `{name}`");
            if real_keys.contains_key(name) {
                Check::new(check_name, CheckStatus::Ok, "loaded")
            } else {
                Check::new(
                    check_name,
                    CheckStatus::Info,
                    "none (fine if this provider needs none, e.g. ollama)",
                )
            }
        })
        .collect())
}

/// Check the local environment is actually ready to use `coder`: catches the kind of
/// thing that otherwise only surfaces as a confusing failure deep into `coder run`
/// (a missing secrets file, a missing image, a `coder` binary not on PATH).
fn doctor_checks() -> Vec<Check> {
    use CheckStatus::{Ok, Problem};
    let mut checks = Vec::new();

    match std::process::Command::new("podman")
        .arg("--version")
        .output()
    {
        Result::Ok(out) if out.status.success() => checks.push(Check::new(
            "podman",
            Ok,
            String::from_utf8_lossy(&out.stdout).trim(),
        )),
        _ => checks.push(Check::new("podman", Problem, "not found on PATH")),
    }

    for image in ["coder-sandbox", "coder-proxy"] {
        let out = std::process::Command::new("podman")
            .args(["images", "-q", image])
            .output();
        let name = format!("image `{image}`");
        match out {
            Result::Ok(o) if !o.stdout.is_empty() => checks.push(Check::new(name, Ok, "built")),
            _ => checks.push(Check::new(name, Problem, "not built, run `coder build`")),
        }
    }

    location_checks(&mut checks);
    proxy_checks(&mut checks);
    secrets_checks(&mut checks);
    provider_reach_checks(&mut checks);
    binary_check(&mut checks);
    checks
}

/// Where coder keeps its files: settings and the generated model list in the config
/// directory, and the source `coder build` would use.
fn location_checks(checks: &mut Vec<Check>) {
    use CheckStatus::{Info, Ok, Problem};
    match config_dir() {
        Result::Ok(dir) if dir.is_dir() => {
            checks.push(Check::new(
                "config directory",
                Ok,
                dir.display().to_string(),
            ));
        }
        Result::Ok(dir) => checks.push(Check::new(
            "config directory",
            Info,
            format!("{} (created on first use)", dir.display()),
        )),
        Err(e) => checks.push(Check::new("config directory", Problem, e.to_string())),
    }
    if let Result::Ok(path) = opencode_jsonc_path() {
        checks.push(if path.is_file() {
            Check::new("model list", Ok, path.display().to_string())
        } else {
            Check::new(
                "model list",
                Info,
                "not generated; opencode uses the shipped default until `coder models \
                 refresh`",
            )
        });
    }
    let version = env!("CARGO_PKG_VERSION");
    checks.push(if looks_like_source_checkout(Path::new(".")) {
        Check::new("build source", Ok, "the coder checkout in this directory")
    } else {
        match managed_source_dir(version) {
            Result::Ok(dir) => match find_existing_checkout(&dir) {
                Some(root) => Check::new("build source", Ok, root.display().to_string()),
                None => Check::new(
                    "build source",
                    Info,
                    format!(
                        "not downloaded yet; `coder build` downloads it into {}",
                        dir.display()
                    ),
                ),
            },
            Err(e) => Check::new("build source", Problem, e.to_string()),
        }
    });
}

fn proxy_checks(checks: &mut Vec<Check>) {
    use CheckStatus::{Info, Ok, Problem};
    if proxy_is_running() {
        checks.push(Check::new("proxy", Ok, "running"));
        let ports = std::process::Command::new("podman")
            .args(["port", "coder-proxy"])
            .output();
        checks.push(match ports {
            Result::Ok(o) if o.stdout.is_empty() => Check::new(
                "proxy admin API",
                Ok,
                "not published to the host (expected, reached via `podman exec` only)",
            ),
            Result::Ok(o) => Check::new(
                "proxy",
                Problem,
                format!(
                    "unexpectedly has a published port: {}",
                    String::from_utf8_lossy(&o.stdout).trim()
                ),
            ),
            Err(e) => Check::new(
                "proxy",
                Problem,
                format!("could not check published ports: {e}"),
            ),
        });
    } else {
        checks.push(Check::new(
            "proxy",
            Info,
            "not running (run `coder proxy up` when you need it)",
        ));
    }
}

/// Whether each provider's upstream answers from `coder-egress`, the proxy's network.
/// Unreachable is only a problem for a provider with a key loaded: the defaults include
/// providers many users never use.
fn provider_reach_checks(checks: &mut Vec<Check>) {
    let skip = |checks: &mut Vec<Check>, why: &str| {
        checks.push(Check::new("provider reachability", CheckStatus::Info, why));
    };
    let ready = |args: &[String]| exec_podman(args, &[]).is_ok_and(|o| o.status.success());
    if !ready(&network_exists_args(EGRESS_NETWORK)) {
        return skip(
            checks,
            "skipped: the proxy network is created by `coder proxy up`",
        );
    }
    if !ready(&["image".into(), "exists".into(), "coder-sandbox".into()]) {
        return skip(checks, "skipped: needs the coder-sandbox image");
    }
    let Result::Ok(providers) = load_or_init_providers() else {
        return skip(checks, "skipped: providers.toml could not be loaded");
    };
    let keys = real_keys_for(&providers, None).unwrap_or_default();
    let mut targets: Vec<(String, String)> = providers
        .iter()
        .map(|(name, p)| (name.clone(), p.upstream.clone()))
        .collect();
    targets.sort();
    let codes = match exec_podman(&provider_probe_args(&targets), &[]) {
        Result::Ok(out) => parse_probe_output(&String::from_utf8_lossy(&out.stdout)),
        Err(e) => return skip(checks, &format!("skipped: could not run the probe: {e}")),
    };
    for (name, upstream) in &targets {
        let code = codes.get(name).copied().unwrap_or(0);
        checks.push(reach_check(name, upstream, code, keys.contains_key(name)));
    }
}

fn reach_check(name: &str, upstream: &str, code: u16, has_key: bool) -> Check {
    let check_name = format!("provider `{name}`");
    if code != 0 {
        return Check::new(
            check_name,
            CheckStatus::Ok,
            format!("reachable from the proxy network (HTTP {code})"),
        );
    }
    let status = if has_key {
        CheckStatus::Problem
    } else {
        CheckStatus::Info
    };
    let mut detail = format!("unreachable from the proxy network at {upstream}");
    if upstream.contains("host.containers.internal") {
        detail.push_str(
            ". If it runs on this host, check it's listening; podman's host route can also go \
             stale after a network change: stop all podman containers on bridge networks to \
             reset it, or set `upstream` to this host's IP",
        );
    } else if !has_key {
        detail.push_str(" (fine if you don't use this provider)");
    }
    Check::new(check_name, status, detail)
}

fn secrets_checks(checks: &mut Vec<Check>) {
    use CheckStatus::{Info, Ok, Problem};
    match default_secrets_path() {
        Result::Ok(path) if path.exists() => {
            checks.push(Check::new(
                "secrets file",
                Ok,
                format!("found at {}", path.display()),
            ));
            match key_status_checks() {
                Result::Ok(keys) => checks.extend(keys),
                Err(e) => checks.push(Check::new(
                    "secrets file",
                    Problem,
                    format!("could not read keys: {e}"),
                )),
            }
        }
        Result::Ok(path) => checks.push(Check::new(
            "secrets file",
            Info,
            format!(
                "none at {} (pass --secrets-file, or create it, before `coder proxy up`)",
                path.display()
            ),
        )),
        Err(e) => checks.push(Check::new(
            "secrets file",
            Problem,
            format!("could not resolve default path: {e}"),
        )),
    }
}

fn binary_check(checks: &mut Vec<Check>) {
    use CheckStatus::{Ok, Problem};
    match std::env::current_exe() {
        Result::Ok(exe) => {
            let on_path = std::env::var("PATH")
                .ok()
                .and_then(|path| {
                    std::env::split_paths(&path)
                        // `coder.exe` on Windows
                        .map(|dir| dir.join(format!("coder{}", std::env::consts::EXE_SUFFIX)))
                        .find(|candidate| candidate.exists())
                })
                .is_some();
            checks.push(if on_path {
                Check::new("coder binary", Ok, format!("on PATH ({})", exe.display()))
            } else {
                Check::new(
                    "coder binary",
                    Problem,
                    format!(
                        "not found on PATH (currently running from {})",
                        exe.display()
                    ),
                )
            });
        }
        Err(e) => checks.push(Check::new(
            "coder binary",
            Problem,
            format!("could not determine location: {e}"),
        )),
    }
}

/// Fails if any check is a problem, after printing them all (text or `--json`).
fn do_doctor(json: bool) -> Result<()> {
    let checks = doctor_checks();
    let problems = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Problem)
        .count();

    if json {
        #[derive(serde::Serialize)]
        struct Report<'a> {
            ok: bool,
            checks: &'a [Check],
        }
        let report = Report {
            ok: problems == 0,
            checks: &checks,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for check in checks.iter().filter(|c| c.status != CheckStatus::Problem) {
            println!("{}: {}", check.name, check.detail);
        }
        if problems == 0 {
            println!("all checks passed");
        } else {
            eprintln!();
            for check in checks.iter().filter(|c| c.status == CheckStatus::Problem) {
                eprintln!("problem: {}: {}", check.name, check.detail);
            }
        }
    }

    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    Ok(())
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

/// The opencode config `coder models refresh` generates. `coder build` bakes it into the
/// sandbox image in place of the shipped default.
fn opencode_jsonc_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("opencode.jsonc"))
}

/// The sandbox image's build context: `shipped` (`containers/sandbox`), or, when `coder
/// models refresh` has generated an opencode config at `generated`, a temporary copy of
/// `shipped` with that config in place of the shipped default. The temp dir must outlive
/// the build.
fn sandbox_build_context(
    shipped: &Path,
    generated: &Path,
) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    if !generated.is_file() {
        return Ok((shipped.to_path_buf(), None));
    }
    let staged = tempfile::tempdir().context("creating a temp directory")?;
    copy_dir(shipped, staged.path())?;
    std::fs::copy(generated, staged.path().join("opencode.jsonc"))
        .with_context(|| format!("copying {}", generated.display()))?;
    println!("using the model list from {}", generated.display());
    Ok((staged.path().to_path_buf(), Some(staged)))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir(&target)?;
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn do_models_refresh(provider_name: &str, secrets_file: Option<&Path>) -> Result<()> {
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, secrets_file)?;
    let dir = config_dir()?;
    models::refresh_one_provider(provider_name, &providers, &real_keys, &dir)?;
    models::regenerate_opencode_and_report(&dir, &opencode_jsonc_path()?)
}

fn do_models_refresh_all(secrets_file: Option<&Path>) -> Result<()> {
    let providers = load_or_init_providers()?;
    let real_keys = real_keys_for(&providers, secrets_file)?;
    let dir = config_dir()?;
    models::refresh_all_providers(&providers, &real_keys, &dir, &opencode_jsonc_path()?)
}

fn do_models_list(provider_name: &str) -> Result<()> {
    let dir = config_dir()?;
    let path = models::shared_models_path(&dir, provider_name);
    for id in models::read_shared_list(&path)? {
        println!("{id}");
    }
    Ok(())
}

const GITHUB_REPO: &str = "DevBytAmir/coder";

/// Restores the process's original working directory on drop, so `coder build`
/// leaves the shell it was run from unaffected even though it has to `chdir` to
/// build from fetched or `--source-dir` source.
struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn enter(dir: &Path) -> Result<Self> {
        let original = std::env::current_dir().context("determining current directory")?;
        std::env::set_current_dir(dir)
            .with_context(|| format!("changing directory to {}", dir.display()))?;
        Ok(Self { original })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Where `coder build` keeps downloaded source, one directory per version.
fn managed_source_dir(version: &str) -> Result<PathBuf> {
    Ok(home_dir()?
        // separate joins, so Windows gets `\` throughout
        .join(".local")
        .join("share")
        .join("coder")
        .join("source")
        .join(format!("v{version}")))
}

/// Resolves which source tree to build the images from: `--source-dir`, a coder
/// checkout in the current directory, or this version's source under
/// `~/.local/share/coder/source/`, downloaded the first time and again with `--fetch`.
/// The returned guard restores the original working directory once the build is done.
fn resolve_build_source(fetch: bool, source_dir: Option<&Path>) -> Result<Option<CwdGuard>> {
    if let Some(dir) = source_dir {
        if !looks_like_source_checkout(dir) {
            bail!(
                "{} doesn't look like a coder checkout (no Cargo.toml/containers/)",
                dir.display()
            );
        }
        println!("building from {}", dir.display());
        return Ok(Some(CwdGuard::enter(dir)?));
    }
    if !fetch && looks_like_source_checkout(Path::new(".")) {
        return Ok(None);
    }
    let version = env!("CARGO_PKG_VERSION");
    let dest = managed_source_dir(version)?;
    let root = match find_existing_checkout(&dest) {
        Some(root) if !fetch => {
            println!("using coder v{version} source at {}", root.display());
            root
        }
        _ => download_source(version, &dest)?,
    };
    Ok(Some(CwdGuard::enter(&root)?))
}

/// Downloads this version's source into `dest`, replacing what's there only once the
/// download has fully succeeded, and returns the extracted source root.
fn download_source(version: &str, dest: &Path) -> Result<PathBuf> {
    let parent = dest.parent().context("source directory has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    println!(
        "downloading coder v{version} source from GitHub into {}...",
        dest.display()
    );
    let staging = tempfile::tempdir_in(parent).context("creating a staging directory")?;
    fetch_source(GITHUB_REPO, version, staging.path())?;
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    let staged = staging.keep();
    if let Err(err) = std::fs::rename(&staged, dest) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(err).with_context(|| format!("moving the source into {}", dest.display()));
    }
    find_existing_checkout(dest)
        .with_context(|| format!("{} doesn't contain a coder checkout", dest.display()))
}

fn do_build(fetch: bool, source_dir: Option<&Path>) -> Result<()> {
    let _source = resolve_build_source(fetch, source_dir)?;
    let (sandbox_context, _staged) =
        sandbox_build_context(Path::new("containers/sandbox"), &opencode_jsonc_path()?)?;
    check_status(run_podman_attached(
        &build_sandbox_image_args(&sandbox_context),
        &[],
    )?)?;
    check_status(run_podman_attached(&build_proxy_image_args(), &[])?)
}

/// Fails open: unreadable or unparseable input lets the tool call through, since a hook
/// that errors must never block every tool call in the session.
fn run_pre_tool_use_hook() -> Result<()> {
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
    let Ok(input) = serde_json::from_str::<agent_hook::HookInput>(&raw) else {
        return Ok(());
    };
    if let Some(reason) = agent_hook::deny_reason(&input, &config_dir()?) {
        println!("{}", agent_hook::deny_output(reason));
    }
    Ok(())
}

fn main() {
    if let Err(err) = run_cli() {
        eprintln!("Error: {err:?}");
        std::process::exit(exit::code_for(&err));
    }
}

/// Rust ignores SIGPIPE, so printing to a closed pipe (`coder models list x | head`)
/// panics instead of exiting quietly like other CLIs. Restore the default for every
/// command except the proxy server, where a client hanging up mid-response must not kill
/// the whole process.
fn restore_default_sigpipe(command: &Command) {
    #[cfg(unix)]
    if !matches!(command, Command::ProxyServe) {
        // SAFETY: called once at startup before any threads exist; setting a signal's
        // disposition to SIG_DFL has no other preconditions.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    #[cfg(not(unix))]
    let _ = command;
}

fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    restore_default_sigpipe(&cli.command);
    match cli.command {
        Command::Build { fetch, source_dir } => do_build(fetch, source_dir.as_deref()),
        Command::Proxy { action } => match action {
            ProxyAction::Up { secrets_file } => do_proxy_up(secrets_file.as_deref()),
            ProxyAction::Down => {
                if exec_podman(&proxy_exists_args(), &[])?.status.success() {
                    print_and_check(&exec_podman(&proxy_down_args(), &[])?)
                } else {
                    println!("proxy is not running");
                    Ok(())
                }
            }
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
        Command::Doctor { json } => do_doctor(json),
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut completion_command(),
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
        Command::Hook {
            event: HookEvent::PreToolUse,
        } => run_pre_tool_use_hook(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_context_is_the_shipped_dir_until_a_model_list_is_generated() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = dir.path().join("sandbox");
        std::fs::create_dir(&shipped).unwrap();
        std::fs::write(shipped.join("Containerfile"), "FROM x\n").unwrap();
        std::fs::write(shipped.join("opencode.jsonc"), "shipped").unwrap();
        let generated = dir.path().join("opencode.jsonc");

        let (context, staged) = sandbox_build_context(&shipped, &generated).unwrap();
        assert_eq!(context, shipped);
        assert!(staged.is_none());

        std::fs::write(&generated, "generated").unwrap();
        let (context, staged) = sandbox_build_context(&shipped, &generated).unwrap();
        assert!(staged.is_some());
        assert_ne!(context, shipped);
        let read = |name: &str| std::fs::read_to_string(context.join(name)).unwrap();
        assert_eq!(read("Containerfile"), "FROM x\n");
        assert_eq!(read("opencode.jsonc"), "generated");
        assert_eq!(
            std::fs::read_to_string(shipped.join("opencode.jsonc")).unwrap(),
            "shipped",
            "the shipped file must not be modified"
        );
    }

    #[test]
    fn reachable_providers_are_ok_whatever_the_http_status() {
        for code in [200, 401, 404] {
            let check = reach_check("nim", "https://x", code, true);
            assert!(check.status == CheckStatus::Ok, "{code}");
        }
    }

    #[test]
    fn unreachable_is_a_problem_only_with_a_key_loaded() {
        assert!(reach_check("nim", "https://x", 0, true).status == CheckStatus::Problem);
        let unused = reach_check("openrouter", "https://x", 0, false);
        assert!(unused.status == CheckStatus::Info);
        assert!(unused.detail.contains("fine if you don't use"));
    }

    #[test]
    fn unreachable_host_service_explains_the_stale_route_fix() {
        let check = reach_check(
            "ollama",
            "http://host.containers.internal:11434/v1",
            0,
            false,
        );
        assert!(
            check.detail.contains("set `upstream` to this host's IP"),
            "{}",
            check.detail
        );
    }
}
