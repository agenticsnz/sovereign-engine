use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Extension, Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::auth::tokens;
use crate::auth::AuthUser;
use crate::proxy::streaming::proxy_to_backend;
use crate::scheduler::usage;
use crate::AppState;

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/messages", post(messages))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Anthropic Messages API types
// ---------------------------------------------------------------------------

/// Metadata included in the Anthropic request (e.g. user_id for attribution).
#[derive(Debug, Deserialize, Default)]
struct AnthropicMetadata {
    #[serde(default)]
    user_id: Option<String>,
}

/// A single tool definition in an Anthropic request.
#[derive(Debug, Deserialize)]
struct AnthropicTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value, // string or array of content blocks
}

#[derive(Debug, Deserialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u64,
    #[serde(default)]
    messages: Vec<AnthropicMessage>,
    #[serde(default)]
    system: Option<serde_json::Value>, // string or array of {type: "text", text: "..."}
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<AnthropicMetadata>,
    #[serde(default)]
    tools: Option<Vec<AnthropicTool>>,
}

/// Token usage in Anthropic format.
#[derive(Debug, Serialize, Clone)]
struct AnthropicUsage {
    input_tokens: i64,
    output_tokens: i64,
}

/// Non-streaming Anthropic response.
#[derive(Debug, Serialize)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: &'static str,
    role: &'static str,
    content: Vec<serde_json::Value>,
    model: String,
    stop_reason: Option<String>,
    stop_sequence: Option<serde_json::Value>,
    usage: AnthropicUsage,
}

/// Anthropic error response.
#[derive(Debug, Serialize)]
struct AnthropicError {
    #[serde(rename = "type")]
    response_type: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(Debug, Serialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

// ---------------------------------------------------------------------------
// OpenAI types (for deserializing backend responses)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCallFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCall {
    id: Option<String>,
    function: Option<OpenAIToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct OpenAIMessage {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: Option<OpenAIMessage>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Option<Vec<OpenAIChoice>>,
    usage: Option<OpenAIUsage>,
}

/// A single delta in an OpenAI streaming chunk.
#[derive(Debug, Deserialize)]
struct OpenAIStreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAIStreamToolCallDelta>>,
}

/// Tool call delta in an OpenAI streaming chunk.
#[derive(Debug, Deserialize)]
struct OpenAIStreamToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<OpenAIStreamToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamToolCallFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// A single choice in an OpenAI streaming chunk.
#[derive(Debug, Deserialize)]
struct OpenAIStreamChoice {
    delta: Option<OpenAIStreamDelta>,
    finish_reason: Option<String>,
}

/// An OpenAI streaming chunk.
#[derive(Debug, Deserialize)]
struct OpenAIStreamChunk {
    choices: Option<Vec<OpenAIStreamChoice>>,
    usage: Option<OpenAIUsage>,
}

// ---------------------------------------------------------------------------
// Translation helpers
// ---------------------------------------------------------------------------

/// Extract plain text from the system field. Handles both:
///   - `"system": "You are helpful"` (string)
///   - `"system": [{"type": "text", "text": "You are helpful"}]` (array of blocks)
fn system_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Convert an Anthropic message content value to a plain string.
/// Handles both `"content": "hello"` and `"content": [{"type":"text","text":"hello"}]`.
/// Non-text blocks (images, tool_result, etc.) are skipped for the text extraction.
fn content_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Check if content blocks contain tool_use blocks (assistant message).
fn content_has_tool_use(value: &serde_json::Value) -> bool {
    if let serde_json::Value::Array(blocks) = value {
        blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
    } else {
        false
    }
}

/// Check if content blocks contain tool_result blocks (user message).
fn content_has_tool_result(value: &serde_json::Value) -> bool {
    if let serde_json::Value::Array(blocks) = value {
        blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
    } else {
        false
    }
}

/// Translate an Anthropic assistant message with tool_use blocks into OpenAI format.
/// Returns an OpenAI assistant message with tool_calls array.
fn translate_assistant_tool_use(content: &serde_json::Value) -> serde_json::Value {
    let blocks = match content {
        serde_json::Value::Array(b) => b,
        _ => {
            return serde_json::json!({
                "role": "assistant",
                "content": content_to_string(content),
            });
        }
    };

    let mut text_parts = String::new();
    let mut tool_calls = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
                let arguments = serde_json::to_string(&input).unwrap_or_default();

                tool_calls.push(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                }));
            }
            _ => {}
        }
    }

    let mut msg = serde_json::json!({
        "role": "assistant",
    });

    if !text_parts.is_empty() {
        msg["content"] = serde_json::Value::String(text_parts);
    } else {
        msg["content"] = serde_json::Value::Null;
    }

    if !tool_calls.is_empty() {
        msg["tool_calls"] = serde_json::Value::Array(tool_calls);
    }

    msg
}

/// Translate Anthropic user message with tool_result blocks into OpenAI format.
/// Each tool_result becomes a separate message with role "tool".
/// Any text blocks become a regular user message.
fn translate_user_tool_result(content: &serde_json::Value) -> Vec<serde_json::Value> {
    let blocks = match content {
        serde_json::Value::Array(b) => b,
        _ => {
            return vec![serde_json::json!({
                "role": "user",
                "content": content_to_string(content),
            })];
        }
    };

    let mut messages = Vec::new();
    let mut text_parts = String::new();

    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("tool_result") => {
                // Flush any accumulated text as a user message first
                if !text_parts.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": text_parts,
                    }));
                    text_parts = String::new();
                }

                let tool_call_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // Content can be a string or array of content blocks
                let result_content = if let Some(content_val) = block.get("content") {
                    content_to_string(content_val)
                } else {
                    String::new()
                };

                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": result_content,
                }));
            }
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push_str(t);
                }
            }
            _ => {}
        }
    }

    // Flush any remaining text
    if !text_parts.is_empty() {
        messages.push(serde_json::json!({
            "role": "user",
            "content": text_parts,
        }));
    }

    messages
}

/// Translate an Anthropic request into an OpenAI chat completion request body.
fn translate_request(req: &AnthropicRequest) -> serde_json::Value {
    let mut openai_messages: Vec<serde_json::Value> = Vec::new();

    // System prompt becomes the first message with role "system"
    if let Some(ref system) = req.system {
        let system_text = system_to_string(system);
        if !system_text.is_empty() {
            openai_messages.push(serde_json::json!({
                "role": "system",
                "content": system_text,
            }));
        }
    }

    // Map user/assistant messages, handling tool use/result content blocks
    for msg in &req.messages {
        if msg.role == "assistant" && content_has_tool_use(&msg.content) {
            openai_messages.push(translate_assistant_tool_use(&msg.content));
        } else if msg.role == "user" && content_has_tool_result(&msg.content) {
            openai_messages.extend(translate_user_tool_result(&msg.content));
        } else {
            openai_messages.push(serde_json::json!({
                "role": msg.role,
                "content": content_to_string(&msg.content),
            }));
        }
    }

    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": openai_messages,
        "stream": req.stream,
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(top_p) = req.top_p {
        body["top_p"] = serde_json::json!(top_p);
    }
    if let Some(ref stop) = req.stop_sequences {
        body["stop"] = serde_json::json!(stop);
    }

    // Translate Anthropic tools to OpenAI function-calling tools
    if let Some(ref tools) = req.tools {
        let openai_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                let mut func = serde_json::json!({
                    "name": t.name,
                });
                if let Some(ref desc) = t.description {
                    func["description"] = serde_json::json!(desc);
                }
                if let Some(ref schema) = t.input_schema {
                    func["parameters"] = schema.clone();
                }
                serde_json::json!({
                    "type": "function",
                    "function": func,
                })
            })
            .collect();
        body["tools"] = serde_json::json!(openai_tools);
    }

    // When streaming, request usage stats in the final chunk (OpenAI extension)
    if req.stream {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }

    body
}

/// Map OpenAI finish_reason to Anthropic stop_reason.
fn translate_stop_reason(finish_reason: Option<&str>) -> String {
    match finish_reason {
        Some("stop") => "end_turn".to_string(),
        Some("length") => "max_tokens".to_string(),
        Some("tool_calls") => "tool_use".to_string(),
        Some(other) => other.to_string(),
        None => "end_turn".to_string(),
    }
}

/// Generate a msg_ prefixed ID.
fn generate_message_id() -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("msg_{}", &id[..24])
}

/// Generate a toolu_ prefixed ID for tool use blocks.
fn generate_tool_use_id() -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("toolu_{}", &id[..24])
}

/// Extract token usage from raw OpenAI response bytes.
fn extract_usage(body: &[u8]) -> (i64, i64) {
    match serde_json::from_slice::<OpenAIResponse>(body) {
        Ok(resp) => {
            if let Some(u) = resp.usage {
                (
                    u.prompt_tokens.unwrap_or(0),
                    u.completion_tokens.unwrap_or(0),
                )
            } else {
                (0, 0)
            }
        }
        Err(_) => (0, 0),
    }
}

/// Translate an OpenAI non-streaming response into Anthropic content blocks.
/// Returns (content_blocks, stop_reason, input_tokens, output_tokens).
fn translate_openai_response(
    openai_resp: &OpenAIResponse,
    requested_model: &str,
) -> AnthropicResponse {
    let choice = openai_resp.choices.as_ref().and_then(|c| c.first());

    let finish_reason = choice.and_then(|c| c.finish_reason.as_deref());
    let message = choice.and_then(|c| c.message.as_ref());

    let mut content_blocks: Vec<serde_json::Value> = Vec::new();

    // Add text content if present
    if let Some(msg) = message {
        if let Some(ref text) = msg.content {
            if !text.is_empty() {
                content_blocks.push(serde_json::json!({
                    "type": "text",
                    "text": text,
                }));
            }
        }

        // Add tool_calls as tool_use content blocks
        if let Some(ref tool_calls) = msg.tool_calls {
            for tc in tool_calls {
                let id = tc.id.as_deref().unwrap_or("").to_string();
                // Use the backend-provided ID if available, otherwise generate one
                let tool_id = if id.is_empty() {
                    generate_tool_use_id()
                } else {
                    id
                };

                let name = tc
                    .function
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or_default();
                let arguments_str = tc
                    .function
                    .as_ref()
                    .and_then(|f| f.arguments.clone())
                    .unwrap_or_else(|| "{}".to_string());
                let input: serde_json::Value =
                    serde_json::from_str(&arguments_str).unwrap_or(serde_json::json!({}));

                content_blocks.push(serde_json::json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": name,
                    "input": input,
                }));
            }
        }
    }

    // If no content blocks at all, add an empty text block
    if content_blocks.is_empty() {
        content_blocks.push(serde_json::json!({
            "type": "text",
            "text": "",
        }));
    }

    let (input_tokens, output_tokens) = openai_resp
        .usage
        .as_ref()
        .map(|u| {
            (
                u.prompt_tokens.unwrap_or(0),
                u.completion_tokens.unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));

    AnthropicResponse {
        id: generate_message_id(),
        response_type: "message",
        role: "assistant",
        content: content_blocks,
        model: requested_model.to_string(),
        stop_reason: Some(translate_stop_reason(finish_reason)),
        stop_sequence: None,
        usage: AnthropicUsage {
            input_tokens,
            output_tokens,
        },
    }
}

// ---------------------------------------------------------------------------
// Error response helpers
// ---------------------------------------------------------------------------

fn error_response(status: StatusCode, error_type: &str, message: String) -> Response<Body> {
    let body = AnthropicError {
        response_type: "error",
        error: AnthropicErrorDetail {
            error_type: error_type.to_string(),
            message,
        },
    };
    (status, Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// Streaming SSE helpers
// ---------------------------------------------------------------------------

/// Format a single Anthropic SSE event with named event type.
fn sse_event(event: &str, data: &serde_json::Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

/// State for tracking tool call assembly during streaming.
#[derive(Debug, Clone)]
struct StreamingToolCall {
    index: usize,
    id: String,
    name: String,
    arguments: String,
    /// The content block index in the Anthropic response.
    block_index: usize,
    /// Whether content_block_start has been emitted for this tool call.
    started: bool,
}

/// Transform an OpenAI SSE byte stream into Anthropic SSE events.
///
/// Spawns a task that reads the OpenAI stream, translates each chunk, and
/// sends Anthropic-format SSE events through a channel. Returns the body
/// stream and a shared usage accumulator that is populated when the stream ends.
fn transform_stream(
    openai_stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    model: String,
    msg_id: String,
) -> (Body, Arc<tokio::sync::Mutex<(i64, i64)>>) {
    let usage_accumulator: Arc<tokio::sync::Mutex<(i64, i64)>> =
        Arc::new(tokio::sync::Mutex::new((0, 0)));
    let usage_ref = usage_accumulator.clone();

    // Channel to bridge the spawned transform task and the response body stream
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        use futures::StreamExt;

        let mut text_block_started = false;
        let mut buffer = String::new();
        let mut final_stop_reason: Option<String> = None;
        let mut final_usage = (0i64, 0i64);
        // Next available content block index (text block is 0 if used)
        let mut next_block_index: usize = 0;
        // The block index assigned to the text content block (if any)
        let mut text_block_index: Option<usize> = None;
        // Track tool calls being assembled
        let mut tool_calls: Vec<StreamingToolCall> = Vec::new();

        // Helper macro: send an SSE event; returns early if the receiver is gone
        macro_rules! send_event {
            ($event:expr, $data:expr) => {
                if tx
                    .send(Ok(Bytes::from(sse_event($event, $data))))
                    .await
                    .is_err()
                {
                    // Client disconnected
                    let mut acc = usage_ref.lock().await;
                    *acc = final_usage;
                    return;
                }
            };
        }

        // Emit message_start
        let start_event = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        send_event!("message_start", &start_event);

        // Emit ping
        let ping = serde_json::json!({"type": "ping"});
        send_event!("ping", &ping);

        let mut pinned_stream = std::pin::pin!(openai_stream);

        while let Some(chunk_result) = pinned_stream.next().await {
            let chunk_bytes = match chunk_result {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "Error reading stream chunk");
                    break;
                }
            };

            // Append to buffer for line-based parsing
            let chunk_str = String::from_utf8_lossy(&chunk_bytes);
            buffer.push_str(&chunk_str);

            // Process complete lines from the buffer
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Handle [DONE] marker
                if line == "data: [DONE]" {
                    continue;
                }

                // Parse SSE data lines
                let data_str = if let Some(stripped) = line.strip_prefix("data: ") {
                    stripped
                } else {
                    continue;
                };

                let chunk: OpenAIStreamChunk = match serde_json::from_str(data_str) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                // Capture usage if present (typically in the final chunk)
                if let Some(ref u) = chunk.usage {
                    final_usage = (
                        u.prompt_tokens.unwrap_or(final_usage.0),
                        u.completion_tokens.unwrap_or(final_usage.1),
                    );
                }

                if let Some(ref choices) = chunk.choices {
                    for choice in choices {
                        if let Some(ref reason) = choice.finish_reason {
                            final_stop_reason = Some(translate_stop_reason(Some(reason)));
                        }

                        if let Some(ref delta) = choice.delta {
                            // Handle text content deltas
                            if let Some(ref text) = delta.content {
                                if !text.is_empty() {
                                    if !text_block_started {
                                        text_block_started = true;
                                        let idx = next_block_index;
                                        text_block_index = Some(idx);
                                        next_block_index += 1;

                                        let block_start = serde_json::json!({
                                            "type": "content_block_start",
                                            "index": idx,
                                            "content_block": {"type": "text", "text": ""}
                                        });
                                        send_event!("content_block_start", &block_start);
                                    }

                                    let idx = text_block_index.unwrap_or(0);
                                    let delta_event = serde_json::json!({
                                        "type": "content_block_delta",
                                        "index": idx,
                                        "delta": {"type": "text_delta", "text": text}
                                    });
                                    send_event!("content_block_delta", &delta_event);
                                }
                            }

                            // Handle tool call deltas
                            if let Some(ref tc_deltas) = delta.tool_calls {
                                for tc_delta in tc_deltas {
                                    let tc_index = tc_delta.index.unwrap_or(0);

                                    // Find or create the tool call entry
                                    let tc = if let Some(existing) =
                                        tool_calls.iter_mut().find(|t| t.index == tc_index)
                                    {
                                        existing
                                    } else {
                                        // Close text block if it was open and this is
                                        // the first tool call
                                        if text_block_started && tool_calls.is_empty() {
                                            let idx = text_block_index.unwrap_or(0);
                                            let block_stop = serde_json::json!({
                                                "type": "content_block_stop",
                                                "index": idx,
                                            });
                                            send_event!("content_block_stop", &block_stop);
                                            text_block_started = false;
                                        }

                                        let block_idx = next_block_index;
                                        next_block_index += 1;

                                        tool_calls.push(StreamingToolCall {
                                            index: tc_index,
                                            id: String::new(),
                                            name: String::new(),
                                            arguments: String::new(),
                                            block_index: block_idx,
                                            started: false,
                                        });
                                        tool_calls.last_mut().unwrap()
                                    };

                                    // Accumulate ID and name from the first delta
                                    if let Some(ref id) = tc_delta.id {
                                        tc.id = id.clone();
                                    }
                                    if let Some(ref func) = tc_delta.function {
                                        if let Some(ref name) = func.name {
                                            tc.name.push_str(name);
                                        }
                                        if let Some(ref args) = func.arguments {
                                            tc.arguments.push_str(args);
                                        }
                                    }

                                    // Emit content_block_start when we have the
                                    // tool name (first delta that carries a name)
                                    if !tc.started && !tc.name.is_empty() {
                                        tc.started = true;
                                        let id = if tc.id.is_empty() {
                                            generate_tool_use_id()
                                        } else {
                                            tc.id.clone()
                                        };
                                        tc.id = id.clone();

                                        let block_start = serde_json::json!({
                                            "type": "content_block_start",
                                            "index": tc.block_index,
                                            "content_block": {
                                                "type": "tool_use",
                                                "id": id,
                                                "name": tc.name,
                                                "input": {},
                                            }
                                        });
                                        send_event!("content_block_start", &block_start);
                                    }

                                    // Emit argument deltas as input_json_delta
                                    if tc.started {
                                        if let Some(ref func) = tc_delta.function {
                                            if let Some(ref args) = func.arguments {
                                                if !args.is_empty() {
                                                    let delta_event = serde_json::json!({
                                                        "type": "content_block_delta",
                                                        "index": tc.block_index,
                                                        "delta": {
                                                            "type": "input_json_delta",
                                                            "partial_json": args,
                                                        }
                                                    });
                                                    send_event!(
                                                        "content_block_delta",
                                                        &delta_event
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Close text block if still open
        if text_block_started {
            let idx = text_block_index.unwrap_or(0);
            let block_stop = serde_json::json!({
                "type": "content_block_stop",
                "index": idx,
            });
            send_event!("content_block_stop", &block_stop);
        }

        // Close any open tool call blocks
        for tc in &tool_calls {
            if tc.started {
                let block_stop = serde_json::json!({
                    "type": "content_block_stop",
                    "index": tc.block_index,
                });
                send_event!("content_block_stop", &block_stop);
            }
        }

        // If no content blocks were emitted at all, emit an empty text block pair
        if !text_block_started && tool_calls.is_empty() {
            let block_start = serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            });
            send_event!("content_block_start", &block_start);

            let block_stop = serde_json::json!({
                "type": "content_block_stop",
                "index": 0,
            });
            send_event!("content_block_stop", &block_stop);
        }

        let stop_reason = final_stop_reason.unwrap_or_else(|| "end_turn".to_string());

        // Emit message_delta with stop_reason and final usage
        let msg_delta = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null,
            },
            "usage": {"output_tokens": final_usage.1}
        });
        send_event!("message_delta", &msg_delta);

        // Emit message_stop
        let msg_stop = serde_json::json!({"type": "message_stop"});
        send_event!("message_stop", &msg_stop);

        // Store final usage for the caller
        let mut acc = usage_ref.lock().await;
        *acc = final_usage;
    });

    let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    (Body::from_stream(body_stream), usage_accumulator)
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// POST /v1/messages -- Anthropic Messages API compatible endpoint.
/// Translates to OpenAI format, proxies to the llama.cpp backend, and
/// translates the response back to Anthropic format.
async fn messages(
    State(state): State<Arc<AppState>>,
    Extension(auth_user): Extension<AuthUser>,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Parse request body as Anthropic format
    let parsed: AnthropicRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid request body: {}", e),
            );
        }
    };

    info!(
        model = %parsed.model,
        stream = parsed.stream,
        user_id = %auth_user.user_id,
        "Anthropic messages request"
    );

    let start = Instant::now();

    // 2. Resolve model via scheduler
    let model = match state
        .scheduler
        .resolve_model(
            &state.db,
            &parsed.model,
            auth_user.category_id.as_deref(),
            auth_user.specific_model_id.as_deref(),
        )
        .await
    {
        Ok(m) => m,
        Err(e) => {
            error!(error = %e, model = %parsed.model, "Model resolution failed");
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found_error",
                format!("Model not found: {}", parsed.model),
            );
        }
    };

    // 3. Check model loaded status
    if !model.loaded {
        let msg = if model.hf_repo == parsed.model {
            format!("Model '{}' is not currently loaded", parsed.model)
        } else {
            format!(
                "Model '{}' (overridden by token from '{}') is not currently loaded",
                model.hf_repo, parsed.model
            )
        };
        warn!(
            model_id = %model.id,
            hf_repo = %model.hf_repo,
            requested = %parsed.model,
            "Anthropic: model not loaded"
        );
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "overloaded_error", msg);
    }

    // 4. Check reservation
    if !auth_user.is_internal {
        if let Some(active) = state.scheduler.active_reservation().await {
            if active.user_id != auth_user.user_id {
                warn!(
                    user = %auth_user.user_id,
                    reserved_by = %active.user_id,
                    "Anthropic: blocked by reservation"
                );
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "overloaded_error",
                    "System is currently reserved for exclusive use".to_string(),
                );
            }
        }
    }

    // 5. Acquire concurrency gate slot
    let queue_start = Instant::now();
    let settings = state.scheduler.settings().await;
    let timeout = Duration::from_secs(settings.queue_timeout_secs);
    let _slot = match state
        .scheduler
        .gate()
        .acquire_with_timeout(
            &model.id,
            &auth_user.user_id,
            &state.db,
            &settings,
            state.scheduler.queue(),
            timeout,
        )
        .await
    {
        Ok(slot) => slot,
        Err(_) => {
            warn!(
                model = %model.id,
                user = %auth_user.user_id,
                "Request timed out in queue"
            );
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Server is busy. Please retry later.".to_string(),
            );
        }
    };
    let queued_ms = queue_start.elapsed().as_millis() as i64;

    // 6. Look up backend API key
    let api_key: Option<String> =
        sqlx::query_as::<_, (String,)>("SELECT api_key FROM container_secrets WHERE model_id = ?")
            .bind(&model.id)
            .fetch_optional(&state.db.pool)
            .await
            .ok()
            .flatten()
            .map(|(key,)| key);

    // 7. Translate request to OpenAI format
    let openai_body = translate_request(&parsed);
    let openai_bytes = Bytes::from(serde_json::to_vec(&openai_body).unwrap());

    // Backend URL
    let backend_url = format!(
        "{}/v1/chat/completions",
        state
            .docker
            .backend_base_url(&model.id, &model.backend_type),
    );

    let requested_model = parsed.model.clone();
    let is_streaming = parsed.stream;

    // Extract user_id from metadata for meta token resolution (usage attribution)
    let user_email_override: Option<String> =
        parsed.metadata.as_ref().and_then(|m| m.user_id.clone());

    // Meta token resolution: if this is an internal token and the request
    // includes a metadata.user_id email, attribute usage to the actual user.
    let (log_user_id, log_token_id) = if auth_user.is_internal {
        if let Some(ref email) = user_email_override {
            match tokens::resolve_meta_user(&state.db, email).await {
                Ok(Some(meta)) => (meta.user_id, meta.token_id),
                Ok(None) => {
                    warn!(email = %email, "Meta resolution: no user found for email");
                    (auth_user.user_id.clone(), auth_user.token_id.clone())
                }
                Err(e) => {
                    warn!(error = %e, email = %email, "Meta resolution: lookup failed");
                    (auth_user.user_id.clone(), auth_user.token_id.clone())
                }
            }
        } else {
            (auth_user.user_id.clone(), auth_user.token_id.clone())
        }
    } else {
        (auth_user.user_id.clone(), auth_user.token_id.clone())
    };

    if !is_streaming {
        // 8. NON-STREAMING: proxy via proxy_to_backend, then transform response
        let client = reqwest::Client::new();
        let result = proxy_to_backend(
            &client,
            &backend_url,
            openai_bytes,
            false,
            api_key.as_deref(),
            &model.id,
            &state.probe_tx,
        )
        .await;

        let latency_ms = start.elapsed().as_millis() as i64;

        let (input_tokens, output_tokens) = result
            .body_bytes
            .as_ref()
            .map(|b| extract_usage(b))
            .unwrap_or((0, 0));

        // Transform response body from OpenAI to Anthropic format
        let response = if let Some(ref body_bytes) = result.body_bytes {
            match serde_json::from_slice::<OpenAIResponse>(body_bytes) {
                Ok(openai_resp) => {
                    let anthropic_resp = translate_openai_response(&openai_resp, &requested_model);
                    (StatusCode::OK, Json(anthropic_resp)).into_response()
                }
                Err(_) => {
                    // Could not parse backend response; return an error wrapper
                    error!("Failed to parse OpenAI response from backend");
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "Backend returned an unparseable response".to_string(),
                    )
                }
            }
        } else {
            // No body bytes means proxy_to_backend returned an error response
            error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Backend unavailable".to_string(),
            )
        };

        // 9. Log usage (fire and forget)
        let db = state.db.clone();
        let model_id = model.id.clone();
        let category_id = model.category_id.clone();

        tokio::spawn(async move {
            let entry = usage::UsageEntry {
                token_id: &log_token_id,
                user_id: &log_user_id,
                model_id: &model_id,
                category_id: category_id.as_deref(),
                input_tokens,
                output_tokens,
                latency_ms,
                queued_ms,
            };
            if let Err(e) = usage::log_usage(&db, &entry).await {
                warn!(error = %e, "Failed to log usage");
            }
        });

        response
    } else {
        // 10. STREAMING: make the reqwest call directly, transform SSE stream
        let client = reqwest::Client::new();
        let mut request = client
            .post(&backend_url)
            .header("content-type", "application/json");

        if let Some(ref key) = api_key {
            request = request.header("authorization", format!("Bearer {}", key));
        }

        let backend_response = match request.body(openai_bytes).send().await {
            Ok(resp) => resp,
            Err(e) => {
                error!(error = %e, "Failed to connect to backend");
                // Phase 3: kick supervisor on connect-failure.
                let _ = state.probe_tx.try_send(crate::supervisor::ProbeKick {
                    model_id: model.id.clone(),
                    reason: crate::supervisor::ProbeReason::OnFailure,
                });
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Backend unavailable".to_string(),
                );
            }
        };

        if !backend_response.status().is_success() {
            let status = backend_response.status();
            // Phase 3: kick supervisor on 5xx response from backend.
            if status.is_server_error() {
                let _ = state.probe_tx.try_send(crate::supervisor::ProbeKick {
                    model_id: model.id.clone(),
                    reason: crate::supervisor::ProbeReason::OnFailure,
                });
            }
            let error_body = backend_response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown backend error".to_string());
            error!(status = %status, body = %error_body, "Backend returned error");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Backend error: {}", error_body),
            );
        }

        let msg_id = generate_message_id();
        let openai_stream = backend_response.bytes_stream();

        let (body, usage_accumulator) = transform_stream(openai_stream, requested_model, msg_id);

        // Log usage after stream completes
        let db = state.db.clone();
        let model_id = model.id.clone();
        let category_id = model.category_id.clone();
        let start_time = start;

        tokio::spawn(async move {
            // Wait for the stream to finish; the usage accumulator is
            // updated at the end of the stream. We poll with a timeout so we
            // don't block indefinitely.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let acc = usage_accumulator.lock().await;
                let (input_tokens, output_tokens) = *acc;
                if input_tokens > 0 || output_tokens > 0 || tokio::time::Instant::now() >= deadline
                {
                    let latency_ms = start_time.elapsed().as_millis() as i64;
                    let entry = usage::UsageEntry {
                        token_id: &log_token_id,
                        user_id: &log_user_id,
                        model_id: &model_id,
                        category_id: category_id.as_deref(),
                        input_tokens,
                        output_tokens,
                        latency_ms,
                        queued_ms,
                    };
                    if let Err(e) = usage::log_usage(&db, &entry).await {
                        warn!(error = %e, "Failed to log streaming usage");
                    }
                    break;
                }
                drop(acc);
            }
        });

        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(body)
            .unwrap()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // system_to_string
    // -----------------------------------------------------------------------

    #[test]
    fn system_string_passthrough() {
        let val = serde_json::json!("You are helpful");
        assert_eq!(system_to_string(&val), "You are helpful");
    }

    #[test]
    fn system_array_of_text_blocks() {
        let val = serde_json::json!([
            {"type": "text", "text": "First."},
            {"type": "text", "text": "Second."}
        ]);
        assert_eq!(system_to_string(&val), "First.\nSecond.");
    }

    #[test]
    fn system_null_returns_empty() {
        assert_eq!(system_to_string(&serde_json::Value::Null), "");
    }

    // -----------------------------------------------------------------------
    // content_to_string
    // -----------------------------------------------------------------------

    #[test]
    fn content_plain_string() {
        let val = serde_json::json!("hello");
        assert_eq!(content_to_string(&val), "hello");
    }

    #[test]
    fn content_array_text_blocks() {
        let val = serde_json::json!([
            {"type": "text", "text": "foo"},
            {"type": "image", "source": {}},
            {"type": "text", "text": "bar"},
        ]);
        assert_eq!(content_to_string(&val), "foobar");
    }

    // -----------------------------------------------------------------------
    // content_has_tool_use / content_has_tool_result
    // -----------------------------------------------------------------------

    #[test]
    fn detect_tool_use_in_content() {
        let val = serde_json::json!([
            {"type": "text", "text": "thinking..."},
            {"type": "tool_use", "id": "t1", "name": "fn", "input": {}}
        ]);
        assert!(content_has_tool_use(&val));
        assert!(!content_has_tool_result(&val));
    }

    #[test]
    fn detect_tool_result_in_content() {
        let val = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
        ]);
        assert!(!content_has_tool_use(&val));
        assert!(content_has_tool_result(&val));
    }

    #[test]
    fn plain_string_has_no_tool_blocks() {
        let val = serde_json::json!("just text");
        assert!(!content_has_tool_use(&val));
        assert!(!content_has_tool_result(&val));
    }

    // -----------------------------------------------------------------------
    // translate_stop_reason
    // -----------------------------------------------------------------------

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(translate_stop_reason(Some("stop")), "end_turn");
        assert_eq!(translate_stop_reason(Some("length")), "max_tokens");
        assert_eq!(translate_stop_reason(Some("tool_calls")), "tool_use");
        assert_eq!(translate_stop_reason(None), "end_turn");
        assert_eq!(translate_stop_reason(Some("other")), "other");
    }

    // -----------------------------------------------------------------------
    // generate_message_id / generate_tool_use_id
    // -----------------------------------------------------------------------

    #[test]
    fn message_id_format() {
        let id = generate_message_id();
        assert!(id.starts_with("msg_"), "expected msg_ prefix, got: {}", id);
        assert_eq!(id.len(), 4 + 24); // "msg_" + 24 hex chars
    }

    #[test]
    fn tool_use_id_format() {
        let id = generate_tool_use_id();
        assert!(
            id.starts_with("toolu_"),
            "expected toolu_ prefix, got: {}",
            id
        );
    }

    // -----------------------------------------------------------------------
    // translate_request — basic
    // -----------------------------------------------------------------------

    #[test]
    fn translate_basic_request() {
        let req = AnthropicRequest {
            model: "llama3.1:8b".to_string(),
            max_tokens: 256,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            system: Some(serde_json::json!("Be concise.")),
            stream: false,
            temperature: Some(0.5),
            top_p: None,
            stop_sequences: None,
            metadata: None,
            tools: None,
        };

        let openai = translate_request(&req);

        assert_eq!(openai["model"], "llama3.1:8b");
        assert_eq!(openai["max_tokens"], 256);
        assert_eq!(openai["temperature"], 0.5);
        assert_eq!(openai["stream"], false);

        let msgs = openai["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2); // system + user
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be concise.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hello");
    }

    #[test]
    fn translate_request_with_tools() {
        let req = AnthropicRequest {
            model: "test".to_string(),
            max_tokens: 100,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("What's the weather?"),
            }],
            system: None,
            stream: false,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            metadata: None,
            tools: Some(vec![AnthropicTool {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}}
                })),
            }]),
        };

        let openai = translate_request(&req);
        let tools = openai["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["description"], "Get weather");
        assert!(tools[0]["function"]["parameters"]["properties"]["location"].is_object());
    }

    #[test]
    fn translate_request_streaming_includes_usage_option() {
        let req = AnthropicRequest {
            model: "test".to_string(),
            max_tokens: 100,
            messages: vec![],
            system: None,
            stream: true,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            metadata: None,
            tools: None,
        };

        let openai = translate_request(&req);
        assert_eq!(openai["stream"], true);
        assert_eq!(openai["stream_options"]["include_usage"], true);
    }

    // -----------------------------------------------------------------------
    // translate_request — tool_use / tool_result messages
    // -----------------------------------------------------------------------

    #[test]
    fn translate_assistant_tool_use_message() {
        let req = AnthropicRequest {
            model: "test".to_string(),
            max_tokens: 100,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("What's the weather in SF?"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "Let me check."},
                        {"type": "tool_use", "id": "toolu_abc", "name": "get_weather", "input": {"location": "SF"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_abc", "content": "72F sunny"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            metadata: None,
            tools: None,
        };

        let openai = translate_request(&req);
        let msgs = openai["messages"].as_array().unwrap();

        // msg 0: user
        assert_eq!(msgs[0]["role"], "user");

        // msg 1: assistant with tool_calls
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "Let me check.");
        let tool_calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "toolu_abc");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");

        // msg 2: tool result
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "toolu_abc");
        assert_eq!(msgs[2]["content"], "72F sunny");
    }

    // -----------------------------------------------------------------------
    // translate_openai_response
    // -----------------------------------------------------------------------

    #[test]
    fn translate_simple_response() {
        let openai_resp = OpenAIResponse {
            choices: Some(vec![OpenAIChoice {
                message: Some(OpenAIMessage {
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                }),
                finish_reason: Some("stop".to_string()),
            }]),
            usage: Some(OpenAIUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
            }),
        };

        let resp = translate_openai_response(&openai_resp, "llama3.1:8b");
        assert_eq!(resp.response_type, "message");
        assert_eq!(resp.role, "assistant");
        assert_eq!(resp.model, "llama3.1:8b");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);

        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.content[0]["type"], "text");
        assert_eq!(resp.content[0]["text"], "Hello!");
    }

    #[test]
    fn translate_response_with_tool_calls() {
        let openai_resp = OpenAIResponse {
            choices: Some(vec![OpenAIChoice {
                message: Some(OpenAIMessage {
                    content: None,
                    tool_calls: Some(vec![OpenAIToolCall {
                        id: Some("call_123".to_string()),
                        function: Some(OpenAIToolCallFunction {
                            name: Some("get_weather".to_string()),
                            arguments: Some("{\"location\":\"SF\"}".to_string()),
                        }),
                    }]),
                }),
                finish_reason: Some("tool_calls".to_string()),
            }]),
            usage: Some(OpenAIUsage {
                prompt_tokens: Some(20),
                completion_tokens: Some(10),
            }),
        };

        let resp = translate_openai_response(&openai_resp, "test");
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.content[0]["type"], "tool_use");
        assert_eq!(resp.content[0]["name"], "get_weather");
        assert_eq!(resp.content[0]["input"]["location"], "SF");
    }

    #[test]
    fn translate_empty_response_gets_empty_text_block() {
        let openai_resp = OpenAIResponse {
            choices: Some(vec![OpenAIChoice {
                message: Some(OpenAIMessage {
                    content: Some(String::new()),
                    tool_calls: None,
                }),
                finish_reason: Some("stop".to_string()),
            }]),
            usage: None,
        };

        let resp = translate_openai_response(&openai_resp, "test");
        // Should have at least one content block (empty text fallback)
        assert!(!resp.content.is_empty());
    }

    // -----------------------------------------------------------------------
    // sse_event formatting
    // -----------------------------------------------------------------------

    #[test]
    fn sse_event_format() {
        let data = serde_json::json!({"type": "ping"});
        let output = sse_event("ping", &data);
        assert!(output.starts_with("event: ping\n"));
        assert!(output.contains("data: "));
        assert!(output.ends_with("\n\n"));
    }

    // -----------------------------------------------------------------------
    // Integration tests against Ollama (requires network)
    // -----------------------------------------------------------------------

    /// Integration test: translate an Anthropic request, send to Ollama,
    /// translate the response back, and validate the Anthropic format.
    ///
    /// Run with: cargo test -p sovereign-engine ollama_non_streaming -- --ignored
    #[tokio::test]
    #[ignore = "requires Ollama at 10.24.0.200:11434"]
    async fn ollama_non_streaming_roundtrip() {
        let req = AnthropicRequest {
            model: "llama3.1:8b".to_string(),
            max_tokens: 64,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Say hello in exactly 3 words."),
            }],
            system: Some(serde_json::json!("You are a concise assistant.")),
            stream: false,
            temperature: Some(0.1),
            top_p: None,
            stop_sequences: None,
            metadata: None,
            tools: None,
        };

        // Translate to OpenAI format
        let openai_body = translate_request(&req);
        println!(
            "OpenAI request: {}",
            serde_json::to_string_pretty(&openai_body).unwrap()
        );

        // Send to Ollama
        let client = reqwest::Client::new();
        let resp = client
            .post("http://10.24.0.200:11434/v1/chat/completions")
            .json(&openai_body)
            .send()
            .await
            .expect("Failed to connect to Ollama");

        assert!(
            resp.status().is_success(),
            "Ollama returned {}",
            resp.status()
        );

        let body_bytes = resp.bytes().await.unwrap();
        println!("OpenAI response: {}", String::from_utf8_lossy(&body_bytes));

        // Parse and translate back
        let openai_resp: OpenAIResponse =
            serde_json::from_slice(&body_bytes).expect("Failed to parse OpenAI response");
        let anthropic_resp = translate_openai_response(&openai_resp, &req.model);

        println!(
            "Anthropic response: {}",
            serde_json::to_string_pretty(&anthropic_resp).unwrap()
        );

        // Validate Anthropic format
        assert_eq!(anthropic_resp.response_type, "message");
        assert_eq!(anthropic_resp.role, "assistant");
        assert_eq!(anthropic_resp.model, "llama3.1:8b");
        assert!(anthropic_resp.id.starts_with("msg_"));
        assert!(anthropic_resp.stop_reason.is_some());
        assert!(!anthropic_resp.content.is_empty());
        assert_eq!(anthropic_resp.content[0]["type"], "text");
        assert!(
            !anthropic_resp.content[0]["text"]
                .as_str()
                .unwrap()
                .is_empty(),
            "Expected non-empty text"
        );
        assert!(anthropic_resp.usage.input_tokens > 0 || anthropic_resp.usage.output_tokens > 0);

        println!("✓ Non-streaming roundtrip passed");
    }

    /// Integration test: streaming translation against Ollama.
    ///
    /// Run with: cargo test -p sovereign-engine ollama_streaming -- --ignored
    #[tokio::test]
    #[ignore = "requires Ollama at 10.24.0.200:11434"]
    async fn ollama_streaming_roundtrip() {
        let req = AnthropicRequest {
            model: "llama3.1:8b".to_string(),
            max_tokens: 64,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Count from 1 to 5."),
            }],
            system: None,
            stream: true,
            temperature: Some(0.1),
            top_p: None,
            stop_sequences: None,
            metadata: None,
            tools: None,
        };

        let openai_body = translate_request(&req);

        // Send streaming request to Ollama
        let client = reqwest::Client::new();
        let resp = client
            .post("http://10.24.0.200:11434/v1/chat/completions")
            .json(&openai_body)
            .send()
            .await
            .expect("Failed to connect to Ollama");

        assert!(
            resp.status().is_success(),
            "Ollama returned {}",
            resp.status()
        );

        let msg_id = generate_message_id();
        let (body, usage_acc) =
            transform_stream(resp.bytes_stream(), "llama3.1:8b".to_string(), msg_id);

        // Collect all SSE events from the body stream
        use http_body_util::BodyExt;
        let collected = body.collect().await.expect("failed to collect body");
        let events = String::from_utf8_lossy(&collected.to_bytes()).to_string();

        println!("--- Raw SSE events ---\n{events}--- End ---");

        // Validate event structure
        assert!(
            events.contains("event: message_start"),
            "Missing message_start"
        );
        assert!(
            events.contains("event: content_block_start"),
            "Missing content_block_start"
        );
        assert!(
            events.contains("event: content_block_delta"),
            "Missing content_block_delta"
        );
        assert!(
            events.contains("event: content_block_stop"),
            "Missing content_block_stop"
        );
        assert!(
            events.contains("event: message_delta"),
            "Missing message_delta"
        );
        assert!(
            events.contains("event: message_stop"),
            "Missing message_stop"
        );
        assert!(events.contains("event: ping"), "Missing ping");
        assert!(
            events.contains("\"type\":\"text_delta\""),
            "Missing text_delta in deltas"
        );

        // Check usage was accumulated
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (input, output) = *usage_acc.lock().await;
        println!("Usage: input_tokens={input}, output_tokens={output}");
        if input > 0 || output > 0 {
            println!("✓ Usage tokens captured");
        } else {
            println!(
                "⚠ No usage tokens in streaming response (Ollama may not support stream_options)"
            );
        }

        println!("✓ Streaming roundtrip passed");
    }
}
