# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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

[0.4.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.4.0
[0.3.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.3.0
[0.2.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.2.0
[0.1.0]: https://github.com/DevBytAmir/coder/releases/tag/v0.1.0
