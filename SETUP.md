# Setup

This covers installing and configuring `coder` from scratch. Linux steps
are verified end-to-end (built, run, and tested live against real
providers during development). macOS steps are reasoned from Podman's
own documentation and the code's actual platform-specific behavior; they
haven't been run on that platform by the people who wrote this, so treat
them as a solid starting point, not a guarantee. Native Windows builds
and runs in CI (see below) but hasn't been tested live against real
providers either. Please open an issue if something here is wrong.

## Why platforms differ

`coder` isolates sandboxed tool containers on a podman `--internal`
network with rootless user-namespace mapping (`--userns=keep-id`) and
relies on `podman exec` to reach the proxy's loopback-only admin API.
These are Linux container primitives. On macOS and Windows, Podman
provides them by running a real Linux VM (`podman machine`); the
containers and their networking are genuinely Linux either way, you're
just reaching them through a different host launcher.

Provider keys live in a plaintext `KEY=value` file on every platform
(see "Store your API keys" below). On Unix (Linux/macOS/WSL2), `coder`
refuses to read that file unless it's `chmod 600`. Native Windows has
no equivalent mode bit to check, so it trusts the file as-is there;
restrict it to your own user account via its file properties/ACL
instead.

## 1. Install Podman

### Linux

Use your distro's package manager.

```sh
# Arch
sudo pacman -S podman

# Debian/Ubuntu
sudo apt install podman

# Fedora
sudo dnf install podman
```

Rootless mode (what `coder` assumes) works out of the box on a normal
desktop Linux install. Confirm with:

```sh
podman info --format '{{.Host.Security.Rootless}}'
```

### macOS

```sh
brew install podman
podman machine init
podman machine start
```

This downloads and boots a small Linux VM that actually runs your
containers; the `podman` CLI on your Mac just talks to it. On Apple
Silicon, if you need x86_64 images to run too, initialize with
`podman machine init --rosetta` (Rosetta translation must be enabled at
VM creation time, not added after). If the default VM backend misbehaves,
`podman machine init --provider applehv` is worth trying.

Podman Desktop (`brew install --cask podman-desktop`) is optional, a GUI
over the same `podman machine`, not required for anything `coder` does.

### Windows (native)

Install [Podman Desktop](https://podman-desktop.io/) or the Podman CLI
directly, then:

```powershell
podman machine init
podman machine start
```

This boots the same Linux VM that Podman uses on macOS; the native
`coder.exe` (from the GitHub release, or built with `cargo build
--release`) talks to it over the `podman` CLI exactly like on Linux,
nothing about container orchestration is Windows-specific.

### Windows (via WSL2)

If you'd rather run inside a Linux environment generally:

1. Install WSL2 with a Linux distro (`wsl --install` in PowerShell, or
   follow Microsoft's WSL docs if you already have WSL and just need a
   distro).
2. Open a shell **inside** that WSL2 distro and follow the **Linux**
   steps above and below; from here on, treat WSL2 as Linux, because as
   far as `coder` and the Rust toolchain are concerned, it is.

## 2. Install Rust

Any platform:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Or your distro's packaged `rustup` (preferred on Linux if available, to
stay inside your normal package-manager workflow, e.g. `sudo pacman -S
rustup && rustup default stable` on Arch).

## 3. Install coder

### Option A: install script (fastest)

```sh
curl -fsSL https://raw.githubusercontent.com/DevBytAmir/coder/main/scripts/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/DevBytAmir/coder/main/scripts/install.ps1 | iex
```

This downloads the right binary for your OS/arch from the latest
GitHub release, verifies its checksum, installs it, and checks
whether `podman` is on your `PATH`. There's no local checkout this
way, so `coder build` needs one of the two flags in step 4 below.

### Option B: build from source

```sh
git clone https://github.com/DevBytAmir/coder.git
cd coder
cargo build --release
ln -s "$(pwd)/target/release/coder" ~/.local/bin/coder
```

## 4. Build the container images

```sh
coder build
```

**Must run from the repo root** if you built from source, since
`coder build` uses paths relative to it by default. If you used the
install script instead and have no local checkout, either point it at
a checkout you have elsewhere:

```sh
coder build --source-dir /path/to/coder
```

Or have it download the matching tagged source itself:

```sh
coder build --fetch                      # into a temp directory, deleted after
coder build --fetch --keep ~/.cache/coder-src   # kept there, reused on a later build
```

## 5. Configure providers

`coder` generates default config on first use. Check
`~/.config/coder/providers.toml` and `~/.config/coder/tools.toml` after
running any subcommand that reads config; edit `providers.toml` to add
or change providers (upstream URL, wire format, default model). See the
file's own comments for the field meanings.

## 6. Store your API keys

Provider keys go in a plaintext `KEY=value` file, named by each
provider's uppercased name plus `_API_KEY`:

```sh
cat > ~/.config/coder/secrets.env <<'EOF'
ANTHROPIC_API_KEY=
NIM_API_KEY=
OPENROUTER_API_KEY=
EOF
chmod 600 ~/.config/coder/secrets.env
# edit the file yourself and fill in real values
```

`coder proxy up`/`coder models refresh` default to
`~/.config/coder/secrets.env`; pass `--secrets-file <path>` to use a
different one.

## 7. Start the proxy and run a tool

```sh
coder proxy up

coder run aider -C ~/path/to/project --provider anthropic
coder run opencode -C ~/path/to/project --provider nim
coder run claude -C ~/path/to/project --provider anthropic
```

`claude` only works with providers whose `wire_format` is
`anthropic_messages` (just `anthropic` by default; `coder` refuses the
pairing otherwise, rather than silently sending a malformed request).
`aider`/`opencode` work with any `openai_chat_completions` provider
(`nim`, `openrouter`, `ollama`, and anything else you add that speaks
that format).

```sh
coder proxy down   # when you're done
```

## 8. Optional: populate opencode's model picker

```sh
coder models refresh <provider> [--secrets-file P]
coder models refresh --all [--secrets-file P]   # every configured provider at once
coder build          # bake the regenerated list into the image
```

Run `coder models refresh` for each provider you use, or `--all` to do
every configured provider in one pass; it merges into one
`opencode.jsonc`, with each model labeled by which provider(s) it
came from. **Order matters**: `coder build` bakes in whatever's
currently in `containers/sandbox/opencode.jsonc` on disk, so always
refresh *before* building if you want the full catalog; running `coder
build` on its own resets it back to the small shipped default.

For `aider`/`claude`, which take `--model` directly rather than having an
in-app picker:

```sh
coder models list <provider>   # prints ids to copy into --model
```
