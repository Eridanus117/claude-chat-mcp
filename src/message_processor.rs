//! MCP request dispatch.
//!
//! Mirrors the structure of openai/codex codex-rs/mcp-server/src/message_processor.rs
//! (Apache-2.0), but stripped down: no resources, no prompts, no approvals,
//! no cancellation, no progress, no server-initiated requests.

use std::sync::Arc;

use crate::claude_backend::cancel_claude_job;
use crate::claude_backend::get_claude_job_events;
use crate::claude_backend::get_claude_job_result;
use crate::claude_backend::get_claude_job_status;
use crate::claude_backend::get_claude_job_tail;
use crate::claude_backend::start_claude_job;
use crate::claude_backend::ClaudeRunArgs;
use crate::outgoing::OutgoingMessageSender;
use crate::tool_schema::create_tool_for_claude_cancel_param;
use crate::tool_schema::create_tool_for_claude_events_param;
use crate::tool_schema::create_tool_for_claude_result_param;
use crate::tool_schema::create_tool_for_claude_start_param;
use crate::tool_schema::create_tool_for_claude_status_param;
use crate::tool_schema::create_tool_for_claude_tail_param;
use crate::tool_schema::ClaudeEventsParam;
use crate::tool_schema::ClaudeJobIdParam;
use crate::tool_schema::ClaudeStartToolCallParam;
use crate::tool_schema::ClaudeTailParam;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::Content;
use rmcp::model::ErrorCode;
use rmcp::model::ErrorData;
use rmcp::model::Implementation;
use rmcp::model::InitializeResult;
use rmcp::model::JsonRpcError;
use rmcp::model::JsonRpcNotification;
use rmcp::model::JsonRpcRequest;
use rmcp::model::JsonRpcResponse;
use rmcp::model::RequestId;
use rmcp::model::ServerCapabilities;
use rmcp::model::ToolsCapability;
use serde_json::json;

const SERVER_NAME: &str = "claude-chat-mcp";
const SERVER_TITLE: &str = "Claude Chat MCP";

pub(crate) struct MessageProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    initialized: bool,
}

impl MessageProcessor {
    pub(crate) fn new(outgoing: OutgoingMessageSender) -> Self {
        Self {
            outgoing: Arc::new(outgoing),
            initialized: false,
        }
    }

    pub(crate) async fn process_request(&mut self, request: JsonRpcRequest<ClientRequest>) {
        let request_id = request.id.clone();
        match request.request {
            ClientRequest::InitializeRequest(params) => {
                self.handle_initialize(request_id, params.params).await;
            }
            ClientRequest::PingRequest(_) => {
                self.outgoing.send_response(request_id, json!({})).await;
            }
            ClientRequest::ListToolsRequest(_) => {
                self.handle_list_tools(request_id).await;
            }
            ClientRequest::CallToolRequest(params) => {
                self.handle_call_tool(request_id, params.params).await;
            }
            other => {
                let method = describe_unsupported_request(&other);
                tracing::debug!("ignoring unsupported request: {method}");
                self.outgoing
                    .send_error(
                        request_id,
                        ErrorData::new(
                            ErrorCode::METHOD_NOT_FOUND,
                            format!("method not supported by claude-chat-mcp: {method}"),
                            Some(json!({ "method": method })),
                        ),
                    )
                    .await;
            }
        }
    }

    pub(crate) async fn process_response(&self, response: JsonRpcResponse<serde_json::Value>) {
        // We never issue server-initiated requests in v1, so any response from
        // the client is unexpected. Log and drop.
        tracing::warn!("ignoring unexpected client response: {:?}", response.id);
    }

    pub(crate) async fn process_notification(
        &self,
        notification: JsonRpcNotification<ClientNotification>,
    ) {
        tracing::debug!("notification received: {:?}", notification.notification);
    }

    pub(crate) fn process_error(&self, err: JsonRpcError) {
        tracing::error!("client error: {:?}", err);
    }

    async fn handle_initialize(
        &mut self,
        id: RequestId,
        params: rmcp::model::InitializeRequestParams,
    ) {
        if self.initialized {
            self.outgoing
                .send_error(
                    id,
                    ErrorData::invalid_request("initialize called more than once", None),
                )
                .await;
            return;
        }

        let server_info = Implementation {
            name: SERVER_NAME.to_string(),
            title: Some(SERVER_TITLE.to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: Some(
                "Observable Claude job bridge: starts async `claude --print` jobs and persists status, stream events, tails, and final results.".to_string(),
            ),
            icons: None,
            website_url: None,
        };

        let init_result = InitializeResult {
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                ..Default::default()
            },
            instructions: None,
            protocol_version: params.protocol_version.clone(),
            server_info,
        };

        self.initialized = true;
        self.outgoing.send_response(id, init_result).await;
    }

    async fn handle_list_tools(&self, id: RequestId) {
        let result = rmcp::model::ListToolsResult {
            meta: None,
            tools: vec![
                create_tool_for_claude_start_param(),
                create_tool_for_claude_status_param(),
                create_tool_for_claude_result_param(),
                create_tool_for_claude_tail_param(),
                create_tool_for_claude_events_param(),
                create_tool_for_claude_cancel_param(),
            ],
            next_cursor: None,
        };
        self.outgoing.send_response(id, result).await;
    }

    async fn handle_call_tool(&self, id: RequestId, params: CallToolRequestParams) {
        let CallToolRequestParams {
            name, arguments, ..
        } = params;

        match name.as_ref() {
            "claude-start" => self.handle_tool_call_claude_start(id, arguments).await,
            "claude-status" => self.handle_tool_call_claude_status(id, arguments).await,
            "claude-result" => self.handle_tool_call_claude_result(id, arguments).await,
            "claude-tail" => self.handle_tool_call_claude_tail(id, arguments).await,
            "claude-events" => self.handle_tool_call_claude_events(id, arguments).await,
            "claude-cancel" => self.handle_tool_call_claude_cancel(id, arguments).await,
            other => {
                let result = CallToolResult {
                    content: vec![Content::text(format!("unknown tool: {other}"))],
                    structured_content: None,
                    is_error: Some(true),
                    meta: None,
                };
                self.outgoing.send_response(id, result).await;
            }
        }
    }

    async fn handle_tool_call_claude_start(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let parsed: ClaudeStartToolCallParam = match parse_tool_arguments(arguments) {
            Ok(v) => v,
            Err(msg) => {
                self.outgoing.send_response(id, error_result(msg)).await;
                return;
            }
        };

        let args = ClaudeRunArgs {
            prompt: parsed.prompt,
            cwd: parsed.cwd,
            model: parsed.model,
            allowed_tools: parsed.allowed_tools,
            resume_session_id: parsed.thread_id,
            stream: parsed.stream,
        };

        match start_claude_job(args).await {
            Ok(started) => {
                self.outgoing
                    .send_response(
                        id,
                        json_result(json!({
                            "jobId": started.job_id,
                            "status": started.status,
                            "jobDir": started.job_dir,
                            "pid": started.pid,
                            "createdAtMs": started.created_at_ms,
                            "claudeBin": started.claude_bin
                        })),
                    )
                    .await;
            }
            Err(err) => {
                self.outgoing
                    .send_response(
                        id,
                        error_result(format!("failed to start Claude job: {err}")),
                    )
                    .await;
            }
        }
    }

    async fn handle_tool_call_claude_status(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let parsed: ClaudeJobIdParam = match parse_tool_arguments(arguments) {
            Ok(v) => v,
            Err(msg) => {
                self.outgoing.send_response(id, error_result(msg)).await;
                return;
            }
        };

        match get_claude_job_status(&parsed.job_id) {
            Ok(state) => {
                self.outgoing
                    .send_response(
                        id,
                        json_result(serde_json::to_value(state).unwrap_or_else(|_| json!({}))),
                    )
                    .await;
            }
            Err(err) => {
                self.outgoing
                    .send_response(
                        id,
                        error_result(format!("failed to read Claude job status: {err}")),
                    )
                    .await;
            }
        }
    }

    async fn handle_tool_call_claude_result(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let parsed: ClaudeJobIdParam = match parse_tool_arguments(arguments) {
            Ok(v) => v,
            Err(msg) => {
                self.outgoing.send_response(id, error_result(msg)).await;
                return;
            }
        };

        match get_claude_job_result(&parsed.job_id) {
            Ok(Some(result)) => {
                let is_error = if result.is_error { Some(true) } else { None };
                self.outgoing
                    .send_response(
                        id,
                        json_result_with_error(
                            serde_json::to_value(result).unwrap_or_else(|_| json!({})),
                            is_error,
                        ),
                    )
                    .await;
            }
            Ok(None) => {
                self.outgoing
                    .send_response(
                        id,
                        json_result(json!({
                            "jobId": parsed.job_id,
                            "status": "running",
                            "content": "Claude job is still running"
                        })),
                    )
                    .await;
            }
            Err(err) => {
                self.outgoing
                    .send_response(
                        id,
                        error_result(format!("failed to read Claude job result: {err}")),
                    )
                    .await;
            }
        }
    }

    async fn handle_tool_call_claude_cancel(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let parsed: ClaudeJobIdParam = match parse_tool_arguments(arguments) {
            Ok(v) => v,
            Err(msg) => {
                self.outgoing.send_response(id, error_result(msg)).await;
                return;
            }
        };

        match cancel_claude_job(&parsed.job_id).await {
            Ok(state) => {
                self.outgoing
                    .send_response(
                        id,
                        json_result(serde_json::to_value(state).unwrap_or_else(|_| json!({}))),
                    )
                    .await;
            }
            Err(err) => {
                self.outgoing
                    .send_response(
                        id,
                        error_result(format!("failed to cancel Claude job: {err}")),
                    )
                    .await;
            }
        }
    }

    async fn handle_tool_call_claude_tail(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let parsed: ClaudeTailParam = match parse_tool_arguments(arguments) {
            Ok(v) => v,
            Err(msg) => {
                self.outgoing.send_response(id, error_result(msg)).await;
                return;
            }
        };

        match get_claude_job_tail(&parsed.job_id, parsed.max_chars) {
            Ok(value) => {
                self.outgoing.send_response(id, json_result(value)).await;
            }
            Err(err) => {
                self.outgoing
                    .send_response(
                        id,
                        error_result(format!("failed to read Claude job tail: {err}")),
                    )
                    .await;
            }
        }
    }

    async fn handle_tool_call_claude_events(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let parsed: ClaudeEventsParam = match parse_tool_arguments(arguments) {
            Ok(v) => v,
            Err(msg) => {
                self.outgoing.send_response(id, error_result(msg)).await;
                return;
            }
        };

        match get_claude_job_events(&parsed.job_id, parsed.cursor, parsed.max_events) {
            Ok(value) => {
                self.outgoing.send_response(id, json_result(value)).await;
            }
            Err(err) => {
                self.outgoing
                    .send_response(
                        id,
                        error_result(format!("failed to read Claude job events: {err}")),
                    )
                    .await;
            }
        }
    }
}

fn parse_tool_arguments<T: serde::de::DeserializeOwned>(
    arguments: Option<rmcp::model::JsonObject>,
) -> Result<T, String> {
    let value = arguments
        .map(serde_json::Value::Object)
        .ok_or_else(|| "missing arguments object".to_string())?;
    serde_json::from_value(value).map_err(|e| format!("failed to parse tool arguments: {e}"))
}

fn error_result(text: String) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(text)],
        structured_content: None,
        is_error: Some(true),
        meta: None,
    }
}

fn json_result(value: serde_json::Value) -> CallToolResult {
    json_result_with_error(value, None)
}

fn json_result_with_error(value: serde_json::Value, is_error: Option<bool>) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(value.to_string())],
        structured_content: Some(value),
        is_error,
        meta: None,
    }
}

fn describe_unsupported_request(request: &ClientRequest) -> &'static str {
    match request {
        ClientRequest::InitializeRequest(_) => "initialize",
        ClientRequest::PingRequest(_) => "ping",
        ClientRequest::ListResourcesRequest(_) => "resources/list",
        ClientRequest::ListResourceTemplatesRequest(_) => "resources/templates/list",
        ClientRequest::ReadResourceRequest(_) => "resources/read",
        ClientRequest::SubscribeRequest(_) => "resources/subscribe",
        ClientRequest::UnsubscribeRequest(_) => "resources/unsubscribe",
        ClientRequest::ListPromptsRequest(_) => "prompts/list",
        ClientRequest::GetPromptRequest(_) => "prompts/get",
        ClientRequest::ListToolsRequest(_) => "tools/list",
        ClientRequest::CallToolRequest(_) => "tools/call",
        ClientRequest::SetLevelRequest(_) => "logging/setLevel",
        ClientRequest::CompleteRequest(_) => "completion/complete",
        _ => "<other>",
    }
}
