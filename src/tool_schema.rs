//! Tool input/output schemas for observable async Claude jobs.
//!
//! Schema construction follows the same pattern as openai/codex
//! codex-rs/mcp-server/src/codex_tool_config.rs (Apache-2.0): generate via
//! schemars then prune to JSON-Schema core keys.

use std::sync::Arc;

use rmcp::model::JsonObject;
use rmcp::model::Tool;
use schemars::r#gen::SchemaSettings;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Input for the `claude-start` tool. Starts a background Claude job and
/// returns immediately with a job id.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "kebab-case")]
pub struct ClaudeStartToolCallParam {
    /// The user prompt to send to Claude. Required.
    pub prompt: String,

    /// Optional working directory for the spawned `claude` subprocess. If
    /// relative, resolved against the runner's cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// Optional model alias or full id (e.g. `sonnet`, `opus`,
    /// `claude-sonnet-4-6`). Forwarded to `--model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Optional comma-or-space separated allow list of tool names. Forwarded
    /// to `--allowed-tools`. Use empty string to disable all tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,

    /// Optional Claude thread id. When provided, forwarded to
    /// `claude --resume <thread_id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,

    /// When true, run Claude with `--output-format stream-json` and persist
    /// observable intermediate output for claude-tail / claude-events.
    #[serde(default)]
    pub stream: bool,
}

/// Input for `claude-status`, `claude-result`, and `claude-cancel`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeJobIdParam {
    /// The job id returned by `claude-start`.
    pub job_id: String,
}

/// Input for `claude-tail`. Returns a compact tail of the current aggregated
/// assistant text and progress metadata.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTailParam {
    /// The job id returned by `claude-start`.
    pub job_id: String,

    /// Maximum characters to return from partial.txt. Defaults to 4000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
}

/// Input for `claude-events`. Returns raw stream-json events from events.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeEventsParam {
    /// The job id returned by `claude-start`.
    pub job_id: String,

    /// Zero-based event cursor. Defaults to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,

    /// Maximum number of events to return. Defaults to 50, capped at 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events: Option<usize>,
}

pub(crate) fn create_tool_for_claude_start_param() -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ClaudeStartToolCallParam>();

    Tool {
        name: "claude-start".into(),
        title: Some("Claude Start".to_string()),
        input_schema: create_tool_input_schema(schema),
        output_schema: Some(generic_object_output_schema()),
        description: Some(
            "Start a Claude job in the background and return a jobId immediately. Use stream=true for observable long tasks; use thread-id to resume an existing Claude session."
                .into(),
        ),
        annotations: None,
        execution: None,
        icons: None,
        meta: None,
    }
}

pub(crate) fn create_tool_for_claude_status_param() -> Tool {
    create_job_id_tool(
        "claude-status",
        "Claude Status",
        "Return the current status for a background Claude job.",
    )
}

pub(crate) fn create_tool_for_claude_result_param() -> Tool {
    create_job_id_tool(
        "claude-result",
        "Claude Result",
        "Return the result for a completed background Claude job, or a running status if it has not finished yet.",
    )
}

pub(crate) fn create_tool_for_claude_tail_param() -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ClaudeTailParam>();

    Tool {
        name: "claude-tail".into(),
        title: Some("Claude Tail".to_string()),
        input_schema: create_tool_input_schema(schema),
        output_schema: Some(generic_object_output_schema()),
        description: Some(
            "Return the current partial assistant text tail and progress metadata for a streaming Claude job."
                .into(),
        ),
        annotations: None,
        execution: None,
        icons: None,
        meta: None,
    }
}

pub(crate) fn create_tool_for_claude_events_param() -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ClaudeEventsParam>();

    Tool {
        name: "claude-events".into(),
        title: Some("Claude Events".to_string()),
        input_schema: create_tool_input_schema(schema),
        output_schema: Some(generic_object_output_schema()),
        description: Some(
            "Return raw stream-json Claude events from a background job using a cursor for incremental polling."
                .into(),
        ),
        annotations: None,
        execution: None,
        icons: None,
        meta: None,
    }
}

pub(crate) fn create_tool_for_claude_cancel_param() -> Tool {
    create_job_id_tool(
        "claude-cancel",
        "Claude Cancel",
        "Cancel a running background Claude job.",
    )
}

fn create_job_id_tool(name: &str, title: &str, description: &str) -> Tool {
    let schema = SchemaSettings::draft2019_09()
        .with(|s| {
            s.inline_subschemas = true;
            s.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ClaudeJobIdParam>();

    Tool {
        name: name.to_string().into(),
        title: Some(title.to_string()),
        input_schema: create_tool_input_schema(schema),
        output_schema: Some(generic_object_output_schema()),
        description: Some(description.to_string().into()),
        annotations: None,
        execution: None,
        icons: None,
        meta: None,
    }
}

fn generic_object_output_schema() -> Arc<JsonObject> {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": true
    });
    match schema {
        serde_json::Value::Object(map) => Arc::new(map),
        _ => unreachable!("json literal must be an object"),
    }
}

fn create_tool_input_schema(schema: schemars::schema::RootSchema) -> Arc<JsonObject> {
    #[expect(clippy::expect_used)]
    let schema_value = serde_json::to_value(&schema).expect("tool schema must serialize");
    let mut schema_object = match schema_value {
        serde_json::Value::Object(object) => object,
        _ => panic!("tool schema must serialize to a JSON object"),
    };

    let mut input_schema = JsonObject::new();
    for key in ["properties", "required", "type", "$defs", "definitions"] {
        if let Some(value) = schema_object.remove(key) {
            input_schema.insert(key.to_string(), value);
        }
    }
    Arc::new(input_schema)
}
