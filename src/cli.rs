use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "coder", about = "Sandboxed agent launcher", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build the sandbox and proxy images
    Build {
        /// Download the matching tagged source from GitHub and build from it, instead
        /// of the current directory. For a standalone release binary with no local
        /// checkout.
        #[arg(long, conflicts_with = "source_dir")]
        fetch: bool,
        /// With --fetch, extract the downloaded source here instead of a temp
        /// directory that gets deleted after the build. Reused (skipping the
        /// download) on a later `coder build --fetch --keep` if already present.
        #[arg(long, requires = "fetch")]
        keep: Option<PathBuf>,
        /// Build from an existing local checkout at this path instead of the
        /// current directory.
        #[arg(long, conflicts_with = "fetch")]
        source_dir: Option<PathBuf>,
    },
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
    Doctor {
        /// Print the results as JSON on stdout instead of text
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script
    Completions { shell: clap_complete::Shell },
    /// Interactive first-run setup wizard
    Init,
    /// Run a tool inside the sandbox
    Run(RunArgs),
    /// Serve the credential proxy (internal)
    #[command(hide = true)]
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
    /// Hooks for AI coding agents: block tool calls that would expose coder's secrets
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
}

#[derive(Subcommand)]
pub enum HookEvent {
    /// Read a `PreToolUse` event as JSON on stdin and, if the call would read the secrets
    /// file, print the proxy's environment, or run `coder init`, print a deny. Fails open:
    /// input it can't parse is let through.
    PreToolUse,
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
    /// Sandbox the project directory without the confirmation prompt (for scripts and
    /// agents). Requires an explicit -C. Never overrides the refusal of `$HOME`, `/`,
    /// and other dangerous directories.
    #[arg(short = 'y', long, requires = "dir")]
    pub yes: bool,
    /// Stop the tool container and exit with status 124 if it is still running after
    /// this long (e.g. 90, 90s, 30m, 2h)
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub timeout: Option<Duration>,
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

/// Parse a `--timeout` value: a whole number of seconds, optionally suffixed with `s`,
/// `m`, or `h`.
///
/// # Errors
///
/// Returns a message for clap to show if `text` isn't a positive whole number with an
/// optional unit, or overflows.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let invalid = || format!("invalid duration `{text}`: expected e.g. 90, 90s, 30m or 2h");
    let (number, unit_secs) = match text.as_bytes().last() {
        Some(b's') => (&text[..text.len() - 1], 1),
        Some(b'm') => (&text[..text.len() - 1], 60),
        Some(b'h') => (&text[..text.len() - 1], 3600),
        _ => (text, 1),
    };
    let count: u64 = number.parse().map_err(|_| invalid())?;
    if count == 0 {
        return Err(format!(
            "invalid duration `{text}`: must be greater than zero"
        ));
    }
    count
        .checked_mul(unit_secs)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_duration_accepts_bare_seconds_and_units() {
        assert_eq!(parse_duration("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Ok(Duration::from_mins(30)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
    }

    #[test]
    fn parse_duration_rejects_zero_negative_garbage_and_overflow() {
        for bad in [
            "0",
            "0m",
            "-5",
            "",
            "s",
            "5d",
            "1.5h",
            "abc",
            "18446744073709551615h",
        ] {
            assert!(parse_duration(bad).is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn yes_requires_an_explicit_dir() {
        assert!(Cli::try_parse_from(["coder", "run", "claude", "--yes"]).is_err());
        assert!(Cli::try_parse_from(["coder", "run", "claude", "-y", "-C", "/p"]).is_ok());
    }

    #[test]
    fn run_flags_parse_before_passthrough_args() {
        let cli = Cli::try_parse_from([
            "coder",
            "run",
            "claude",
            "-C",
            "/p",
            "--yes",
            "--timeout",
            "30m",
            "--",
            "-p",
            "fix the tests",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(args.yes);
        assert_eq!(args.timeout, Some(Duration::from_mins(30)));
        assert_eq!(args.extra_args, vec!["-p", "fix the tests"]);
    }
}
