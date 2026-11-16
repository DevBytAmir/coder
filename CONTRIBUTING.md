# Contributing

Bug reports and pull requests are welcome. For bugs, use the bug-report
form on GitHub. Report security problems privately, as described in
[SECURITY.md](SECURITY.md), not in a public issue or pull request.

## Building

You need Rust (stable) and, for anything that runs containers, rootless
podman. See "Option B: build from source" in [SETUP.md](SETUP.md).

```sh
cargo build
cargo run -- build   # build the sandbox and proxy images (run from the repository root)
```

## Checks

CI runs these on every push and pull request; run them before you open
one:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings -D clippy::all -D clippy::pedantic
cargo test --locked
scripts/bump-version.sh --check
```

A few tests need podman and the `coder-sandbox` image, so `cargo test`
skips them. Run them with:

```sh
cargo test -- --ignored
```

If you change the plugin under `plugins/coder/`, also run `claude plugin
validate --strict plugins/coder`, and try it in a session with `claude
--plugin-dir plugins/coder`.

## Commits

- [Conventional Commits](https://www.conventionalcommits.org/): `type(scope):
  summary`, lowercase, imperative, no trailing period, for example
  `fix(proxy): make proxy up and down idempotent`. Common types are
  `feat`, `fix`, `docs`, `refactor`, and `chore`.
- One change per commit, with its documentation in the same commit: its
  entry under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md), following
  [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and any README,
  SETUP, or SECURITY updates it needs.
- Each commit should build and pass the checks above on its own.

## Releasing

1. `scripts/bump-version.sh X.Y.Z` sets the version in `Cargo.toml`,
   `Cargo.lock`, the plugin manifests, and the README badge.
2. Move the `[Unreleased]` entries in `CHANGELOG.md` into a new `[X.Y.Z]`
   section with today's date, and update the comparison links at the
   bottom.
3. Commit, then tag and push: `git tag vX.Y.Z && git push origin main vX.Y.Z`.
   The release workflow builds the binaries, publishes them with
   `SHA256SUMS.txt`, and the install scripts pick up the new release.
