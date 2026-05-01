# Security

`claude-chat-mcp` launches the local Claude Code CLI and persists job artifacts on disk.

## Sensitive Data

Prompts, partial assistant output, raw stream events, stderr, and final results may contain sensitive information. Configure `CLAUDE_CHAT_MCP_JOBS_DIR` to a private directory and clean old jobs according to your local retention policy.

The server does not read, store, or manage Claude credentials directly. Authentication is handled by the configured Claude Code CLI.

## Reporting Issues

If this repository is published, use the repository's private vulnerability reporting channel when available. Otherwise, open a minimal issue that avoids secrets, prompts, tokens, or private output.
