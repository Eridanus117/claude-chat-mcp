//! Backend that starts observable `claude --print` jobs and persists
//! status, stream events, partial text, stderr, and final results.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Command;

const DEFAULT_CLAUDE_BIN: &str = "claude";
const STDERR_TAIL_BYTES: usize = 8192;
const DEFAULT_TAIL_CHARS: usize = 4000;
const MAX_EVENTS_LIMIT: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeRunArgs {
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<String>,
    /// `Some` to resume an existing Claude session; `None` to start fresh.
    pub resume_session_id: Option<String>,
    /// When true, async jobs run Claude in stream-json mode and persist
    /// intermediate events for observability.
    #[serde(default)]
    pub stream: bool,
}

/// Subset of fields we care about from `claude --print --output-format json`.
#[derive(Debug, Deserialize)]
struct ClaudePrintResponse {
    #[serde(default)]
    result: Option<String>,
    session_id: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeJobStarted {
    pub job_id: String,
    pub status: String,
    pub job_dir: PathBuf,
    pub pid: Option<u32>,
    pub created_at_ms: u64,
    pub claude_bin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeJobState {
    pub job_id: String,
    pub status: String,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub child_pid: Option<u32>,
    pub pid: Option<u32>,
    pub claude_bin: Option<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    #[serde(flatten)]
    #[serde(default)]
    pub progress: ClaudeJobProgress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeJobResult {
    pub job_id: String,
    pub status: String,
    pub thread_id: Option<String>,
    pub content: String,
    pub is_error: bool,
    pub claude_bin: Option<String>,
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeJobProgress {
    pub stream: bool,
    pub event_count: usize,
    pub last_event_at_ms: Option<u64>,
    pub partial_bytes: usize,
    pub last_text_tail: String,
    pub seen_tools: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClaudeJobSpec {
    job_id: String,
    args: ClaudeRunArgs,
    created_at_ms: u64,
}

pub(crate) async fn start_claude_job(args: ClaudeRunArgs) -> anyhow::Result<ClaudeJobStarted> {
    let claude_bin = resolve_claude_bin()?;
    let created_at_ms = now_ms();
    let job_id = format!("claude-{}-{}", created_at_ms, std::process::id());
    let job_dir = job_dir(&job_id);
    fs::create_dir_all(&job_dir)?;

    let spec = ClaudeJobSpec {
        job_id: job_id.clone(),
        args,
        created_at_ms,
    };
    write_json_atomic(&job_dir.join("spec.json"), &spec)?;
    let mut initial = initial_state(&job_id, created_at_ms);
    initial.claude_bin = Some(claude_bin.clone());
    initial.progress.stream = spec.args.stream;
    write_state(&job_dir, &initial)?;

    let exe = std::env::current_exe()?;
    let mut runner = StdCommand::new(exe);
    runner
        .arg("run-job")
        .arg(&job_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    runner.process_group(0);
    let child = runner.spawn()?;
    let pid = Some(child.id());
    // Detach the child. The runner persists all state to the job directory.
    drop(child);

    let mut state = read_state(&job_dir)?;
    state.status = "running".to_string();
    state.pid = pid;
    state.started_at_ms = Some(now_ms());
    write_state(&job_dir, &state)?;

    Ok(ClaudeJobStarted {
        job_id,
        status: state.status,
        job_dir,
        pid,
        created_at_ms,
        claude_bin,
    })
}

pub(crate) fn get_claude_job_status(job_id: &str) -> anyhow::Result<ClaudeJobState> {
    refresh_stale_state(&job_dir(job_id))
}

pub(crate) fn get_claude_job_result(job_id: &str) -> anyhow::Result<Option<ClaudeJobResult>> {
    let dir = job_dir(job_id);
    let state = refresh_stale_state(&dir)?;
    if state.status == "queued" || state.status == "running" {
        return Ok(None);
    }

    let result_path = dir.join("result.json");
    if result_path.exists() {
        return Ok(Some(read_json(&result_path)?));
    }

    Ok(Some(ClaudeJobResult {
        job_id: job_id.to_string(),
        status: state.status,
        thread_id: None,
        content: state
            .error
            .unwrap_or_else(|| "Claude job finished without result".to_string()),
        is_error: true,
        claude_bin: state.claude_bin,
        exit_code: state.exit_code,
        stderr_tail: read_tail_lossy(&dir.join("stderr.log"), STDERR_TAIL_BYTES)
            .unwrap_or_default(),
    }))
}

pub(crate) fn get_claude_job_tail(
    job_id: &str,
    max_chars: Option<usize>,
) -> anyhow::Result<serde_json::Value> {
    let dir = job_dir(job_id);
    let state = refresh_stale_state(&dir)?;
    let max_chars = max_chars.unwrap_or(DEFAULT_TAIL_CHARS);
    let partial = read_tail_chars_lossy(&dir.join("partial.txt"), max_chars).unwrap_or_default();

    Ok(json!({
        "jobId": job_id,
        "status": state.status,
        "partial": partial,
        "progress": state.progress,
        "claudeBin": state.claude_bin,
    }))
}

pub(crate) fn get_claude_job_events(
    job_id: &str,
    cursor: Option<usize>,
    max_events: Option<usize>,
) -> anyhow::Result<serde_json::Value> {
    let dir = job_dir(job_id);
    let state = refresh_stale_state(&dir)?;
    let cursor = cursor.unwrap_or(0);
    let max_events = max_events.unwrap_or(50).min(MAX_EVENTS_LIMIT);
    let events_path = dir.join("events.jsonl");
    let lines = fs::read_to_string(&events_path).unwrap_or_default();

    let mut events = Vec::new();
    let mut next_cursor = cursor;
    for (idx, line) in lines.lines().enumerate().skip(cursor).take(max_events) {
        next_cursor = idx + 1;
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => events.push(json!({ "cursor": idx, "event": value })),
            Err(_) => events.push(json!({ "cursor": idx, "raw": line })),
        }
    }

    Ok(json!({
        "jobId": job_id,
        "status": state.status,
        "cursor": cursor,
        "nextCursor": next_cursor,
        "eventCount": state.progress.event_count,
        "events": events,
    }))
}

pub(crate) async fn cancel_claude_job(job_id: &str) -> anyhow::Result<ClaudeJobState> {
    let dir = job_dir(job_id);
    let mut state = read_state(&dir)?;
    if matches!(state.status.as_str(), "succeeded" | "failed" | "cancelled") {
        return Ok(state);
    }

    if let Some(pid) = state.child_pid {
        terminate_pid(pid).await;
    }
    if let Some(pid) = state.pid {
        terminate_pid(pid).await;
    }
    state.status = "cancelled".to_string();
    state.finished_at_ms = Some(now_ms());
    state.error = Some("cancelled by claude-cancel".to_string());
    write_state(&dir, &state)?;
    Ok(state)
}

async fn terminate_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

pub(crate) async fn run_job_from_cli(job_id: &str) -> i32 {
    match run_job(job_id).await {
        Ok(()) => 0,
        Err(err) => {
            let dir = job_dir(job_id);
            let mut state = read_state(&dir).unwrap_or_else(|_| initial_state(job_id, now_ms()));
            if state.status == "cancelled" {
                return 0;
            }
            state.status = "failed".to_string();
            state.finished_at_ms = Some(now_ms());
            state.error = Some(err.to_string());
            let _ = write_state(&dir, &state);
            1
        }
    }
}

async fn run_job(job_id: &str) -> anyhow::Result<()> {
    let dir = job_dir(job_id);
    let spec: ClaudeJobSpec = read_json(&dir.join("spec.json"))?;
    let mut state = read_state(&dir).unwrap_or_else(|_| initial_state(job_id, spec.created_at_ms));
    state.status = "running".to_string();
    state.pid = Some(std::process::id());
    state.started_at_ms.get_or_insert_with(now_ms);
    write_state(&dir, &state)?;

    let claude_bin = resolve_claude_bin()?;
    state.claude_bin = Some(claude_bin.clone());
    write_state(&dir, &state)?;

    let mut cmd = Command::new(&claude_bin);
    cmd.kill_on_drop(true);
    cmd.args(claude_cli_args(&spec.args, spec.args.stream));
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if let Some(cwd) = &spec.args.cwd {
        cmd.current_dir(cwd);
    }

    if spec.args.stream {
        return run_stream_job(dir, job_id, state, cmd, claude_bin).await;
    }

    let child = cmd.spawn()?;
    state.child_pid = child.id();
    write_state(&dir, &state)?;

    let output = child.wait_with_output().await?;
    if matches!(read_state(&dir), Ok(state) if state.status == "cancelled") {
        return Ok(());
    }
    fs::write(dir.join("stdout.json"), &output.stdout)?;
    fs::write(dir.join("stderr.log"), &output.stderr)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr_tail =
        read_tail_lossy(&dir.join("stderr.log"), STDERR_TAIL_BYTES).unwrap_or_default();
    let exit_code = output.status.code();
    let parsed = serde_json::from_str::<ClaudePrintResponse>(stdout.trim());

    let result = match parsed {
        Ok(parsed) => ClaudeJobResult {
            job_id: job_id.to_string(),
            status: if parsed.is_error || !output.status.success() {
                "failed".to_string()
            } else {
                "succeeded".to_string()
            },
            thread_id: Some(parsed.session_id),
            content: parsed.result.unwrap_or_else(|| {
                format!(
                    "(claude --print returned no `result` field; subtype={:?}, is_error={})",
                    parsed.subtype, parsed.is_error,
                )
            }),
            is_error: parsed.is_error || !output.status.success(),
            claude_bin: Some(claude_bin),
            exit_code,
            stderr_tail,
        },
        Err(err) => ClaudeJobResult {
            job_id: job_id.to_string(),
            status: "failed".to_string(),
            thread_id: None,
            content: format!(
                "failed to parse claude --print json response: {err}. raw stdout (first 4KiB): {head}",
                head = &stdout.chars().take(4096).collect::<String>(),
            ),
            is_error: true,
            claude_bin: Some(claude_bin),
            exit_code,
            stderr_tail,
        },
    };

    write_json_atomic(&dir.join("result.json"), &result)?;
    state.status = result.status.clone();
    state.finished_at_ms = Some(now_ms());
    state.exit_code = exit_code;
    if result.is_error {
        state.error = Some(result.content.chars().take(512).collect());
    }
    write_state(&dir, &state)?;
    Ok(())
}

async fn run_stream_job(
    dir: PathBuf,
    job_id: &str,
    mut state: ClaudeJobState,
    mut cmd: Command,
    claude_bin: String,
) -> anyhow::Result<()> {
    let mut child = cmd.spawn()?;
    state.child_pid = child.id();
    state.progress.stream = true;
    write_state(&dir, &state)?;

    let stdout = child
        .stdout
        .take()
        .context("stream Claude child missing stdout pipe")?;
    let stderr = child
        .stderr
        .take()
        .context("stream Claude child missing stderr pipe")?;
    let stderr_path = dir.join("stderr.log");
    let stderr_task = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        fs::write(stderr_path, bytes)?;
        anyhow::Ok(())
    });

    let events_path = dir.join("events.jsonl");
    let partial_path = dir.join("partial.txt");
    let progress_path = dir.join("progress.json");
    let mut reader = BufReader::new(stdout).lines();
    let mut partial = String::new();
    let mut final_result: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut is_error = false;

    while let Some(line) = reader.next_line().await? {
        append_line(&events_path, &line)?;

        state.progress.event_count += 1;
        state.progress.last_event_at_ms = Some(now_ms());

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(sid) = find_string_field(&value, "session_id") {
                session_id = Some(sid);
            }
            if let Some(result) = value.get("result").and_then(|v| v.as_str()) {
                final_result = Some(result.to_string());
            }
            if value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                is_error = true;
            }
            let before = partial.len();
            append_text_deltas(&value, &mut partial);
            collect_tool_names(&value, &mut state.progress.seen_tools);
            if partial.len() != before {
                fs::write(&partial_path, &partial)?;
            }
        }

        state.progress.partial_bytes = partial.len();
        state.progress.last_text_tail = tail_chars(&partial, DEFAULT_TAIL_CHARS);
        write_json_atomic(&progress_path, &state.progress)?;
        write_state(&dir, &state)?;
    }

    let output_status = child.wait().await?;
    stderr_task.await??;
    if matches!(read_state(&dir), Ok(state) if state.status == "cancelled") {
        return Ok(());
    }

    let stderr_tail =
        read_tail_lossy(&dir.join("stderr.log"), STDERR_TAIL_BYTES).unwrap_or_default();
    let exit_code = output_status.code();
    let mut content = final_result.unwrap_or_else(|| partial.clone());
    let status = if is_error || !output_status.success() {
        "failed"
    } else {
        "succeeded"
    };
    if content.is_empty() && status == "failed" {
        content = stderr_tail.clone();
    }

    let result = ClaudeJobResult {
        job_id: job_id.to_string(),
        status: status.to_string(),
        thread_id: session_id,
        content,
        is_error: is_error || !output_status.success(),
        claude_bin: Some(claude_bin),
        exit_code,
        stderr_tail,
    };

    write_json_atomic(&dir.join("result.json"), &result)?;
    state.status = result.status.clone();
    state.finished_at_ms = Some(now_ms());
    state.exit_code = exit_code;
    state.progress.partial_bytes = partial.len();
    state.progress.last_text_tail = tail_chars(&partial, DEFAULT_TAIL_CHARS);
    if result.is_error {
        state.error = Some(result.content.chars().take(512).collect());
    }
    write_state(&dir, &state)?;
    Ok(())
}

fn claude_cli_args(args: &ClaudeRunArgs, stream: bool) -> Vec<String> {
    let mut cli_args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        if stream { "stream-json" } else { "json" }.to_string(),
    ];
    if stream {
        cli_args.push("--include-partial-messages".to_string());
        cli_args.push("--verbose".to_string());
    }

    if let Some(sid) = &args.resume_session_id {
        cli_args.push("--resume".to_string());
        cli_args.push(sid.clone());
    }
    if let Some(model) = &args.model {
        cli_args.push("--model".to_string());
        cli_args.push(model.clone());
    }

    // Claude's tool allow-list flag is variadic. If it appears before the
    // prompt, clap can consume the prompt as another tool name.
    cli_args.push(args.prompt.clone());

    if let Some(tools) = &args.allowed_tools {
        cli_args.push("--allowed-tools".to_string());
        cli_args.push(tools.clone());
    }

    cli_args
}

fn jobs_dir() -> PathBuf {
    std::env::var("CLAUDE_CHAT_MCP_JOBS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_jobs_dir())
}

fn job_dir(job_id: &str) -> PathBuf {
    jobs_dir().join(job_id)
}

fn initial_state(job_id: &str, created_at_ms: u64) -> ClaudeJobState {
    ClaudeJobState {
        job_id: job_id.to_string(),
        status: "queued".to_string(),
        created_at_ms,
        started_at_ms: None,
        finished_at_ms: None,
        pid: None,
        child_pid: None,
        claude_bin: None,
        exit_code: None,
        error: None,
        progress: ClaudeJobProgress::default(),
    }
}

fn resolve_claude_bin() -> anyhow::Result<String> {
    Ok(std::env::var("CLAUDE_CHAT_MCP_CLAUDE_BIN")
        .unwrap_or_else(|_| DEFAULT_CLAUDE_BIN.to_string()))
}

fn default_jobs_dir() -> PathBuf {
    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state_home).join("claude-chat-mcp/jobs");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/claude-chat-mcp/jobs");
    }
    PathBuf::from(".claude-chat-mcp/jobs")
}

fn refresh_stale_state(dir: &Path) -> anyhow::Result<ClaudeJobState> {
    let mut state = read_state(dir)?;
    if !matches!(state.status.as_str(), "queued" | "running") {
        return Ok(state);
    }

    let runner_alive = state.pid.map(is_pid_running).unwrap_or(false);
    let child_alive = state.child_pid.map(is_pid_running).unwrap_or(false);
    if runner_alive || child_alive {
        return Ok(state);
    }

    state.status = "failed".to_string();
    state.finished_at_ms = Some(now_ms());
    state.error = Some("Claude job process exited without writing a result".to_string());
    write_state(dir, &state)?;
    Ok(state)
}

fn append_line(path: &Path, line: &str) -> anyhow::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn append_text_deltas(value: &serde_json::Value, partial: &mut String) {
    if value.get("type").and_then(|v| v.as_str()) == Some("content_block_delta") {
        if let Some(text) = value
            .get("delta")
            .and_then(|delta| delta.get("text"))
            .and_then(|text| text.as_str())
        {
            partial.push_str(text);
        }
    }

    if value.get("type").and_then(|v| v.as_str()) == Some("assistant") {
        let message_text = assistant_message_text(value);
        if !message_text.is_empty() {
            if partial.is_empty() || message_text.starts_with(partial.as_str()) {
                *partial = message_text;
            } else if !partial.ends_with(&message_text) {
                if !partial.ends_with('\n') {
                    partial.push('\n');
                }
                partial.push_str(&message_text);
            }
        }
    }

    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                append_text_deltas(item, partial);
            }
        }
        serde_json::Value::Object(map) => {
            for nested in map.values() {
                append_text_deltas(nested, partial);
            }
        }
        _ => {}
    }
}

fn assistant_message_text(value: &serde_json::Value) -> String {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
    else {
        return String::new();
    };

    content
        .iter()
        .filter_map(|item| {
            if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                item.get("text").and_then(|text| text.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn collect_tool_names(value: &serde_json::Value, names: &mut Vec<String>) {
    if value.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
        if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.to_string());
            }
        }
    }

    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_tool_names(item, names);
            }
        }
        serde_json::Value::Object(map) => {
            for nested in map.values() {
                collect_tool_names(nested, names);
            }
        }
        _ => {}
    }
}

fn find_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get(field).and_then(|v| v.as_str()) {
                return Some(text.to_string());
            }
            map.values()
                .find_map(|nested| find_string_field(nested, field))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|nested| find_string_field(nested, field)),
        _ => None,
    }
}

fn is_pid_running(pid: u32) -> bool {
    StdCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn read_state(dir: &Path) -> anyhow::Result<ClaudeJobState> {
    read_json(&dir.join("state.json"))
}

fn write_state(dir: &Path, state: &ClaudeJobState) -> anyhow::Result<()> {
    write_json_atomic(&dir.join("state.json"), state)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn read_tail_lossy(path: &Path, max_bytes: usize) -> anyhow::Result<String> {
    let bytes = fs::read(path)?;
    let start = bytes.len().saturating_sub(max_bytes);
    Ok(String::from_utf8_lossy(&bytes[start..]).to_string())
}

fn read_tail_chars_lossy(path: &Path, max_chars: usize) -> anyhow::Result<String> {
    let text = fs::read_to_string(path)?;
    Ok(tail_chars(&text, max_chars))
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_precedes_allowed_tools_flag() {
        let args = ClaudeRunArgs {
            prompt: "ping".to_string(),
            cwd: None,
            model: None,
            allowed_tools: Some(String::new()),
            resume_session_id: None,
            stream: false,
        };

        assert_eq!(
            claude_cli_args(&args, false),
            vec![
                "--print",
                "--output-format",
                "json",
                "ping",
                "--allowed-tools",
                ""
            ]
        );
    }

    #[test]
    fn stream_args_enable_stream_json_and_partial_messages() {
        let args = ClaudeRunArgs {
            prompt: "ping".to_string(),
            cwd: None,
            model: None,
            allowed_tools: None,
            resume_session_id: None,
            stream: true,
        };

        assert_eq!(
            claude_cli_args(&args, true),
            vec![
                "--print",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--verbose",
                "ping",
            ]
        );
    }

    #[test]
    fn default_job_dir_uses_env_override() {
        std::env::set_var("CLAUDE_CHAT_MCP_JOBS_DIR", "/tmp/claude-chat-test");
        assert_eq!(jobs_dir(), PathBuf::from("/tmp/claude-chat-test"));
        std::env::remove_var("CLAUDE_CHAT_MCP_JOBS_DIR");
    }
}
