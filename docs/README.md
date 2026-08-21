# sats documentation

These documents describe the behavior currently implemented in the
repository. Product planning, target dates, and speculative work live outside
the public code documentation.

## Use sats

- [CLI reference](cli.md): commands, flags, configuration, and JSON output.
- [MCP and agent grants](mcp.md): connect an agent and understand each tool.
- [Providers and guards](providers.md): configure chain access and optional
  asset protection.
- [Security and trust model](security.md): keys, signing, grants, failure
  behavior, and operational guidance.

## Understand and change sats

- [Architecture](architecture.md): crate boundaries, module ownership, and
  transaction flows.
- [Development](development.md): build, test, lint, and documentation policy.
- [Releasing](releasing.md): publish and verify native release archives.
- [AGENTS.md](../AGENTS.md): canonical implementation rules for coding agents
  and contributors.
- [CONTRIBUTING.md](../CONTRIBUTING.md): contribution workflow.

The CLI's `--help`, MCP schemas, tests, and typed Rust contracts are the
authoritative behavior. When prose disagrees with them, fix the disagreement
in the same change.
