# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `coder --version` / `-V`.
- `coder run --yes` / `-y`: sandbox the project directory without the
  confirmation prompt, for scripts and agents. Requires an explicit
  `-C`, and never overrides the refusal of `$HOME`, `/`, and other
  dangerous directories.
- A "Scripts and agents" section in the README, with the exit codes
  `coder` uses.
- Tool containers are labelled `coder.run`, so `podman ps --filter
  label=coder.run` lists active runs.
- `coder run --timeout <duration>` (`90`, `90s`, `30m`, `2h`): stop the
  tool container and exit with status 124 once the duration elapses.
  Podman's own `--timeout` is set 30 seconds later as a backstop, so
  the container is killed even if `coder` itself dies.
- Optional per-tool `memory` and `cpus` limits in `tools.toml`, passed
  to podman as `--memory`/`--cpus` and validated when the file loads.
- `coder doctor --json`: machine-readable results on stdout.
- `coder doctor` checks that each provider's upstream answers from the
  proxy's network, using a throwaway container and no credentials. An
  unreachable provider is a problem if it has a key loaded, and
  information otherwise.
- `coder hook pre-tool-use`: a `PreToolUse` hook for coding agents. It
  denies tool calls that would read the secrets file (file reads and
  edits, searches, shell commands, and `apply_patch` patches), run
  `coder init`, or print the proxy's environment (`podman exec`,
  `inspect`, or `cp` on `coder-proxy`, or reading `/proc/<pid>/environ`).
  Input it can't parse is let through.
- A `coder` plugin for Claude Code and Codex, with a marketplace for
  each (`.claude-plugin/` and `.agents/plugins/`). It adds the
  `run-in-sandbox` skill, which teaches the agent to hand tasks to
  `coder run`, and the hook above, which keeps provider keys out of the
  agent's context.
- `scripts/bump-version.sh`: sets the version in `Cargo.toml`,
  `Cargo.lock`, the plugin manifests, and the README badge. CI runs it
  with `--check` to catch versions that disagree.
- The `run-in-sandbox` skill covers installing `coder` when it's
  missing: with the install script rather than a source build, and with
  Podman and API keys left to the user.
- A GitHub bug-report form asking for the version, OS, podman version,
  and `coder doctor` output, with security reports pointed to private
  vulnerability reporting instead.
- `CONTRIBUTING.md`: the checks CI runs, the commit convention, and the
  release steps.

### Changed

- `coder run` without a terminal on stdin and without `--yes` now fails
  with exit code 64 instead of reading the prompt answer from stdin.
  Piping an answer in (`echo y | coder run ...`) no longer works; use
  `--yes -C <dir>`.
- A refused or declined sandbox-guard check now exits with status 77
  instead of 1.
- The sandbox-guard prompt is written to stderr, so it stays visible
  when stdout is piped.
- Tool containers are named `coder-run-<tool>-<id>` instead of getting
  a random podman name, so parallel runs can each be addressed.
- SIGINT during `coder run` now also stops the tool container, not
  just revokes its session token.
- When a signal kills the tool container's podman process, `coder run`
  exits with 128 + the signal number instead of 1.
- When podman is missing, `scripts/install.sh` prints the install
  command for the detected Linux distro, read from `/etc/os-release`,
  instead of a list for three distro families.

### Fixed

- Secrets-file entries with a blank value (e.g. `ANTHROPIC_API_KEY=` or
  `ANTHROPIC_API_KEY=""`) are now ignored instead of being loaded as an
  empty key, so `coder doctor` no longer reports an unset provider key
  as loaded.
- `scripts/install.sh` finds the latest release through the GitHub web
  redirect instead of the API, which allows only 60 anonymous requests
  an hour per IP.
- A missing secrets file is reported with its path.
- `coder models refresh` works for providers without a key (Ollama),
  and for upstreams at `host.containers.internal`, which only resolves
  inside containers.
- `coder models refresh` for Anthropic-format providers requests
  `/v1/models` with an `anthropic-version` header, instead of `/models`.
- `coder proxy up` while the proxy is running, and `coder proxy down`
  while it is stopped, now say so and succeed instead of failing with a
  podman error.
- Piping output into a command that exits early (`coder models list x |
  head`) no longer panics with "Broken pipe".
- SIGTERM and SIGHUP during `coder run` now revoke the session token and
  stop the tool container. Before, only SIGINT was handled, so a
  `coder run` killed by its caller left the container running and its
  token valid until the proxy's idle timeout.
- `SETUP.md` no longer lists installing Rust as a required step: the
  install script needs no Rust, and is now the recommended way. Rust is
  only needed to build from source. A note tells AI agents setting
  `coder` up to use the script and leave API keys to the user.

## [0.4.0] - 2026-11-02

### Added

- `README.md`, `SETUP.md`, and `SECURITY.md` covering architecture,
  threat model, and full install steps for Linux, macOS, native
  Windows, and Windows via WSL2.
- This changelog.

## [0.3.0] - 2026-11-02

### Added

- `coder build --fetch`: downloads the matching tagged source from
  GitHub and builds from it, for a standalone release binary with no
  local checkout. `--keep <dir>` persists and reuses it across builds
  instead of a one-off temp directory.
- `coder build --source-dir <path>`: build from an existing checkout
  at an arbitrary path instead of the current directory.
- `coder doctor` now checks that `containers/sandbox`/`containers/proxy`
  exist locally, and reports which configured providers have a key
  loaded from the secrets file.
- `scripts/install.sh`/`scripts/install.ps1`: detect OS/arch, download
  the matching release binary and its `SHA256SUMS.txt`, verify the
  checksum, install it, and check for `podman`.
- Release workflow now publishes `SHA256SUMS.txt` alongside every
  binary.

## [0.2.0] - 2026-11-02

### Added

- CI workflow: `cargo test` on Linux/macOS/Windows, `cargo fmt`/`cargo
  clippy --all-targets --all-features -- -D warnings -D clippy::all -D
  clippy::pedantic`, and `cargo audit`, all with dependency caching and
  `--locked`.
- Release workflow: builds and publishes `coder` binaries for
  linux-x86_64, linux-arm64, macos-x86_64, macos-arm64,
  windows-x86_64, and windows-arm64 on every `v*` tag.

## [0.1.0] - 2026-11-02

### Added

- `coder build`, `coder proxy up/down/status`, `coder run`, `coder
  models refresh/list`, `coder config check`, `coder doctor`, `coder
  init`, and `coder completions`.
- Credential-isolating proxy: tool containers never hold a real
  provider API key, reaching providers only through a session token
  scoped to a proxy container on an `--internal` podman network with
  no egress. The proxy alone holds real keys and real network access.
- Provider wire-format translation between OpenAI-style chat
  completions and Anthropic messages, so aider/opencode and Claude
  Code can share one proxy and one set of provider keys.
- Plaintext secrets-file provider key storage (`chmod 600` enforced on
  Unix), with the sandbox-guard confirmation prompt refusing to run
  against `$HOME`, `/`, and other dangerous directories.
- Container hardening on tool containers: `--cap-drop=ALL`,
  `--security-opt no-new-privileges`, `--pids-limit=256`.
- Verified live end to end against real NVIDIA NIM, OpenRouter, and
  Ollama providers.

[Unreleased]: https://github.com/DevBytAmir/coder/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.4.0
[0.3.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.3.0
[0.2.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.2.0
[0.1.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.1.0
