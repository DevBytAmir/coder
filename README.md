<div align="center">

# coder

[![CI](https://github.com/DevBytAmir/coder/actions/workflows/ci.yml/badge.svg)](https://github.com/DevBytAmir/coder/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-stable-2b2b2b?style=flat-square)](https://www.rust-lang.org/)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-2b2b2b?style=flat-square)](./LICENSE)
[![Version](https://img.shields.io/badge/version-0.4.0-2b2b2b?style=flat-square)](./CHANGELOG.md)

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

See [SECURITY.md](SECURITY.md) for the formal statement.

## Usage

```
coder build                          # build/rebuild coder-sandbox and coder-proxy images
coder proxy up [--secrets-file P]    # start the proxy container (defaults to ~/.config/coder/secrets.env)
coder proxy down                     # stop it
coder proxy status                   # is it running, which providers have keys loaded
coder run <tool> [-C dir] [--provider X] [--model Y] [--dry-run] [-- <tool-args...>]
coder models refresh <provider>|--all [--secrets-file P]  # fetch + filter a provider's models
coder models list <provider>         # print a provider's refreshed model ids
coder config check                   # validate providers.toml/tools.toml, no live key needed
coder doctor                         # check podman, images, proxy, secrets file, PATH are all ready
coder completions <shell>            # print a completion script (bash/zsh/fish/...)
coder init                           # interactive first-run setup wizard
```

`coder build` uses paths relative to the repository root by default
(`containers/sandbox`, `containers/proxy/Containerfile`, and `.` as the
proxy build context); pass `--fetch` or `--source-dir <path>` instead
if you installed a standalone binary with no local checkout.

The proxy reads the same `~/.config/coder/providers.toml` as the host
CLI: `coder proxy up` bind-mounts it read-only into the container, so
edits take effect on the next `coder proxy down && coder proxy up`.

`coder run` resolves the project directory (git-root detection), runs
a sandbox-guard confirmation before touching podman (refuses `$HOME`,
`/`, etc.), auto-starts the proxy if it isn't already running, mints
a session token, and launches the requested tool container against
the proxy.

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

coder build --fetch
coder proxy up
coder run aider -C ~/path/to/project
```

Or run `coder init` for an interactive wizard that sets up config and
secrets (still requires Podman and an installed `coder` binary first).

## License

Apache-2.0, see [LICENSE](LICENSE).
