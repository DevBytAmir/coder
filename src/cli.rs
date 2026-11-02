use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "coder", about = "Sandboxed agent launcher")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build the sandbox image
    Build,
    /// Manage the credential proxy
    Proxy {
        #[command(subcommand)]
        action: ProxyAction,
    },
    /// Fetch and filter a provider's available chat models
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Validate providers.toml/tools.toml without needing a live key
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check the local environment is ready (podman, images, proxy, PATH)
    Doctor,
    /// Print a shell completion script
    Completions { shell: clap_complete::Shell },
    /// Interactive first-run setup wizard
    Init,
    /// Run a tool inside the sandbox
    Run(RunArgs),
    /// Serve the credential proxy (internal)
    ProxyServe,
    /// Call the proxy's loopback-only admin API from inside the proxy container (internal)
    #[command(hide = true)]
    ProxyAdmin {
        method: String,
        path: String,
        #[arg(long)]
        body: Option<String>,
    },
    /// Revoke the session token named by `$CODER_REVOKE_TOKEN` (internal, env only, never argv)
    #[command(hide = true)]
    ProxyAdminRevokeSelf,
}

#[derive(Subcommand)]
pub enum ProxyAction {
    /// Start the proxy
    Up {
        /// Plaintext secrets file (KEY=value, chmod 600). Defaults to
        /// ~/.config/coder/secrets.env
        #[arg(long)]
        secrets_file: Option<PathBuf>,
    },
    /// Stop the proxy
    Down,
    /// Show proxy status
    Status,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Parse providers.toml/tools.toml and sanity-check each provider's upstream URL
    Check,
}

#[derive(Subcommand)]
pub enum ModelsAction {
    /// Fetch, filter, and write a provider's model list to
    /// ~/.config/coder/models/<provider>.json, then regenerate opencode.jsonc from
    /// every provider's refreshed list (run `coder build` afterward to apply it)
    Refresh {
        /// Provider to refresh (omit when using --all)
        provider: Option<String>,
        /// Refresh every configured provider instead of just one
        #[arg(long, conflicts_with = "provider")]
        all: bool,
        /// Plaintext secrets file (KEY=value, chmod 600). Defaults to
        /// ~/.config/coder/secrets.env
        #[arg(long)]
        secrets_file: Option<PathBuf>,
    },
    /// Print a provider's refreshed model ids, one per line
    List { provider: String },
}

#[derive(Args)]
pub struct RunArgs {
    /// Tool to launch
    pub tool: String,
    /// Working directory
    #[arg(short = 'C')]
    pub dir: Option<PathBuf>,
    /// Provider to use
    #[arg(long)]
    pub provider: Option<String>,
    /// Model to use
    #[arg(long)]
    pub model: Option<String>,
    /// Print the resolved podman invocation without running it, the proxy, or the
    /// sandbox-guard prompt
    #[arg(long)]
    pub dry_run: bool,
    /// Arguments passed through to the tool
    pub extra_args: Vec<String>,
}
