// Copyright (c) 2026 UChicago Argonne LLC
// CS-Loop (SF-26-122)
// SPDX-License-Identifier: GPL-3.0-only
// Full license and notices: see LICENSE and NOTICE in the repo root.

//! Standalone coding agent with a native tool-calling loop.
//!
//! Ports `codescribe/lib/_agent.py` (minus the `RunObserver`/`ConsoleObserver`
//! presentation types, which live in `observer.rs`).

use std::collections::HashMap;

use crate::logging::{Timer, ToolLogSink};
use crate::model::{ChatResponse, Model, ToolCall};
use crate::observer::RunObserver;
use crate::tools::AgentTool;

const REACT_NUDGE: &str = "\
You are a coding agent with access to tools.

Available tools (high level):
- read: read file contents
- glob: find files by pattern
- bash: run shell commands
- edit: exact text replacements in a file
- write: create/overwrite a file

Rules:
- Be concise and practical.
- Use tools whenever you need to inspect files, run commands, or change the filesystem.
- Do NOT fabricate tool outputs. If you need info, call a tool.
- Batch ALL independent reads and globs into a single turn before acting — gather everything you need first, then implement.
- Once you have the information needed, implement immediately without further exploration.
- Do not re-read files you have already read unless they were modified since your last read.
- Prefer one comprehensive edit over multiple small edits to the same file.
- IMPORTANT: Only use the tools listed here. For shell work, ONLY use the bash tool and ONLY run commands that succeed under the bash tool's safety policy. If a command is blocked, pick an allowed alternative.
- Avoid repeating identical tool calls with the same arguments unless the workspace changed (e.g., after an edit).
- Before using edit, ensure you have read the exact file region you are changing.
- When all required actions are complete, respond with the final answer immediately — do not do additional cleanup or exploration.
";

// ---------------------------------------------------------------------------
// Value objects
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct AgentPolicy {
    pub max_tool_calls_total: u32,
    pub max_calls_per_iteration: u32,
    pub max_repeated_calls: u32,
    /// Reads get more repeats for paging and post-edit verification.
    pub read_repeat_multiplier: u32,
    pub max_consecutive_error_iters: u32,
    pub max_history_chars: usize,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            max_tool_calls_total: 120,
            max_calls_per_iteration: 10,
            max_repeated_calls: 2,
            read_repeat_multiplier: 3,
            max_consecutive_error_iters: 3,
            max_history_chars: 8000,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    /// Build from a provider usage JSON payload. Both Anthropic-shaped (`input_tokens`/
    /// `output_tokens`) and OpenAI-shaped (`prompt_tokens`/`completion_tokens`) keys are
    /// recognized, even though csloop only ever talks to Anthropic — kept for parity
    /// with the Python source and in case another provider is wired in later.
    pub fn from_raw(usage: Option<&serde_json::Value>) -> Self {
        let Some(u) = usage else { return Self::default() };
        let get = |key: &str| u.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        let input = u
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| u.get("input_tokens").and_then(|v| v.as_u64()))
            .unwrap_or(0);
        let output = u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| u.get("output_tokens").and_then(|v| v.as_u64()))
            .unwrap_or(0);
        Self {
            input,
            output,
            reasoning: get("reasoning_tokens"),
            cache_write: get("cache_creation_input_tokens"),
            cache_read: get("cache_read_input_tokens"),
        }
    }
}

impl std::ops::Add for TokenUsage {
    type Output = TokenUsage;
    fn add(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            input: self.input + other.input,
            output: self.output + other.output,
            reasoning: self.reasoning + other.reasoning,
            cache_write: self.cache_write + other.cache_write,
            cache_read: self.cache_read + other.cache_read,
        }
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, other: TokenUsage) {
        *self = *self + other;
    }
}

/// One executed tool call surfaced through `RunResult`. Only real executions are
/// recorded here — blocked-repeat and unparseable-args calls are not — so downstream
/// summaries reflect actual workspace actions.
pub struct ToolResult {
    pub name: String,
    pub args: serde_json::Value,
    pub ok: bool,
    pub output_preview: String,
}

#[derive(Clone, Copy)]
pub enum RejectReason {
    RepeatBlocked,
    BadJson,
    IterationSkip,
}

impl RejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::RepeatBlocked => "repeat_blocked",
            RejectReason::BadJson => "bad_json",
            RejectReason::IterationSkip => "iteration_skip",
        }
    }
}

/// A tool call the harness refused to execute — never touched the workspace, so it's
/// kept out of `tool_results`.
pub struct RejectedCall {
    pub name: String,
    pub args: serde_json::Value,
    pub reason: RejectReason,
}

/// Normalized per-iteration telemetry, collected for downstream loop analysis. Not
/// currently persisted anywhere (forward-compat data, kept for parity with the
/// Python source's `IterationTelemetry`).
#[allow(dead_code)]
pub struct IterationTelemetry {
    pub iteration: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub tool_calls_requested: u32,
    pub had_final_text: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    FinalText,
    MaxIterations,
    ToolBudget,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::FinalText => "final_text",
            StopReason::MaxIterations => "max_iterations",
            StopReason::ToolBudget => "tool_budget",
        }
    }
}

pub struct RunResult {
    pub final_text: Option<String>,
    pub stop_reason: String,
    pub usage: TokenUsage,
    pub iterations: u32,
    pub tool_results: Vec<ToolResult>,
    pub rejected_calls: Vec<RejectedCall>,
    #[allow(dead_code)]
    pub iteration_telemetry: Vec<IterationTelemetry>,
}

impl std::fmt::Display for RunResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(text) = &self.final_text {
            write!(f, "{text}")
        } else if self.stop_reason == "tool_budget" {
            write!(f, "[Agent stopped: tool-call budget reached without a final answer]")
        } else {
            write!(f, "[Agent stopped: {} reached without a final answer]", self.stop_reason)
        }
    }
}

#[derive(Default)]
struct RecentEntry {
    tool: String,
    args_preview: String,
    summary: String,
    // Set (matching Python's `rec["ok"]`) but not read by `workspace_context_block` in
    // either implementation — kept for structural parity.
    #[allow(dead_code)]
    ok: bool,
}

#[derive(Default)]
struct RunState {
    tool_calls_total: u32,
    call_counts: HashMap<String, u32>,
    recent: Vec<RecentEntry>,
    recent_errors: Vec<String>,
    consecutive_error_iters: u32,
    tool_results: Vec<ToolResult>,
    rejected_calls: Vec<RejectedCall>,
    usage: TokenUsage,
    iteration_telemetry: Vec<IterationTelemetry>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn truncate_with_ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let truncated: String = s.chars().take(n).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

pub fn is_error_output(output: &str) -> bool {
    output.trim_start().starts_with("Error:")
}

fn parse_bash_exit_code(output: &str) -> Option<i64> {
    let first = output.lines().next().unwrap_or("").trim();
    first.strip_prefix("exit_code:").and_then(|rest| rest.trim().parse::<i64>().ok())
}

fn last_n<'a>(lines: &[&'a str], n: usize) -> Vec<&'a str> {
    if lines.len() > n {
        lines[lines.len() - n..].to_vec()
    } else {
        lines.to_vec()
    }
}

/// Summary text used only for the workspace-context block (the Python original also
/// returns an `attach_hint` bool that every call site discards — dropped here).
pub(crate) fn summarize_tool_output(name: &str, output: &str) -> String {
    let out_s = output.trim();

    if is_error_output(out_s) {
        let mut msg = out_s[6..].trim().to_string();
        if msg.chars().count() > 400 {
            msg = truncate_with_ellipsis(&msg, 400);
        }
        return format!("Error: {msg}");
    }

    if name == "bash" {
        let code = parse_bash_exit_code(output);
        let all_lines: Vec<&str> = output.lines().collect();
        let lines: Vec<&str> = all_lines
            .iter()
            .skip(1)
            .copied()
            .filter(|l| !l.is_empty() && *l != "STDOUT:" && *l != "STDERR:")
            .collect();
        let head: Vec<&str> = lines.iter().take(12).copied().collect();
        let tail: Vec<&str> = if lines.len() > 24 { last_n(&lines, 12) } else { Vec::new() };
        let mut body: Vec<&str> = head;
        if !tail.is_empty() {
            body.push("…(snip)…");
            body.extend(tail);
        }
        let body_txt = body.join("\n");
        let body_txt = if body_txt.trim().is_empty() { "(no output)".to_string() } else { body_txt.trim().to_string() };
        return match code {
            Some(c) => format!("bash exit_code={c}\n{body_txt}"),
            None => format!("bash exit_code=?\n{body_txt}"),
        };
    }

    if out_s.is_empty() {
        return "(empty)".to_string();
    }

    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= 24 && output.chars().count() <= 1500 {
        return out_s.to_string();
    }

    let head: Vec<&str> = lines.iter().take(12).copied().collect();
    let tail = last_n(&lines, 12);
    let omitted = lines.len().saturating_sub(head.len() + tail.len());
    let mut summary = head.join("\n");
    if omitted > 0 {
        summary.push_str(&format!("\n…[truncated {omitted} lines]…\n"));
        summary.push_str(&tail.join("\n"));
    }
    summary.trim().to_string()
}

fn validate_schema_value(schema: &serde_json::Value, value: &serde_json::Value, path: &str) -> Option<String> {
    match schema.get("type").and_then(|v| v.as_str()) {
        Some("object") => {
            let Some(obj) = value.as_object() else {
                return Some(format!("{path} must be an object"));
            };
            let empty = serde_json::Map::new();
            let props = schema.get("properties").and_then(|v| v.as_object()).unwrap_or(&empty);
            if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
                for key in required {
                    if let Some(k) = key.as_str() {
                        if !obj.contains_key(k) {
                            return Some(format!("missing required argument {k:?}"));
                        }
                    }
                }
            }
            if schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false)) {
                let extras: Vec<String> =
                    obj.keys().filter(|k| !props.contains_key(k.as_str())).map(|k| format!("{k:?}")).collect();
                if !extras.is_empty() {
                    return Some(format!("unexpected argument(s): {}", extras.join(", ")));
                }
            }
            for (key, item) in obj {
                if let Some(subschema) = props.get(key) {
                    if let Some(err) = validate_schema_value(subschema, item, &format!("{path}.{key}")) {
                        return Some(err);
                    }
                }
            }
            None
        }
        Some("array") => {
            let Some(arr) = value.as_array() else {
                return Some(format!("{path} must be an array"));
            };
            if let Some(min_items) = schema.get("minItems").and_then(|v| v.as_u64()) {
                if (arr.len() as u64) < min_items {
                    return Some(format!("{path} must contain at least {min_items} item(s)"));
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (idx, item) in arr.iter().enumerate() {
                    if let Some(err) = validate_schema_value(item_schema, item, &format!("{path}[{idx}]")) {
                        return Some(err);
                    }
                }
            }
            None
        }
        Some("string") => {
            if value.as_str().is_none() {
                Some(format!("{path} must be a string"))
            } else {
                None
            }
        }
        Some("integer") => {
            if !value.is_i64() && !value.is_u64() {
                return Some(format!("{path} must be an integer"));
            }
            if let Some(minimum) = schema.get("minimum").and_then(|v| v.as_i64()) {
                if value.as_i64().unwrap_or(i64::MIN) < minimum {
                    return Some(format!("{path} must be >= {minimum}"));
                }
            }
            None
        }
        _ => None,
    }
}

fn tool_call_key(name: &str, args: &serde_json::Value) -> String {
    // Relies on serde_json's default (non-`preserve_order`) sorted-key object
    // serialization to match Python's `json.dumps(args, sort_keys=True)`. If the
    // `preserve_order` feature is ever enabled, this key stops being stable and the
    // repeat-call detection above silently breaks.
    let json = serde_json::to_string(args).unwrap_or_else(|_| "null".to_string());
    format!("{name}:{json}")
}

fn workspace_context_block(state: &RunStateView, iteration: u32, max_iterations: u32, policy: &AgentPolicy) -> String {
    let mut lines = vec!["WORKSPACE CONTEXT".to_string()];
    lines.push(format!("- iteration: {iteration}/{max_iterations}"));
    lines.push(format!("- tool_calls_total: {}/{}", state.tool_calls_total, policy.max_tool_calls_total));

    if !state.recent_errors.is_empty() {
        lines.push("- recent_errors:".to_string());
        for e in last_n(&state.recent_errors.iter().map(|s| s.as_str()).collect::<Vec<_>>(), 3) {
            lines.push(format!("  - {e}"));
        }
    }

    if !state.recent.is_empty() {
        lines.push("- recent_tool_results:".to_string());
        let start = state.recent.len().saturating_sub(10);
        for r in &state.recent[start..] {
            let mut s = r.summary.replace('\n', " | ");
            if s.chars().count() > 240 {
                s = truncate_with_ellipsis(&s, 240);
            }
            lines.push(format!("  - {}({}): {}", r.tool, r.args_preview, s));
        }
    }

    lines.join("\n").trim().to_string()
}

/// Borrowed view of the fields `workspace_context_block` needs, so it doesn't need a
/// full `&RunState`.
struct RunStateView<'a> {
    tool_calls_total: u32,
    recent_errors: &'a [String],
    recent: &'a [RecentEntry],
}

/// Append or replace the `WORKSPACE CONTEXT` block on the last user message. Keeping
/// this out of the system prompt lets the system message stay byte-identical across
/// iterations, which is required for Anthropic prompt caching to hit.
fn upsert_workspace_context(messages: &mut Vec<serde_json::Value>, block: &str) {
    const MARKER: &str = "WORKSPACE CONTEXT";
    for i in (0..messages.len()).rev() {
        if messages[i].get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let content = messages[i].get("content").cloned().unwrap_or(serde_json::Value::Null);
        match content {
            serde_json::Value::String(c) => {
                let tag = format!("\n\n{MARKER}");
                let base = if let Some(idx) = c.find(&tag) {
                    c[..idx].to_string()
                } else if c.starts_with(MARKER) {
                    String::new()
                } else {
                    c
                };
                let new_content = format!("{base}\n\n{block}").trim_start().to_string();
                messages[i]["content"] = serde_json::Value::String(new_content);
                return;
            }
            serde_json::Value::Array(blocks) => {
                let mut kept: Vec<serde_json::Value> = blocks
                    .into_iter()
                    .filter(|b| {
                        !(b.get("type").and_then(|v| v.as_str()) == Some("text")
                            && b.get("text")
                                .and_then(|v| v.as_str())
                                .map(|t| t.starts_with(MARKER))
                                .unwrap_or(false))
                    })
                    .collect();
                kept.push(serde_json::json!({"type": "text", "text": block}));
                messages[i]["content"] = serde_json::Value::Array(kept);
                return;
            }
            _ => continue,
        }
    }
    messages.push(serde_json::json!({"role": "user", "content": block}));
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn AgentTool>>,
    max_iterations: u32,
    policy: AgentPolicy,
    observer: Box<dyn RunObserver>,
    logging: Box<dyn ToolLogSink>,
}

impl Agent {
    pub fn new(
        model: Box<dyn Model>,
        tools: Vec<Box<dyn AgentTool>>,
        max_iterations: u32,
        observer: Box<dyn RunObserver>,
        logging: Box<dyn ToolLogSink>,
    ) -> Self {
        Self { model, tools, max_iterations, policy: AgentPolicy::default(), observer, logging }
    }

    // Ported for API-shape completeness matching `_agent.py`'s `Agent.enable_tool`/
    // `disable_tool` — `loop_runner.rs` never calls these today (same as Python).
    #[allow(dead_code)]
    pub fn enable_tool(&mut self, name: &str) -> anyhow::Result<()> {
        let tool = self
            .tools
            .iter_mut()
            .find(|t| t.name() == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name:?}"))?;
        tool.set_enabled(true);
        Ok(())
    }

    #[allow(dead_code)]
    pub fn disable_tool(&mut self, name: &str) -> anyhow::Result<()> {
        let tool = self
            .tools
            .iter_mut()
            .find(|t| t.name() == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name:?}"))?;
        tool.set_enabled(false);
        Ok(())
    }

    fn format_args(name: &str, args: &serde_json::Value) -> String {
        match name {
            "bash" => {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("").replace('\n', " ");
                truncate_with_ellipsis(cmd.trim(), 60)
            }
            "read" | "write" => args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            "edit" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let n = args.get("edits").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                format!("{path}  ({n} edit{})", if n != 1 { "s" } else { "" })
            }
            _ => truncate_with_ellipsis(&serde_json::to_string(args).unwrap_or_default(), 60),
        }
    }

    fn emit(&self, payload: serde_json::Value) {
        if let serde_json::Value::Object(map) = payload {
            self.logging.emit(&map);
        }
    }

    fn execute_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
        run_id: &str,
        iteration: u32,
        model_text: Option<&str>,
    ) -> String {
        let timer = Timer::new();
        let mut ok = true;
        let mut error: Option<String> = None;

        let output = 'exec: {
            let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
                ok = false;
                error = Some(format!("unknown tool {name:?}"));
                break 'exec format!("Error: {}", error.as_ref().unwrap());
            };
            if !tool.enabled() {
                ok = false;
                error = Some(format!("tool {name:?} is disabled"));
                break 'exec format!("Error: {}", error.as_ref().unwrap());
            }
            if !args.is_object() {
                ok = false;
                error = Some(format!("tool {name:?} arguments must be an object"));
                break 'exec format!("Error: {}", error.as_ref().unwrap());
            }
            if let Some(err) = validate_schema_value(tool.parameters(), args, "args") {
                ok = false;
                error = Some(err.clone());
                break 'exec format!("Error: {err}");
            }

            self.emit(serde_json::json!({
                "event": "tool_start",
                "run_id": run_id,
                "iteration": iteration,
                "tool": name,
                "args": args,
                "model_reasoning": model_text,
            }));

            let raw_output = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.run(args))) {
                Ok(o) => o,
                Err(_) => {
                    ok = false;
                    error = Some("tool panicked".to_string());
                    "Error: tool panicked".to_string()
                }
            };

            if ok && is_error_output(&raw_output) {
                ok = false;
                error = Some(raw_output.trim_start()[6..].trim().to_string());
            }

            raw_output
        };

        self.emit(serde_json::json!({
            "event": "tool_end",
            "run_id": run_id,
            "iteration": iteration,
            "tool": name,
            "ok": ok,
            "error": error,
            "duration_ms": round3(timer.ms()),
            "output_chars": output.chars().count(),
            "output_preview": if output.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(output.chars().take(500).collect())
            },
        }));

        output
    }

    fn record_rejected_call(
        &self,
        state: &mut RunState,
        name: &str,
        args: &serde_json::Value,
        reason: RejectReason,
        run_id: &str,
        iteration: u32,
    ) {
        state.rejected_calls.push(RejectedCall { name: name.to_string(), args: args.clone(), reason });
        self.emit(serde_json::json!({
            "event": "tool_rejected",
            "run_id": run_id,
            "iteration": iteration,
            "tool": name,
            "args": args,
            "reason": reason.as_str(),
        }));
    }

    fn record_tool_result(&self, state: &mut RunState, name: &str, args: &serde_json::Value, output: &str) {
        let summary = summarize_tool_output(name, output);
        let ok = !is_error_output(output);
        state.recent.push(RecentEntry {
            tool: name.to_string(),
            args_preview: Self::format_args(name, args),
            summary: summary.chars().take(2000).collect(),
            ok,
        });
        if state.recent.len() > 12 {
            let excess = state.recent.len() - 12;
            state.recent.drain(0..excess);
        }
        if !ok {
            state.recent_errors.push(format!("{name}: {}", truncate_with_ellipsis(&summary, 400)));
            if state.recent_errors.len() > 5 {
                let excess = state.recent_errors.len() - 5;
                state.recent_errors.drain(0..excess);
            }
        }
    }

    fn max_repeats_for_tool(&self, tool_name: &str) -> u32 {
        if tool_name == "read" {
            self.policy.max_repeated_calls * self.policy.read_repeat_multiplier
        } else {
            self.policy.max_repeated_calls
        }
    }

    fn repeated_call_hint(max_repeats: u32) -> String {
        format!(
            "Error: repeated tool call blocked after {max_repeats} tries. \
             Change arguments (e.g., different offset/limit/path) or change approach."
        )
    }

    fn execute_one_tool_call(
        &self,
        state: &mut RunState,
        call: &ToolCall,
        model_text: &str,
        run_id: &str,
        iteration: u32,
    ) -> (ToolCall, String) {
        let call_name = call.name.clone();
        let call_args = call.arguments.clone();
        let max_repeats = self.max_repeats_for_tool(&call_name);

        let key = tool_call_key(&call_name, &call_args);
        let count = state.call_counts.entry(key).or_insert(0);
        *count += 1;

        if *count > max_repeats {
            let hint = Self::repeated_call_hint(max_repeats);
            self.record_tool_result(state, &call_name, &call_args, &hint);
            self.record_rejected_call(state, &call_name, &call_args, RejectReason::RepeatBlocked, run_id, iteration);
            return (
                ToolCall {
                    id: call.id.clone(),
                    name: call_name,
                    arguments: call_args,
                    raw_arguments: None,
                    raw_arguments_error: None,
                },
                hint,
            );
        }

        self.observer.on_tool_start(&call_name, &Self::format_args(&call_name, &call_args));

        let output = if let Some(err) = &call.raw_arguments_error {
            let raw = call.raw_arguments.clone().unwrap_or_default();
            let out = format!(
                "Error: tool call arguments were not valid JSON and could not be parsed.\n\
                 tool={call_name:?}\nparse_error={err}\nraw_arguments={raw:?}\n\
                 Fix: emit a tool call with a JSON object matching the tool schema."
            );
            self.record_rejected_call(state, &call_name, &call_args, RejectReason::BadJson, run_id, iteration);
            out
        } else {
            let out = self.execute_tool(&call_name, &call_args, run_id, iteration, Some(model_text));
            state.tool_calls_total += 1;
            state.tool_results.push(ToolResult {
                name: call_name.clone(),
                args: call_args.clone(),
                ok: !is_error_output(&out),
                output_preview: out.chars().take(500).collect(),
            });
            out
        };

        self.record_tool_result(state, &call_name, &call_args, &output);

        if !is_error_output(&output) && (call_name == "edit" || call_name == "write") {
            state.call_counts.clear();
        }

        self.observer.on_tool_end(&call_name, &output);

        (
            ToolCall {
                id: call.id.clone(),
                name: call_name,
                arguments: call_args,
                raw_arguments: None,
                raw_arguments_error: None,
            },
            output,
        )
    }

    fn history_output(&self, output: &str) -> String {
        if is_error_output(output) || output.chars().count() <= self.policy.max_history_chars {
            return output.to_string();
        }

        let omitted = output.chars().count() - self.policy.max_history_chars;
        let truncated: String = output.chars().take(self.policy.max_history_chars).collect();
        format!(
            "{truncated}\n…[output truncated: {omitted} chars omitted. \
             Use read(path, offset=N) to page through the rest.]"
        )
    }

    fn append_skipped_tool_calls(
        &self,
        state: &mut RunState,
        tool_calls: &[ToolCall],
        start_idx: usize,
        outputs: &mut Vec<String>,
        executed_calls: &mut Vec<ToolCall>,
        run_id: &str,
        iteration: u32,
    ) {
        let skipped = tool_calls.len().saturating_sub(start_idx);
        if skipped == 0 {
            return;
        }

        for call in &tool_calls[start_idx..] {
            self.record_rejected_call(state, &call.name, &call.arguments, RejectReason::IterationSkip, run_id, iteration);
        }

        outputs.push(format!(
            "Note: {skipped} tool call(s) were skipped this iteration due to \
             max_tool_calls_per_iteration={}.",
            self.policy.max_calls_per_iteration
        ));
        executed_calls.push(ToolCall {
            id: "skipped_tool_calls_note".to_string(),
            name: "note".to_string(),
            arguments: serde_json::json!({}),
            raw_arguments: None,
            raw_arguments_error: None,
        });
    }

    fn update_consecutive_error_state(&self, state: &mut RunState, outputs: &[String]) {
        let real_outputs: Vec<&String> = outputs.iter().filter(|output| !output.starts_with("Note:")).collect();
        if !real_outputs.is_empty() && real_outputs.iter().all(|output| is_error_output(output)) {
            state.consecutive_error_iters += 1;
        } else {
            state.consecutive_error_iters = 0;
        }
    }

    fn append_stuck_nudge_if_needed(&self, state: &mut RunState, messages: &mut Vec<serde_json::Value>) {
        if state.consecutive_error_iters < self.policy.max_consecutive_error_iters {
            return;
        }

        state.consecutive_error_iters = 0;
        let stuck = "Every tool call for several consecutive iterations has failed. \
                     Stop attempting tool calls and output a BLOCKED: section \
                     listing exactly what is preventing progress and what the user must resolve.";
        let last_is_str = messages.last().and_then(|m| m.get("content")).and_then(|c| c.as_str()).is_some();
        if last_is_str {
            let last = messages.last_mut().unwrap();
            let content = last["content"].as_str().unwrap().to_string();
            last["content"] = serde_json::Value::String(format!("{content}\n\n{stuck}"));
        } else {
            messages.push(serde_json::json!({"role": "user", "content": stuck}));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_tool_calls(
        &self,
        state: &mut RunState,
        tool_calls: &[ToolCall],
        messages: &mut Vec<serde_json::Value>,
        reasoning_blocks: &[serde_json::Value],
        model_text: &str,
        run_id: &str,
        iteration: u32,
    ) -> Option<StopReason> {
        if state.tool_calls_total >= self.policy.max_tool_calls_total {
            return Some(StopReason::ToolBudget);
        }

        let limit = self.policy.max_calls_per_iteration as usize;
        let limited_call_count = tool_calls.len().min(limit);

        let mut outputs: Vec<String> = Vec::new();
        let mut executed_calls: Vec<ToolCall> = Vec::new();

        for call in &tool_calls[..limited_call_count] {
            let (executed_call, output) =
                self.execute_one_tool_call(state, call, model_text, run_id, iteration);

            outputs.push(self.history_output(&output));
            executed_calls.push(executed_call);
        }

        self.append_skipped_tool_calls(
            state,
            tool_calls,
            limited_call_count,
            &mut outputs,
            &mut executed_calls,
            run_id,
            iteration,
        );

        let tool_result_messages = self.model.format_tool_result_messages(&executed_calls, &outputs, reasoning_blocks);
        messages.extend(tool_result_messages);

        self.update_consecutive_error_state(state, &outputs);
        self.append_stuck_nudge_if_needed(state, messages);

        None
    }

    /// Run the agent on a task and return a structured `RunResult`.
    ///
    /// A hard error from the model (network/API failure) propagates out as `Err`,
    /// matching the Python source letting an unhandled provider exception crash the
    /// whole process rather than degrading gracefully.
    pub fn run(&self, task: &str, system: &str, chat_history: &[serde_json::Value]) -> anyhow::Result<RunResult> {
        let mut messages: Vec<serde_json::Value> = Vec::new();

        let system_trimmed = system.trim();
        let mut sys_parts: Vec<&str> = Vec::new();
        if !system_trimmed.is_empty() {
            sys_parts.push(system_trimmed);
        }
        sys_parts.push(REACT_NUDGE.trim());
        messages.push(serde_json::json!({"role": "system", "content": sys_parts.join("\n\n")}));

        messages.extend(chat_history.iter().cloned());
        messages.push(serde_json::json!({"role": "user", "content": task}));

        let mut state = RunState::default();
        let mut empty_reply_nudged = false;

        let run_id = crate::logging::new_run_id();
        self.emit(serde_json::json!({
            "event": "run_start",
            "run_id": run_id,
            "mode": "native_tools",
            "max_iterations": self.max_iterations,
        }));
        self.observer.on_run_start(&run_id, self.max_iterations);

        let mut stop_reason = StopReason::MaxIterations;
        let mut final_text: Option<String> = None;
        let mut iters_done: u32 = 0;

        for iteration in 0..self.max_iterations {
            iters_done = iteration + 1;
            self.emit(serde_json::json!({"event": "iteration_start", "run_id": run_id, "iteration": iters_done}));

            let view = RunStateView {
                tool_calls_total: state.tool_calls_total,
                recent_errors: &state.recent_errors,
                recent: &state.recent,
            };
            let block = workspace_context_block(&view, iters_done, self.max_iterations, &self.policy);
            upsert_workspace_context(&mut messages, &block);

            let tool_schemas: Vec<serde_json::Value> =
                self.tools.iter().filter(|t| t.enabled()).map(|t| t.to_openai_tool()).collect();

            let model_timer = Timer::new();
            let guard = self.observer.model_call_start(iters_done);
            let response: ChatResponse = self.model.chat_with_tools(&messages, &tool_schemas)?;
            drop(guard);

            let text = response.text.trim().to_string();
            let reasoning = response.reasoning.trim().to_string();
            let iter_usage = TokenUsage::from_raw(response.usage.as_ref());
            state.usage += iter_usage;

            self.observer.on_model_response(iters_done, &iter_usage, &reasoning);
            self.emit(serde_json::json!({
                "event": "model_response",
                "run_id": run_id,
                "iteration": iters_done,
                "duration_ms": round3(model_timer.ms()),
                "text_chars": text.chars().count(),
                "tool_calls": response.tool_calls.len(),
                "usage": response.usage,
                "model_text": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text.chars().take(500).collect()) },
                "model_reasoning": if reasoning.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(reasoning.chars().take(500).collect()) },
            }));
            state.iteration_telemetry.push(IterationTelemetry {
                iteration: iters_done,
                input_tokens: iter_usage.input,
                output_tokens: iter_usage.output,
                reasoning_tokens: iter_usage.reasoning,
                cache_write_tokens: iter_usage.cache_write,
                cache_read_tokens: iter_usage.cache_read,
                tool_calls_requested: response.tool_calls.len() as u32,
                had_final_text: !text.is_empty(),
            });

            let tool_calls = response.tool_calls;
            let reasoning_blocks = response.reasoning_blocks;

            // ReAct rule: if tool calls exist, execute them even if some text is also present.
            if !tool_calls.is_empty() {
                let stop = self.handle_tool_calls(
                    &mut state,
                    &tool_calls,
                    &mut messages,
                    &reasoning_blocks,
                    &text,
                    &run_id,
                    iters_done,
                );
                if let Some(reason) = stop {
                    stop_reason = reason;
                    final_text = None;
                    break;
                }
                self.emit(serde_json::json!({
                    "event": "iteration_end",
                    "run_id": run_id,
                    "iteration": iters_done,
                    "stop_reason": "tool_calls",
                }));
                continue;
            }

            if !text.is_empty() {
                final_text = Some(text);
                stop_reason = StopReason::FinalText;
                break;
            }

            if !empty_reply_nudged {
                empty_reply_nudged = true;
                let nudge = if state.recent_errors.iter().any(|e| e.contains("JSON") || e.contains("not valid JSON")) {
                    "Your last tool call could not be parsed because the arguments \
                     were not valid JSON. Please re-emit the tool call with a properly \
                     formed JSON object matching the tool schema."
                } else {
                    "Reply with either tool_calls (to take an action) or a final answer as text."
                };
                let last_is_str = messages.last().and_then(|m| m.get("content")).and_then(|c| c.as_str()).is_some();
                if last_is_str {
                    let last = messages.last_mut().unwrap();
                    let content = last["content"].as_str().unwrap().to_string();
                    last["content"] = serde_json::Value::String(format!("{content}\n\n{nudge}"));
                } else {
                    messages.push(serde_json::json!({"role": "user", "content": nudge}));
                }
                continue;
            }
        }

        let result = RunResult {
            final_text,
            stop_reason: stop_reason.as_str().to_string(),
            usage: state.usage,
            iterations: iters_done,
            tool_results: state.tool_results,
            rejected_calls: state.rejected_calls,
            iteration_telemetry: state.iteration_telemetry,
        };

        self.observer.on_run_end(&result);
        self.emit(serde_json::json!({
            "event": "run_end",
            "run_id": run_id,
            "iteration": iters_done,
            "stop_reason": result.stop_reason,
            "total_input_tokens": result.usage.input,
            "total_output_tokens": result.usage.output,
            "total_reasoning_tokens": result.usage.reasoning,
            "total_cache_creation_tokens": result.usage.cache_write,
            "total_cache_read_tokens": result.usage.cache_read,
        }));

        Ok(result)
    }
}
