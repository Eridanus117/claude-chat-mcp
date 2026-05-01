//! Outgoing JSON-RPC plumbing.
//!
//! Simplified version of openai/codex codex-rs/mcp-server/src/outgoing_message.rs
//! (Apache-2.0). v1 wrapper does not issue server-initiated requests and does
//! not stream events, so the request-callback machinery and event-notification
//! helper are dropped.

use rmcp::model::CustomNotification;
use rmcp::model::CustomRequest;
use rmcp::model::ErrorData;
use rmcp::model::JsonRpcError;
use rmcp::model::JsonRpcMessage;
use rmcp::model::JsonRpcResponse;
use rmcp::model::JsonRpcVersion2_0;
use rmcp::model::RequestId;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

pub(crate) type OutgoingJsonRpcMessage = JsonRpcMessage<CustomRequest, Value, CustomNotification>;

pub(crate) struct OutgoingMessageSender {
    sender: mpsc::UnboundedSender<OutgoingMessage>,
}

impl OutgoingMessageSender {
    pub(crate) fn new(sender: mpsc::UnboundedSender<OutgoingMessage>) -> Self {
        Self { sender }
    }

    pub(crate) async fn send_response<T: Serialize>(&self, id: RequestId, response: T) {
        let result = match serde_json::to_value(response) {
            Ok(result) => result,
            Err(err) => {
                self.send_error(
                    id,
                    ErrorData::internal_error(format!("failed to serialize response: {err}"), None),
                )
                .await;
                return;
            }
        };
        let _ = self
            .sender
            .send(OutgoingMessage::Response(OutgoingResponse { id, result }));
    }

    pub(crate) async fn send_error(&self, id: RequestId, error: ErrorData) {
        let _ = self
            .sender
            .send(OutgoingMessage::Error(OutgoingError { id, error }));
    }
}

pub(crate) enum OutgoingMessage {
    Response(OutgoingResponse),
    Error(OutgoingError),
}

impl From<OutgoingMessage> for OutgoingJsonRpcMessage {
    fn from(val: OutgoingMessage) -> Self {
        match val {
            OutgoingMessage::Response(OutgoingResponse { id, result }) => {
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion2_0,
                    id,
                    result,
                })
            }
            OutgoingMessage::Error(OutgoingError { id, error }) => {
                JsonRpcMessage::Error(JsonRpcError {
                    jsonrpc: JsonRpcVersion2_0,
                    id,
                    error,
                })
            }
        }
    }
}

pub(crate) struct OutgoingResponse {
    pub id: RequestId,
    pub result: Value,
}

pub(crate) struct OutgoingError {
    pub id: RequestId,
    pub error: ErrorData,
}
