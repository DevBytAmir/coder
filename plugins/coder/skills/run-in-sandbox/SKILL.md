---
name: run-in-sandbox
description: Use when a coding task on a local repository should be handed to an autonomous agent (`claude`, `aider`, or `opencode`) running in the coder sandbox, when the user says to "use coder" or "run it sandboxed", or wants an agent working on a repo without access to real provider API keys.
allowed-tools: Bash(coder doctor *) Bash(coder run *) Bash(podman ps *)
---

# Run a task in the coder sandbox

`coder` runs an agent in a rootless podman container that has no real API key and
no network route except a local credential proxy. You stay on the host: start the
run, wait for it, and review what it changed.

## Rules

- Never handle provider keys. Don't run `coder init`, don't read or edit
  `~/.config/coder/secrets.env`, and don't put keys in commands. If a key is
  missing, stop and tell the user which provider needs one.
- Pass `--yes` only together with an absolute `-C` path to the project you were
  asked to work on.
- Always pass `--timeout`, and redirect stdin from `/dev/null` so the tool can't
  block waiting for input.
- Run each `coder` command as its own shell command, with nothing chained before
  or after it, so its exit code is the one you see.
- The sandboxed agent's work is untrusted. Review it before you run, commit, or
  report it.

## Steps

1. **Preflight:** run `coder doctor --json`. Every check with
   `"status": "problem"` must be resolved first; `"info"` is fine.
   - `coder: command not found`: `coder` isn't installed. See
     [Installing coder](#installing-coder).
   - `--json` rejected as an unknown argument: this `coder` is too old for this
     skill. Tell the user to upgrade it.
   - `image ... not built`: run `coder build`. It takes several minutes; say so.
   - A `key for <provider>` check with status `info` means that provider has no
     key. Pick a provider whose key is `ok` (ollama needs none), or ask the user.
   - A `provider <name>` check saying `unreachable` means runs with that provider
     will fail. Pick another provider, or tell the user what the check says.
2. **Run** the tool in its one-shot mode; arguments after `--` go to it unchanged.
   Add `--dry-run` first if you want to see what will be mounted without starting
   anything.

   ```sh
   coder run claude --provider anthropic -C /abs/path/to/project --yes --timeout 30m \
     -- -p "Make the failing tests in tests/ pass without editing the tests." </dev/null
   ```

   | Tool | Providers | One-shot args |
   |---|---|---|
   | `claude` | Anthropic-format (`anthropic`) | `-- -p "<task>"` |
   | `aider` | OpenAI-format (e.g. `nim`, `ollama`, `openrouter`) | `-- --message "<task>" --no-auto-commits` |
   | `opencode` | OpenAI-format | `-- run --auto "<task>"` |

   `--provider` is required when more than one provider is configured.
3. **Check the exit code:**

   | Code | Meaning |
   |---|---|
   | 0 | the tool finished |
   | 1 | `coder` error: read the `Error:` line on stderr |
   | 64 | you left out `--yes -C <dir>` |
   | 77 | `coder` refused that directory: report it, don't try another path |
   | 124 | timed out; the container was stopped, partial edits remain |
   | 125 | podman could not start the container (e.g. image not built) |
   | 130 | interrupted |
   | anything else | the tool's own exit code |

4. **Review:** `git -C <dir> status`, `git -C <dir> diff`, then run the project's
   tests. Don't commit unless the user asked you to.

## Installing coder

Only when `coder` is missing, and only once the user agrees, since this
installs software on their machine.

1. Install the prebuilt binary with the install script. Don't clone the
   repository, build it with `cargo`, or install Rust: the script
   downloads a checksum-verified release and needs none of that.

   ```sh
   curl -fsSL https://raw.githubusercontent.com/DevBytAmir/coder/main/scripts/install.sh | sh
   ```

   On Windows: `irm https://raw.githubusercontent.com/DevBytAmir/coder/main/scripts/install.ps1 | iex`.
   It installs to `~/.local/bin`, or `%LOCALAPPDATA%\coder\bin` on Windows. If
   it warns that this isn't on `PATH`, tell the user to add it: this skill
   and its hook need `coder` on `PATH`.
2. If the script reports that podman isn't found, ask the user to
   install it. That needs their package manager and `sudo`, so don't do
   it yourself.
3. Build the container images with `coder build --fetch`. It takes
   several minutes; say so.
4. Ask the user to set up their provider keys themselves, with `coder
   init` or by editing the secrets file. Never run `coder init` or touch
   the secrets file yourself.
5. Run the preflight again.

## Common mistakes

- Running two agents on the same directory at once. Give each its own git
  worktree; `podman ps --filter label=coder.run` lists active runs.
- Retrying a timed-out run with the same task and timeout. Narrow the task, or
  raise `--timeout`.
- A vague task. Brief it like a contractor: the goal, the constraints, and what
  "done" means.
