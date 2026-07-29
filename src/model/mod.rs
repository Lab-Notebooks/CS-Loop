//! Model-facing interface shared by all backends. Anthropic is the only implementation
//! (see `anthropic.rs`) — this crate is deliberately single-backend.

pub mod anthropic;

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Set only when `arguments` failed to parse as JSON — the raw accumulated text.
    pub raw_arguments: Option<String>,
    pub raw_arguments_error: Option<String>,
}

pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// Raw provider usage payload; normalized later via `agent::TokenUsage::from_raw`.
    pub usage: Option<serde_json::Value>,
    pub reasoning: String,
    /// Wire-format thinking blocks, echoed back verbatim on the next turn.
    pub reasoning_blocks: Vec<serde_json::Value>,
}

pub trait Model {
    fn chat_with_tools(
        &self,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<ChatResponse>;

    /// Build the assistant/tool-result message pair(s) to append to history after
    /// executing one iteration's tool calls.
    fn format_tool_result_messages(
        &self,
        calls: &[ToolCall],
        outputs: &[String],
        reasoning_blocks: &[serde_json::Value],
    ) -> Vec<serde_json::Value>;
}
