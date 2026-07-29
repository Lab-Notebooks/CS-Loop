//! Anthropic Messages API client: request building (prompt caching, extended thinking),
//! streaming (SSE) and non-streaming response handling, retry/backoff.
//!
//! Ports `codescribe/lib/_llm.py::AnthropicModel` (the OpenAI-compatible backend in that
//! file is out of scope — csloop is Anthropic-only).

use std::io::BufRead;
use std::time::Duration;

use super::{ChatResponse, Model, ToolCall};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const CACHE_BETA_HEADER: &str = "extended-cache-ttl-2025-04-11";

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => default,
    }
}

pub struct AnthropicModel {
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: u64,
    streaming: bool,
    prompt_caching: bool,
    thinking: Option<serde_json::Value>,
}

impl AnthropicModel {
    pub fn new(model: String, reasoning: bool) -> anyhow::Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY environment variable is not set"))?;
        let base_url =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        let max_tokens: u64 = std::env::var("CSLOOP_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(32768);
        let streaming = env_flag("CSLOOP_ANTHROPIC_STREAMING", true);
        let prompt_caching = env_flag("CSLOOP_PROMPT_CACHE", true);
        let reasoning_enabled = reasoning || env_flag("CSLOOP_MODEL_REASONING", false);
        let thinking =
            reasoning_enabled.then(|| serde_json::json!({"type": "adaptive", "display": "summarized"}));

        Ok(Self { api_key, base_url, model, max_tokens, streaming, prompt_caching, thinking })
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    /// Build the Anthropic request body. Cache-control blocks include `"ttl":"1h"`
    /// (deliberate fix vs. the Python reference, which sends the
    /// `extended-cache-ttl-2025-04-11` beta header but never sets a matching `ttl` on
    /// any cache_control block, so the header there has no effect).
    fn request_body(&self, messages: &[serde_json::Value], tools: Option<&[serde_json::Value]>) -> serde_json::Value {
        let mut system_parts: Vec<String> = Vec::new();
        let mut msgs: Vec<serde_json::Value> = Vec::new();
        for m in messages {
            if m.get("role").and_then(|v| v.as_str()) == Some("system") {
                if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                    system_parts.push(c.to_string());
                }
            } else {
                msgs.push(m.clone());
            }
        }

        // cache_control goes on the SECOND-TO-LAST user message, not the last — the
        // most recent, still-mutating turn is deliberately left uncached.
        if self.prompt_caching && msgs.len() >= 2 {
            let mut n_user = 0;
            for i in (0..msgs.len()).rev() {
                if msgs[i].get("role").and_then(|v| v.as_str()) == Some("user") {
                    n_user += 1;
                    if n_user == 2 {
                        add_cache_control_to_message(&mut msgs[i]);
                        break;
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": msgs,
        });

        if !system_parts.is_empty() {
            if self.prompt_caching {
                let mut blocks = vec![serde_json::json!({
                    "type": "text",
                    "text": system_parts[0],
                    "cache_control": cache_control_ephemeral(),
                })];
                for p in &system_parts[1..] {
                    blocks.push(serde_json::json!({"type": "text", "text": p}));
                }
                body["system"] = serde_json::Value::Array(blocks);
            } else {
                body["system"] = serde_json::Value::String(system_parts.join("\n\n"));
            }
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                let mut anthropic_tools: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| {
                        let f = t.get("function").cloned().unwrap_or_default();
                        serde_json::json!({
                            "name": f.get("name").cloned().unwrap_or_default(),
                            "description": f.get("description").cloned()
                                .unwrap_or_else(|| serde_json::Value::String(String::new())),
                            "input_schema": f.get("parameters").cloned()
                                .unwrap_or_else(|| serde_json::json!({})),
                        })
                    })
                    .collect();
                // Only the LAST tool schema gets cache_control, not each one.
                if self.prompt_caching {
                    if let Some(last) = anthropic_tools.last_mut() {
                        last["cache_control"] = cache_control_ephemeral();
                    }
                }
                body["tools"] = serde_json::Value::Array(anthropic_tools);
            }
        }

        if let Some(thinking) = &self.thinking {
            body["thinking"] = thinking.clone();
        }

        body
    }

    fn send_streaming(&self, body: &serde_json::Value) -> anyhow::Result<ChatResponse> {
        let mut req = ureq::post(self.messages_url().as_str())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if self.prompt_caching {
            req = req.header("anthropic-beta", CACHE_BETA_HEADER);
        }
        // The API only emits an SSE stream when explicitly asked; without this the
        // response is a single complete JSON object, which the SSE line-parser below
        // would silently fail to find any "data:" lines in (zero events, empty result).
        let mut streaming_body = body.clone();
        streaming_body["stream"] = serde_json::Value::Bool(true);
        let mut resp = req.send_json(&streaming_body)?;
        let reader = resp.body_mut().as_reader();
        let mut buf_reader = std::io::BufReader::new(reader);

        let mut acc = StreamAccumulator::default();
        let mut data_lines: Vec<String> = Vec::new();

        loop {
            let mut line = String::new();
            let n = buf_reader.read_line(&mut line)?;
            if n == 0 {
                if !data_lines.is_empty() {
                    let evt: serde_json::Value = serde_json::from_str(&data_lines.join("\n"))?;
                    acc.handle_event(&evt)?;
                }
                break;
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                if !data_lines.is_empty() {
                    let evt: serde_json::Value = serde_json::from_str(&data_lines.join("\n"))?;
                    acc.handle_event(&evt)?;
                    data_lines.clear();
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("data:") {
                data_lines.push(rest.trim_start().to_string());
            }
            // "event:"/"id:"/"retry:"/":"-comment lines are ignored — every data payload
            // carries its own "type" field, which is authoritative.
        }

        Ok(acc.finish())
    }

    fn send_once(&self, body: &serde_json::Value) -> anyhow::Result<ChatResponse> {
        let mut req = ureq::post(self.messages_url().as_str())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if self.prompt_caching {
            req = req.header("anthropic-beta", CACHE_BETA_HEADER);
        }
        let mut resp = req.send_json(body)?;
        let text = resp.body_mut().read_to_string()?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        Ok(finalize_non_streaming(&value))
    }
}

fn cache_control_ephemeral() -> serde_json::Value {
    serde_json::json!({"type": "ephemeral", "ttl": "1h"})
}

fn add_cache_control_to_message(msg: &mut serde_json::Value) {
    let Some(content) = msg.get("content").cloned() else { return };
    match content {
        serde_json::Value::String(s) => {
            msg["content"] = serde_json::json!([
                {"type": "text", "text": s, "cache_control": cache_control_ephemeral()}
            ]);
        }
        serde_json::Value::Array(mut blocks) => {
            if let Some(last) = blocks.last_mut() {
                if last.get("cache_control").is_none() {
                    if let Some(obj) = last.as_object_mut() {
                        obj.insert("cache_control".to_string(), cache_control_ephemeral());
                    }
                }
            }
            msg["content"] = serde_json::Value::Array(blocks);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Non-streaming response parsing
// ---------------------------------------------------------------------------

fn finalize_non_streaming(value: &serde_json::Value) -> ChatResponse {
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut reasoning_parts = Vec::new();
    let mut reasoning_blocks = Vec::new();

    if let Some(content) = value.get("content").and_then(|c| c.as_array()) {
        for block in content {
            match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        texts.push(t.to_string());
                    }
                }
                "tool_use" => {
                    tool_calls.push(ToolCall {
                        id: block.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        name: block.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        arguments: block.get("input").cloned().unwrap_or_else(|| serde_json::json!({})),
                        raw_arguments: None,
                        raw_arguments_error: None,
                    });
                }
                "thinking" => {
                    let t = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    reasoning_parts.push(t.clone());
                    let mut rb = serde_json::json!({"type": "thinking", "thinking": t});
                    if let Some(sig) = block.get("signature").and_then(|v| v.as_str()) {
                        rb["signature"] = serde_json::Value::String(sig.to_string());
                    }
                    reasoning_blocks.push(rb);
                }
                _ => {}
            }
        }
    }

    ChatResponse {
        text: texts.join("\n").trim().to_string(),
        tool_calls,
        usage: value.get("usage").cloned(),
        reasoning: reasoning_parts.join("\n\n").trim().to_string(),
        reasoning_blocks,
    }
}

// ---------------------------------------------------------------------------
// Streaming (SSE) accumulator
// ---------------------------------------------------------------------------

enum ContentBlockAcc {
    Text { text: String },
    ToolUse { id: String, name: String, partial_json: String },
    Thinking { thinking: String, signature: Option<String> },
    Other,
}

#[derive(Default)]
struct StreamAccumulator {
    blocks: Vec<ContentBlockAcc>,
    message_usage: Option<serde_json::Value>,
    delta_output_tokens: Option<u64>,
}

/// Generous upper bound on concurrent content blocks in one response. Anthropic
/// responses realistically use single digits to low hundreds of blocks; this exists
/// purely as a defensive cap so a malformed or malicious response (e.g. a compromised
/// `ANTHROPIC_BASE_URL` proxy) can't force an unbounded `Vec` allocation via a bogus
/// huge `index` field.
const MAX_CONTENT_BLOCKS: usize = 4096;

impl StreamAccumulator {
    fn ensure_index(&mut self, idx: usize) -> anyhow::Result<()> {
        if idx >= MAX_CONTENT_BLOCKS {
            anyhow::bail!("Anthropic stream content_block index {idx} exceeds the maximum of {MAX_CONTENT_BLOCKS}");
        }
        while self.blocks.len() <= idx {
            self.blocks.push(ContentBlockAcc::Other);
        }
        Ok(())
    }

    fn handle_event(&mut self, evt: &serde_json::Value) -> anyhow::Result<()> {
        match evt.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message_start" => {
                self.message_usage = evt.get("message").and_then(|m| m.get("usage")).cloned();
            }
            "content_block_start" => {
                let idx = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let cb = evt.get("content_block").cloned().unwrap_or_default();
                let block = match cb.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" => ContentBlockAcc::Text { text: String::new() },
                    "tool_use" => ContentBlockAcc::ToolUse {
                        id: cb.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        name: cb.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        partial_json: String::new(),
                    },
                    "thinking" => ContentBlockAcc::Thinking { thinking: String::new(), signature: None },
                    _ => ContentBlockAcc::Other,
                };
                self.ensure_index(idx)?;
                self.blocks[idx] = block;
            }
            "content_block_delta" => {
                let idx = evt.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                self.ensure_index(idx)?;
                let delta = evt.get("delta").cloned().unwrap_or_default();
                let dty = delta.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                match (&mut self.blocks[idx], dty.as_str()) {
                    (ContentBlockAcc::Text { text }, "text_delta") => {
                        text.push_str(delta.get("text").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    (ContentBlockAcc::ToolUse { partial_json, .. }, "input_json_delta") => {
                        partial_json.push_str(delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    (ContentBlockAcc::Thinking { thinking, .. }, "thinking_delta") => {
                        thinking.push_str(delta.get("thinking").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    (ContentBlockAcc::Thinking { signature, .. }, "signature_delta") => {
                        let sig = delta.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                        let combined = match signature.take() {
                            Some(mut existing) => {
                                existing.push_str(sig);
                                existing
                            }
                            None => sig.to_string(),
                        };
                        *signature = Some(combined);
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(ot) = evt.get("usage").and_then(|u| u.get("output_tokens")).and_then(|v| v.as_u64()) {
                    self.delta_output_tokens = Some(ot);
                }
            }
            "content_block_stop" | "message_stop" | "ping" => {}
            "error" => {
                anyhow::bail!(
                    "Anthropic stream error: {}",
                    evt.get("error").cloned().unwrap_or_default()
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> ChatResponse {
        let mut texts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut reasoning_parts = Vec::new();
        let mut reasoning_blocks = Vec::new();

        for block in self.blocks {
            match block {
                ContentBlockAcc::Text { text } => {
                    if !text.is_empty() {
                        texts.push(text);
                    }
                }
                ContentBlockAcc::ToolUse { id, name, partial_json } => {
                    let (arguments, raw_arguments, raw_arguments_error) =
                        match serde_json::from_str::<serde_json::Value>(&partial_json) {
                            Ok(v) => (v, None, None),
                            Err(e) => (serde_json::json!({}), Some(partial_json), Some(e.to_string())),
                        };
                    tool_calls.push(ToolCall { id, name, arguments, raw_arguments, raw_arguments_error });
                }
                ContentBlockAcc::Thinking { thinking, signature } => {
                    reasoning_parts.push(thinking.clone());
                    let mut rb = serde_json::json!({"type": "thinking", "thinking": thinking});
                    if let Some(sig) = signature {
                        rb["signature"] = serde_json::Value::String(sig);
                    }
                    reasoning_blocks.push(rb);
                }
                ContentBlockAcc::Other => {}
            }
        }

        ChatResponse {
            text: texts.join("\n").trim().to_string(),
            tool_calls,
            usage: merge_stream_usage(self.message_usage.as_ref(), self.delta_output_tokens),
            reasoning: reasoning_parts.join("\n\n").trim().to_string(),
            reasoning_blocks,
        }
    }
}

fn merge_stream_usage(
    start_usage: Option<&serde_json::Value>,
    delta_output_tokens: Option<u64>,
) -> Option<serde_json::Value> {
    let mut out = serde_json::Map::new();
    if let Some(u) = start_usage {
        for key in ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"] {
            if let Some(v) = u.get(key) {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    if let Some(ot) = delta_output_tokens {
        out.insert("output_tokens".to_string(), serde_json::json!(ot));
    }
    if out.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(out))
    }
}

// ---------------------------------------------------------------------------
// Retry/backoff
//
// The Python `anthropic` SDK auto-retries 408/409/429/5xx and connection errors with
// backoff by default; a raw `ureq` call gets none of that for free, so we replicate a
// thin version here to avoid a reliability regression in long agent loops.
// ---------------------------------------------------------------------------

fn should_retry(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(code)) => *code == 429 || (500..=599).contains(code),
        Some(ureq::Error::Io(_)) => true,
        Some(ureq::Error::Timeout(_)) => true,
        Some(ureq::Error::ConnectionFailed) => true,
        Some(ureq::Error::HostNotFound) => true,
        _ => false,
    }
}

const MAX_RETRIES: u32 = 2;

fn with_retry<F>(mut f: F) -> anyhow::Result<ChatResponse>
where
    F: FnMut() -> anyhow::Result<ChatResponse>,
{
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(r) => return Ok(r),
            Err(e) if attempt < MAX_RETRIES && should_retry(&e) => {
                attempt += 1;
                let backoff_ms = 500u64 * (1u64 << (attempt - 1));
                std::thread::sleep(Duration::from_millis(backoff_ms));
            }
            Err(e) => return Err(e),
        }
    }
}

impl Model for AnthropicModel {
    fn chat_with_tools(
        &self,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<ChatResponse> {
        let body = self.request_body(messages, Some(tools));
        with_retry(|| {
            if self.streaming {
                self.send_streaming(&body)
            } else {
                self.send_once(&body)
            }
        })
    }

    fn format_tool_result_messages(
        &self,
        calls: &[ToolCall],
        outputs: &[String],
        reasoning_blocks: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        let mut assistant_content: Vec<serde_json::Value> = reasoning_blocks.to_vec();
        for call in calls {
            assistant_content.push(serde_json::json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.arguments,
            }));
        }

        let user_content: Vec<serde_json::Value> = calls
            .iter()
            .zip(outputs.iter())
            .map(|(call, output)| {
                serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": call.id,
                    "content": output,
                })
            })
            .collect();

        vec![
            serde_json::json!({"role": "assistant", "content": assistant_content}),
            serde_json::json!({"role": "user", "content": user_content}),
        ]
    }
}
