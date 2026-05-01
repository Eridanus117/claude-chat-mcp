# claude-chat-mcp

`claude-chat-mcp` is a small stdio MCP server that runs Claude Code CLI turns as observable background jobs.

It is useful when an MCP host has a short tool-call timeout but a Claude Code task may take minutes. Instead of blocking on one synchronous `claude --print` call, the host can start a job, check status, read a partial text tail, stream raw events, and fetch the final result later.

## Tools

- `claude-start`: start a background Claude job and return a `jobId`
- `claude-status`: read current job state
- `claude-tail`: read the current assistant text tail and progress metadata
- `claude-events`: read raw `stream-json` events by cursor
- `claude-result`: return the final result, or a running status if unfinished
- `claude-cancel`: terminate a running job

## Requirements

- Rust 1.75 or newer
- Claude Code CLI available as `claude`
- An MCP host that can launch stdio servers

`claude-chat-mcp` does not manage authentication. Configure Claude Code CLI normally before using this server.

## Install

```sh
cargo install --path .
```

For local development:

```sh
cargo build
cargo test
```

## MCP Configuration

Example MCP server entry:

```json
{
  "mcpServers": {
    "claude-chat": {
      "command": "/path/to/claude-chat-mcp"
    }
  }
}
```

If your Claude binary is not on `PATH`, set `CLAUDE_CHAT_MCP_CLAUDE_BIN`:

```json
{
  "mcpServers": {
    "claude-chat": {
      "command": "/path/to/claude-chat-mcp",
      "env": {
        "CLAUDE_CHAT_MCP_CLAUDE_BIN": "/absolute/path/to/claude"
      }
    }
  }
}
```

## Job Files

Jobs are persisted on disk so the MCP host can poll cheaply.

Default location:

- `$CLAUDE_CHAT_MCP_JOBS_DIR`, when set
- `$XDG_STATE_HOME/claude-chat-mcp/jobs`, when `XDG_STATE_HOME` is set
- `~/.local/state/claude-chat-mcp/jobs`
- `.claude-chat-mcp/jobs`, if no home directory is available

Each job directory may contain:

- `spec.json`: request parameters
- `state.json`: current state
- `progress.json`: streaming progress
- `events.jsonl`: raw Claude Code `stream-json` events
- `partial.txt`: aggregated assistant text during streaming
- `stderr.log`: child process stderr
- `stdout.json`: non-streaming JSON output
- `result.json`: final normalized result

## Example Flow

Start a streaming job:

```json
{
  "prompt": "Review the current repository and list the highest-risk bugs.",
  "cwd": "/path/to/repo",
  "stream": true
}
```

Then poll:

```json
{ "jobId": "claude-..." }
```

Use `claude-tail` for compact progress and `claude-events` when you need raw event details.

## Notes

- Do not run two resumed Claude turns concurrently against the same Claude session id. The server does not currently lock native Claude session ids.
- `allowed-tools` is forwarded to Claude Code after the prompt because Claude Code treats the flag as variadic.
- The server intentionally exposes only tools. It does not provide MCP resources or prompts.

## License

Apache-2.0. See [LICENSE](./LICENSE).
