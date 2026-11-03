# Security Policy

## Threat model

`coder` runs autonomous AI coding agents (aider, opencode, Claude Code)
inside rootless podman containers. These agents operate without a
human approving every action, which makes an agent container with a
real LLM provider API key and open network access a
credential-exfiltration risk: anything the agent runs could read that
key from its own environment and send it anywhere it can reach.

**Mitigated by `coder`:** tool containers never run with
`--network host` and never hold a real provider API key. They can
reach only a dedicated proxy container, over a podman network created
with `--internal` (`coder-net`: no NAT, no default route, no external
DNS), using a short-lived session token. Only the proxy container has
real egress, through a second, proxy-only network (`coder-egress`),
and holds real provider keys (sourced from a plaintext secrets file,
passed to podman by variable name so they never appear in its argv).
The proxy's admin API listens only on the proxy container's loopback
and additionally requires a per-session shared secret, so a tool
container cannot mint or revoke tokens. A compromised or malicious
agent process cannot exfiltrate a real key it never has, and cannot
reach arbitrary network hosts.

**Not mitigated by `coder`:** filesystem isolation of the agent inside
its mounted workspace. `coder` relies on podman/rootless-container
isolation for what the agent can read or write under `/workspace`; it
does not add its own filesystem sandboxing on top. Scope mounted paths
to what the tool actually needs: `coder` will not stop an agent from
reading or modifying anything you mount into its container.

## Supported versions

Only the latest release on the default branch is supported with
security fixes. There are no older maintained release lines at this
time.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this
repository: go to the **Security** tab, then **Report a vulnerability**.
This opens a private advisory visible only to the maintainer, so
details (including a working exploit) never need to touch a public
issue.

Please do not disclose the issue publicly until it has been
addressed.
