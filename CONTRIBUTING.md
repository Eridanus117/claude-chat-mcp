# Contributing

This project is intentionally small. Keep changes focused on the MCP server, Claude Code CLI process handling, job persistence, and documentation.

Before submitting a change:

```sh
cargo fmt --check
cargo test
```

Avoid adding provider-specific launchers, private endpoints, or host-local workflow scripts. Those belong in separate wrappers that invoke `claude-chat-mcp` with environment variables.
