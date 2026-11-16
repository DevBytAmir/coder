<div align="center">

# coder

[![CI](https://github.com/DevBytAmir/coder/actions/workflows/ci.yml/badge.svg)](https://github.com/DevBytAmir/coder/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-stable-2b2b2b?style=flat-square)](https://www.rust-lang.org/)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-2b2b2b?style=flat-square)](./LICENSE)
[![Version](https://img.shields.io/badge/version-0.5.0-2b2b2b?style=flat-square)](./CHANGELOG.md)

</div>

`coder` is a Rust CLI that launches sandboxed AI coding agents (aider,
opencode, Claude Code) inside rootless podman containers, against any
configured LLM provider (Anthropic, NVIDIA NIM, Ollama, OpenRouter, and
future providers), without ever exposing real provider API keys to the
agent process itself.

## Why

These agents run in **autonomous ("auto") mode**: they read/write files
and make tool calls without a human approving every step. Giving an
autonomous agent container unrestricted outbound network access and a
real provider API key is a credential-exfiltration risk: anything the
agent runs can read the key out of its own environment and ship it
anywhere. `coder` closes that hole by splitting the agent container
from the only container that holds real keys and has real internet
access.

## Architecture

```
                    podman --internal network "coder-net" (no egress)
                    ┌─────────────────────────────────────────────────┐
┌───────────────┐   │                                                 │
│ host          │──►│ admin (podman exec)                             │
│ (coder CLI)   │──►│ launches tool, passes session token + proxy URL │
└───────────────┘   │    ┌─────────────────────────────┐              │ coder-egress
                    │    │ proxy container             │─────────────►│ (proxy-only route out)
                    │    │ (coder proxy-serve)         │              │ Anthropic, NIM, Ollama, ...
                    │    └─────────────────────────────┘              │
                    │                   ▲                             │
                    │                   │ session token               │
                    │                   │ only                        │
                    │                   │                             │
                    │    ┌─────────────────────────────┐              │
                    │    │ tool container              │              │
                    │    │ (aider/opencode/            │              │
                    │    │  claude)                    │              │
                    │    └─────────────────────────────┘              │
                    │                                                 │
                    └─────────────────────────────────────────────────┘
```

Tool containers are never on `--network host` and never hold a real
provider key. They join only `coder-net`, created with `--internal`:
no NAT, no default route, no external DNS. The only thing they can
reach is the proxy's public port (8080), using a short-lived session
token. The proxy container is additionally attached to a second,
proxy-only bridge network (`coder-egress`), which is its sole route to
upstream providers (and to the host, e.g. Ollama via
`host.containers.internal`). Only the proxy has real egress and real
provider keys.

The proxy's admin API (token mint/revoke/status) listens on the proxy
container's own loopback (`127.0.0.1:8787`), so nothing on `coder-net`
can reach it, and every call must also carry a per-session shared
secret (`CODER_ADMIN_TOKEN`) that exists only in the proxy container's
environment. The host CLI drives it with `podman exec coder-proxy
coder proxy-admin ...`. Real keys and the admin secret are passed to
podman by variable name only, never on its command line.

## Threat model

**What `coder` mitigates:** credential exfiltration by an autonomous
agent. The agent container has no real API key and no route to the
open internet: only a session token scoped to the proxy, over an
`--internal` podman network with no egress. Even a fully compromised
or malicious agent process inside that container cannot read or
exfiltrate a real provider key, because it never has one, and cannot
reach arbitrary hosts, because its only network is `--internal` and
`--network host` is never used for tool containers.

**What `coder` does not mitigate:** anything inside the sandboxed
filesystem the agent is given. `coder` does not sandbox the agent's
file access within `/workspace` beyond what podman/rootless
containers already provide; that isolation is podman's job, not
`coder`'s. If you mount sensitive host paths into a tool container,
or run a tool with more filesystem access than it needs, `coder`
won't stop the agent from reading or modifying that data. Scope what
you mount accordingly.

**Who is trusted:** whoever runs the `coder` CLI on the host, whether
that's you or a script or agent (such as a Claude Code session) driving
it. The host side can read the secrets file and run podman directly, so
`coder` protects keys from the agent *inside* the sandbox, not from its
caller. See [Scripts and agents](#scripts-and-agents).

See [SECURITY.md](SECURITY.md) for the formal statement.

## Usage

```
coder build                          # build/rebuild coder-sandbox and coder-proxy images
coder proxy up [--secrets-file P]    # start the proxy container (defaults to ~/.config/coder/secrets.env)
coder proxy down                     # stop it
coder proxy status                   # is it running, which providers have keys loaded
coder run <tool> [-C dir] [--provider X] [--model Y] [--yes] [--timeout D] [--dry-run] [-- <tool-args...>]
coder models refresh <provider>|--all [--secrets-file P]  # fetch + filter a provider's models
coder models list <provider>         # print a provider's refreshed model ids
coder config check                   # validate providers.toml/tools.toml, no live key needed
coder doctor [--json]                # check podman, images, file locations, proxy, secrets file, PATH, provider reachability
coder completions <shell>            # print a completion script (bash/zsh/fish/...)
coder init                           # interactive first-run setup wizard
coder hook pre-tool-use              # hook for coding agents (see "Agent plugin" below)
```

`coder build` builds from the coder checkout you run it in. Anywhere
else, for example with a standalone binary from the install script, it
downloads this version's source into `~/.local/share/coder/source/` the
first time and reuses it after; `--fetch` downloads it again, and
`--source-dir <path>` builds from a checkout elsewhere.

### Where coder keeps things

| Path | What | Who writes it |
|---|---|---|
| `~/.config/coder/providers.toml`, `tools.toml` | providers and tools | created with defaults on first use; you edit them |
| `~/.config/coder/secrets.env` | provider API keys (`chmod 600`) | you, or `coder init` |
| `~/.config/coder/models/<provider>.json` | each provider's refreshed model list | `coder models refresh` |
| `~/.config/coder/opencode.jsonc` | opencode's model picker, built from those lists | `coder models refresh`; `coder build` bakes it into the sandbox image |
| `~/.local/share/coder/source/<version>/` | container files and source the images are built from | `coder build`, outside a checkout |

`coder doctor` prints the ones in use. Everything under
`~/.local/share/coder` can be deleted; `coder build` downloads it again.

The proxy reads the same `~/.config/coder/providers.toml` as the host
CLI: `coder proxy up` bind-mounts it read-only into the container, so
edits take effect on the next `coder proxy down && coder proxy up`.

`coder run` resolves the project directory (git-root detection), runs
a sandbox-guard confirmation before touching podman (refuses `$HOME`,
`/`, etc.), auto-starts the proxy if it isn't already running, mints
a session token, and launches the requested tool container against
the proxy.

## Scripts and agents

`coder run` normally asks `Proceed? [y/N]` before sandboxing a
directory. To drive it from a script, CI job, or another agent:

```sh
coder doctor --json                       # preflight: {"ok": bool, "checks": [...]}
coder run claude --provider anthropic -C /abs/path/to/project --yes --timeout 30m \
  -- -p "make the failing tests pass" </dev/null
```

- `--yes` / `-y` skips the prompt. It requires an explicit `-C`, and it
  never overrides the refusal of `$HOME`, `/`, and other dangerous
  directories.
- Without a terminal on stdin and without `--yes`, `coder run` fails
  with exit code 64 instead of prompting.
- Arguments after `--` go to the tool unchanged, so use the tool's own
  one-shot mode: `claude -p "..."`, `aider --message "..."
  --no-auto-commits`, `opencode run --auto "..."`. opencode needs
  `--auto` because the sandbox config makes edits ask first, and a
  one-shot run has no one to ask; its deny rules still apply. Redirect
  stdin from `/dev/null` so a tool that reads piped input can't block
  on it.
- `--timeout <D>` (`90`, `90s`, `30m`, `2h`) stops the tool container
  once it elapses, giving it 10 seconds to exit cleanly. Podman's own
  `--timeout` is also set 30 seconds later, so the container is killed
  even if `coder` itself dies first.
- SIGINT, SIGTERM, or SIGHUP to `coder` revokes the session token and
  stops the container.
- Each run's container is named `coder-run-<tool>-<id>` and labelled
  `coder.run`: `podman ps --filter label=coder.run` lists active runs.
- Optional per-tool resource limits in `tools.toml`, checked by
  `coder config check`: `memory = "4g"`, `cpus = 2`.

Exit codes:

| Code | Meaning |
|---|---|
| tool's own | `coder run` passes the tool's exit code through (128 + N if a signal N killed it) |
| 1 | any other `coder` error |
| 2 | invalid command-line arguments |
| 64 | confirmation required: no terminal, and `--yes` not given |
| 77 | sandbox guard refused the directory, or the prompt was declined |
| 124 | `--timeout` elapsed; the container was stopped |
| 125 | podman could not create or start the container (e.g. the image is missing) |
| 130 | interrupted (SIGINT, SIGTERM, or SIGHUP) |

Keep provider-key setup (`coder init`, editing the secrets file) with
a human: an orchestrating agent never needs to see real keys.

### Agent plugin

The `coder` plugin teaches a coding agent on the host to delegate tasks
through `coder` as described above. It works in Claude Code and Codex,
and adds:

- the `run-in-sandbox` skill: preflight with `coder doctor --json`, the
  right one-shot flags for each tool, what each exit code means, and
  reviewing the result with git
- a `PreToolUse` hook that keeps real provider keys out of the agent's
  context (below)

Install it in **Claude Code**:

```
/plugin marketplace add DevBytAmir/coder
/plugin install coder@coder
```

From a shell: `claude plugin marketplace add DevBytAmir/coder`, then
`claude plugin install coder@coder`.

Install it in **Codex**:

```sh
codex plugin marketplace add DevBytAmir/coder
codex plugin add coder@coder
```

Write `coder@coder` in full: Codex's built-in marketplace has an
unrelated plugin also named `coder`. Codex doesn't run a plugin's hooks
until you approve them, and asks you to review them the next time you
start it interactively.

#### Permissions in Claude Code

Claude Code asks once before it uses the skill, because the skill
grants tool permissions. After you approve, its `allowed-tools` lets
the session run `coder doctor`, `coder run`, and `podman ps` without
further prompts. Combined with `--yes`, that means the session can
sandbox any directory the guard doesn't refuse without asking you.

- To skip the skill prompt too, for example in headless `claude -p`
  runs, add `Skill(coder:run-in-sandbox)` to `permissions.allow` in
  your Claude Code settings.
- To approve every run yourself instead, add `Bash(coder run *)` to
  `permissions.ask`. Ask and deny rules override `allowed-tools`.

#### The hook

The plugin's hook runs `coder hook pre-tool-use` before file, search,
shell, and `apply_patch` calls, and denies:

- reading, editing, or searching `~/.config/coder/secrets.env` or the
  rest of the coder config directory, apart from `providers.toml`,
  `tools.toml`, `models/`, and `opencode.jsonc`
- `coder init`
- `podman exec`, `inspect`, or `cp` on `coder-proxy`, and reading
  `/proc/<pid>/environ`, which would print the proxy's environment

The agent sees the reason and is told to ask you instead. The hook runs
`coder hook pre-tool-use`, so `coder` must be on `PATH`; if it isn't, the
agent reports a hook error and carries on. It is a guardrail against
accidents, not a security boundary:
it matches known paths and commands, and it protects only the default
secrets file, not one passed with `--secrets-file`. See the trust
boundary in [SECURITY.md](SECURITY.md).

## Setup

See [SETUP.md](SETUP.md) for full installation and configuration steps
(Linux, macOS, native Windows, and Windows via WSL2), including
provider keys and the model-picker refresh workflow.

Quick version, once Podman is installed:

```sh
curl -fsSL https://raw.githubusercontent.com/DevBytAmir/coder/main/scripts/install.sh | sh

cat > ~/.config/coder/secrets.env <<'EOF'
ANTHROPIC_API_KEY=sk-...
EOF
chmod 600 ~/.config/coder/secrets.env

coder build
coder proxy up
coder run aider -C ~/path/to/project
```

Or run `coder init` for an interactive wizard that sets up config and
secrets (still requires Podman and an installed `coder` binary first).

## Reporting bugs

Open an issue with the bug-report form. It asks for `coder --version`,
your OS, `podman --version`, and `coder doctor` output, which together
answer most first questions. Never paste API keys or your secrets file:
nothing in the form needs them. Report security problems privately
instead, as described in [SECURITY.md](SECURITY.md).

## Contributing

To work on `coder` itself, see [CONTRIBUTING.md](CONTRIBUTING.md): the
checks CI runs, the commit convention, and how releases are made.

## License

Apache-2.0, see [LICENSE](LICENSE).
