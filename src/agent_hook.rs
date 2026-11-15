//! `coder hook pre-tool-use`: a `PreToolUse` hook for AI coding agents that keeps real
//! provider keys out of the agent's context. It denies tool calls that would read the
//! secrets file or print the proxy's environment, and `coder init`. A guardrail against
//! accidental exposure, not a security boundary: the host side is trusted (see
//! SECURITY.md), and a determined caller can always find another way.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::orchestrate::guard::resolve_lexical;

#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Value,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// Files under the coder config directory an agent may read: everything but secrets.
const READABLE_CONFIG: &[&str] = &["/providers.toml", "/tools.toml", "/models"];

const SECRETS_REASON: &str = "coder's secrets file holds real provider API keys and must \
     never enter the agent's context. If a key is missing, ask the user to add it.";
const INIT_REASON: &str = "`coder init` sets up provider API keys and is for the user to \
     run themselves, not an agent.";
const PROXY_ENV_REASON: &str = "the coder-proxy container's environment holds real \
     provider API keys. Use `coder proxy status` or `podman logs coder-proxy` instead.";

/// What a tool call touches, as far as the secrets file is concerned.
enum Action<'a> {
    /// Reads or writes one file.
    File(&'a str),
    /// Edits the files named in an `apply_patch` patch.
    Patch(&'a str),
    /// Searches under `path`; `prints_content` if matching lines are shown, not only names.
    Search {
        path: &'a str,
        prints_content: bool,
    },
    Shell(&'a str),
}

fn action(input: &HookInput) -> Option<Action<'_>> {
    let field = |name: &str| input.tool_input.get(name).and_then(Value::as_str);
    Some(match input.tool_name.as_str() {
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            Action::File(field("file_path").or_else(|| field("notebook_path"))?)
        }
        "Grep" => Action::Search {
            path: field("path").unwrap_or("."),
            prints_content: field("output_mode") == Some("content"),
        },
        "apply_patch" => Action::Patch(field("command")?),
        "Bash" | "PowerShell" => Action::Shell(field("command")?),
        _ => return None,
    })
}

/// The reason to deny this tool call, or `None` to let it through. `config_dir` is
/// coder's config directory (`~/.config/coder`).
#[must_use]
pub fn deny_reason(input: &HookInput, config_dir: &Path) -> Option<&'static str> {
    let cwd = input.cwd.as_deref().unwrap_or(Path::new("/"));
    let secrets = config_dir.join("secrets.env");
    let is_secrets = |path: &str| same_file(&resolve(cwd, path), &secrets);

    let denied = match action(input)? {
        Action::File(path) => is_secrets(path),
        Action::Patch(patch) => patch_paths(patch).any(is_secrets),
        Action::Search {
            path,
            prints_content,
        } => {
            let path = resolve(cwd, path);
            same_file(&path, &secrets)
                || same_file(&path, config_dir)
                || (prints_content && resolve_lexical(&secrets).starts_with(resolve_lexical(&path)))
        }
        Action::Shell(command) => {
            let command = command.replace('\\', "/");
            if runs_coder_init(&command) {
                return Some(INIT_REASON);
            }
            if exposes_proxy_env(&command) {
                return Some(PROXY_ENV_REASON);
            }
            touches_secrets(&command, same_file(cwd, config_dir))
        }
    };
    denied.then_some(SECRETS_REASON)
}

/// The `PreToolUse` JSON that denies the call with `reason`.
#[must_use]
pub fn deny_output(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// Paths an `apply_patch` patch adds, updates, deletes, or moves to.
fn patch_paths(patch: &str) -> impl Iterator<Item = &str> {
    const MARKERS: &[&str] = &[
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    patch.lines().filter_map(|line| {
        MARKERS
            .iter()
            .find_map(|marker| line.strip_prefix(marker))
            .map(str::trim)
    })
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Whether two paths name the same file, following symlinks (see [`resolve_lexical`]),
/// so a symlink to the secrets file still matches.
fn same_file(a: &Path, b: &Path) -> bool {
    resolve_lexical(a) == resolve_lexical(b)
}

/// Whether the shell command refers to the coder config directory other than its
/// readable files, or to `secrets` from inside that directory.
fn touches_secrets(command: &str, cwd_is_config_dir: bool) -> bool {
    if cwd_is_config_dir && command.contains("secrets") {
        return true;
    }
    command.match_indices(".config/coder").any(|(at, needle)| {
        let rest = &command[at + needle.len()..];
        !READABLE_CONFIG.iter().any(|ok| rest.starts_with(ok))
    })
}

fn runs_coder_init(command: &str) -> bool {
    command_words(command)
        .windows(2)
        .any(|pair| (pair[0] == "coder" || pair[0].ends_with("/coder")) && pair[1] == "init")
}

/// `podman exec`/`inspect`/`cp` on the proxy, or reading a process's environment from
/// `/proc`, where the proxy's keys are visible from the host.
fn exposes_proxy_env(command: &str) -> bool {
    if command.contains("/proc/") && command.contains("/environ") {
        return true;
    }
    let words = command_words(command);
    words
        .iter()
        .any(|w| *w == "podman" || w.ends_with("/podman"))
        && words
            .iter()
            .any(|w| matches!(*w, "exec" | "inspect" | "cp"))
        && words.iter().any(|w| w.starts_with("coder-proxy"))
}

/// Words of a shell command, split on whitespace and the separators `;&|()` plus quotes.
fn command_words(command: &str) -> Vec<&str> {
    command
        .split(|c: char| c.is_whitespace() || ";&|()'\"`".contains(c))
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONFIG: &str = "/home/u/.config/coder";

    fn check(tool: &str, tool_input: Value, cwd: &str) -> Option<&'static str> {
        let input = HookInput {
            tool_name: tool.to_string(),
            tool_input,
            cwd: Some(PathBuf::from(cwd)),
        };
        deny_reason(&input, Path::new(CONFIG))
    }

    fn bash(command: &str) -> Option<&'static str> {
        check("Bash", json!({ "command": command }), "/home/u/project")
    }

    #[test]
    fn file_tools_cannot_open_the_secrets_file() {
        for tool in ["Read", "Edit", "Write", "MultiEdit"] {
            let input = json!({ "file_path": "/home/u/.config/coder/secrets.env" });
            assert_eq!(check(tool, input, "/"), Some(SECRETS_REASON), "{tool}");
        }
        let relative = json!({ "file_path": "../.config/coder/./secrets.env" });
        assert_eq!(
            check("Read", relative, "/home/u/project"),
            Some(SECRETS_REASON)
        );
    }

    #[test]
    fn file_tools_can_open_other_files_including_coder_config() {
        for path in [
            "/home/u/.config/coder/providers.toml",
            "/home/u/project/secrets.env",
            "/home/u/project/src/main.rs",
        ] {
            assert_eq!(
                check("Read", json!({ "file_path": path }), "/"),
                None,
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_the_secrets_file_is_still_denied() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("secrets.env"), "K=v\n").unwrap();
        let link = dir.path().join("innocent.txt");
        std::os::unix::fs::symlink(config.join("secrets.env"), &link).unwrap();
        let input = HookInput {
            tool_name: "Read".into(),
            tool_input: json!({ "file_path": link }),
            cwd: None,
        };
        assert_eq!(deny_reason(&input, &config), Some(SECRETS_REASON));
    }

    #[test]
    fn grep_cannot_search_the_secrets_but_can_list_files_elsewhere() {
        let in_config = json!({ "pattern": "KEY", "path": CONFIG });
        assert_eq!(check("Grep", in_config, "/"), Some(SECRETS_REASON));
        let home_content = json!({ "pattern": "sk-", "path": "/home/u", "output_mode": "content" });
        assert_eq!(check("Grep", home_content, "/"), Some(SECRETS_REASON));
        let home_names = json!({ "pattern": "sk-", "path": "/home/u" });
        assert_eq!(check("Grep", home_names, "/"), None);
        let project = json!({ "pattern": "x", "output_mode": "content" });
        assert_eq!(check("Grep", project, "/home/u/project"), None);
    }

    #[test]
    fn patches_cannot_touch_the_secrets_file() {
        let patch = |path: &str| json!({ "command": format!("*** Begin Patch\n*** Update File: {path}\n@@\n-a\n+b\n*** End Patch") });
        assert_eq!(
            check(
                "apply_patch",
                patch("/home/u/.config/coder/secrets.env"),
                "/"
            ),
            Some(SECRETS_REASON)
        );
        assert_eq!(
            check("apply_patch", patch("secrets.env"), CONFIG),
            Some(SECRETS_REASON)
        );
        let moved = json!({ "command": "*** Begin Patch\n*** Update File: a.txt\n*** Move to: /home/u/.config/coder/secrets.env\n*** End Patch" });
        assert_eq!(check("apply_patch", moved, "/"), Some(SECRETS_REASON));
        assert_eq!(
            check("apply_patch", patch("src/main.rs"), "/home/u/project"),
            None
        );
    }

    #[test]
    fn shell_commands_cannot_read_the_secrets_file() {
        for command in [
            "cat ~/.config/coder/secrets.env",
            "cat $HOME/.config/coder/secrets.env | head",
            "grep -r KEY ~/.config/coder",
            "cat ~/.config/coder/*",
            "cd ~/.config/coder && cat secrets.env",
            r"type C:\Users\u\.config\coder\secrets.env",
        ] {
            assert_eq!(bash(command), Some(SECRETS_REASON), "{command}");
        }
        let in_config_dir = check("Bash", json!({ "command": "cat secrets.env" }), CONFIG);
        assert_eq!(in_config_dir, Some(SECRETS_REASON));
    }

    #[test]
    fn shell_commands_can_read_the_rest_of_the_coder_config() {
        for command in [
            "cat ~/.config/coder/providers.toml",
            "cat ~/.config/coder/tools.toml",
            "ls ~/.config/coder/models/",
            "coder run claude -C /home/u/project --yes -- -p 'tidy secrets.env handling'",
            "cat secrets.env",
        ] {
            assert_eq!(bash(command), None, "{command}");
        }
    }

    #[test]
    fn coder_init_is_denied_but_other_subcommands_are_not() {
        for command in [
            "coder init",
            "./target/debug/coder init",
            "cd x && coder  init",
        ] {
            assert_eq!(bash(command), Some(INIT_REASON), "{command}");
        }
        for command in ["coder doctor --json", "git init", "echo coder initialize"] {
            assert_eq!(bash(command), None, "{command}");
        }
    }

    #[test]
    fn the_proxy_environment_cannot_be_printed() {
        for command in [
            "podman exec coder-proxy env",
            "podman inspect coder-proxy",
            "podman container inspect --format '{{.Config.Env}}' coder-proxy",
            "podman cp coder-proxy:/proc/1/environ /tmp/x",
            "cat /proc/12345/environ",
            "tr '\\0' '\\n' < /proc/$(pgrep -f proxy-serve)/environ",
        ] {
            assert_eq!(bash(command), Some(PROXY_ENV_REASON), "{command}");
        }
        for command in [
            "podman logs coder-proxy",
            "podman ps --filter label=coder.run",
            "cat /proc/cpuinfo",
        ] {
            assert_eq!(bash(command), None, "{command}");
        }
    }

    #[test]
    fn unrelated_tools_and_malformed_input_are_allowed() {
        assert_eq!(check("WebFetch", json!({ "url": "https://x" }), "/"), None);
        assert_eq!(check("Read", json!({}), "/"), None);
        assert_eq!(deny_reason(&HookInput::default(), Path::new(CONFIG)), None);
    }

    #[test]
    fn deny_output_matches_the_pre_tool_use_schema() {
        let out: Value = serde_json::from_str(&deny_output("nope")).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            out["hookSpecificOutput"]["permissionDecisionReason"],
            "nope"
        );
    }
}
