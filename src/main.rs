//! claude-chat-mcp: stdio MCP server that exposes observable async Claude job
//! tools backed by `claude --print`.
//!
//! Topology lifted from openai/codex codex-rs/mcp-server (Apache-2.0):
//! stdin reader -> mpsc -> message processor -> mpsc -> stdout writer.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod claude_backend;
mod message_processor;
mod outgoing;
mod tool_schema;

use std::io::Result as IoResult;

use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::JsonRpcMessage;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::io::{self};
use tokio::sync::mpsc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::message_processor::MessageProcessor;
use crate::outgoing::OutgoingJsonRpcMessage;
use crate::outgoing::OutgoingMessage;
use crate::outgoing::OutgoingMessageSender;

const CHANNEL_CAPACITY: usize = 128;

type IncomingMessage = JsonRpcMessage<ClientRequest, Value, ClientNotification>;

#[tokio::main]
async fn main() -> IoResult<()> {
    let mut args = std::env::args();
    let _exe = args.next();
    if matches!(args.next().as_deref(), Some("run-job")) {
        let Some(job_id) = args.next() else {
            return Ok(());
        };
        std::process::exit(crate::claude_backend::run_job_from_cli(&job_id).await);
    }

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::from_default_env());
    let _ = tracing_subscriber::registry().with(fmt_layer).try_init();

    let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingMessage>(CHANNEL_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();

    let stdin_reader_handle = tokio::spawn(async move {
        let stdin = io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await.unwrap_or_default() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<IncomingMessage>(&line) {
                Ok(msg) => {
                    if incoming_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => error!("failed to deserialize JSON-RPC message: {e}; line={line}"),
            }
        }
        debug!("stdin reader finished (EOF)");
    });

    let processor_handle = tokio::spawn({
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);
        let mut processor = MessageProcessor::new(outgoing_message_sender);
        async move {
            while let Some(msg) = incoming_rx.recv().await {
                match msg {
                    JsonRpcMessage::Request(r) => processor.process_request(r).await,
                    JsonRpcMessage::Response(r) => processor.process_response(r).await,
                    JsonRpcMessage::Notification(n) => processor.process_notification(n).await,
                    JsonRpcMessage::Error(e) => processor.process_error(e),
                }
            }
            info!("processor task exited (channel closed)");
        }
    });

    let stdout_writer_handle = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stdout = io::stdout();
        while let Some(outgoing_message) = outgoing_rx.recv().await {
            let msg: OutgoingJsonRpcMessage = outgoing_message.into();
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    if let Err(e) = stdout.write_all(json.as_bytes()).await {
                        error!("failed to write to stdout: {e}");
                        break;
                    }
                    if let Err(e) = stdout.write_all(b"\n").await {
                        error!("failed to write newline: {e}");
                        break;
                    }
                }
                Err(e) => error!("failed to serialize JSON-RPC message: {e}"),
            }
        }
        info!("stdout writer exited (channel closed)");
    });

    let _ = tokio::join!(stdin_reader_handle, processor_handle, stdout_writer_handle);
    Ok(())
}
