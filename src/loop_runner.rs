//! Bounded author → review agent loop orchestration.
//!
//! Ports `codescribe/lib/_loop.py` (minus the `LoopPaths`/`get_loop_paths` section,
//! which lives in `paths.rs`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;

use crate::agent::{Agent, RunResult};
use crate::logging::{atomic_write_text, atomic_write_toml, iso_utc_now, new_run_id, read_toml, Timer, ToolLogToml, MultiToolLogSink};
use crate::model::anthropic::AnthropicModel;
use crate::observer::{ConsoleObserver, NoopObserver, RunObserver};
use crate::paths::{get_loop_paths, LoopPaths};
use crate::tools::{make_tools, AgentTool, BashTool, GlobTool, ReadTool, WriteTool};

// ---------------------------------------------------------------------------
// Cross-loop summary
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct LoopSummary {
    // Set on construction but never read back — callers (build_review_task, etc.)
    // thread the loop index separately, matching Python's LoopSummary.loop_index.
    #[allow(dead_code)]
    pub loop_index: u32,
    pub files_written: Vec<String>,
    pub files_edited: Vec<String>,
    pub files_read: Vec<String>,
    pub commands_run: Vec<String>,
    pub errors: Vec<String>,
    pub rejected: Vec<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
}

/// Plain hard-truncate (no ellipsis) — matches Python's `s[:n]` slicing used when
/// building `LoopSummary` action previews.
fn truncate_ap(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn action_preview(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("").replace('\n', " ");
            truncate_ap(cmd.trim(), 80)
        }
        "read" | "write" | "edit" => args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

fn record_tool_error(summary: &mut LoopSummary, tool_name: &str, action: &str, preview: &str) {
    let err_msg = if preview.is_empty() {
        "unknown error".to_string()
    } else {
        truncate_ap(preview, 80)
    };
    summary.errors.push(format!("{tool_name}({action}): {err_msg}"));
}

fn record_bash_command(summary: &mut LoopSummary, action: &str, preview: &str) {
    let first_line = truncate_ap(preview.lines().next().unwrap_or(""), 60);
    summary.commands_run.push(format!("{action}  →  {first_line}"));
}

/// Build a harness-computed `LoopSummary` directly from an `Agent::run()` result,
/// rather than re-parsing an event log.
pub fn loop_summary_from_result(loop_index: u32, result: &RunResult) -> LoopSummary {
    let mut summary = LoopSummary { loop_index, ..Default::default() };

    for tr in &result.tool_results {
        let action = action_preview(&tr.name, &tr.args);
        let preview = tr.output_preview.trim();

        if !tr.ok {
            record_tool_error(&mut summary, &tr.name, &action, preview);
            continue;
        }

        if tr.name == "write" {
            summary.files_written.push(action);
            continue;
        }

        if tr.name == "edit" {
            summary.files_edited.push(action);
            continue;
        }

        if tr.name == "read" {
            summary.files_read.push(action);
            continue;
        }

        if tr.name == "bash" {
            record_bash_command(&mut summary, &action, preview);
        }
    }

    for rc in &result.rejected_calls {
        let ap = action_preview(&rc.name, &rc.args);
        summary.rejected.push(format!("{}({ap}): {}", rc.name, rc.reason.as_str()));
    }

    summary.total_input_tokens = result.usage.input;
    summary.total_output_tokens = result.usage.output;
    summary.total_cache_creation_tokens = result.usage.cache_write;
    summary.total_cache_read_tokens = result.usage.cache_read;

    summary
}

// ---------------------------------------------------------------------------
// Prompt builders
// ---------------------------------------------------------------------------

fn build_system_prompt() -> String {
    "You are an autonomous coding agent specializing in test-driven development and repair.

Core rules:
- NEVER fabricate command results, test output, or file contents — always call a tool.
- Determine project state by reading files and running commands; never assume or infer.
- Only access files and paths within the working directory; never target system directories.
- In bash commands, only use relative paths or paths under the working directory.

Efficiency rules (critical for speed):
- Batch ALL independent reads and globs into the first turn — fetch everything you need before acting.
- Once you have identified the changes needed, implement them immediately without further exploration.
- Do not re-read files you have already read unless they were modified since your last read.
- Prefer a single comprehensive edit over multiple small edits to the same file.
- Run tests after each meaningful code change — do not defer validation to the end.
- When all tests pass and all pending items are resolved, stop immediately — do not invent further work.

Tool guidance:
- Use glob to discover structure, read for content, bash for running commands and tests.
- Use edit for targeted changes; use write only when creating new files or doing full rewrites.
"
    .to_string()
}

fn format_loop_context(loop_idx: u32, agent_loops: u32, loop_summaries: &[LoopSummary], pending_items: &[String]) -> String {
    let mut lines = vec![format!("Loop {loop_idx} of {agent_loops}.")];

    let mut all_written: Vec<String> = Vec::new();
    let mut all_edited: Vec<String> = Vec::new();
    let mut seen_w: HashSet<String> = HashSet::new();
    let mut seen_e: HashSet<String> = HashSet::new();
    for s in loop_summaries {
        for f in &s.files_written {
            if seen_w.insert(f.clone()) {
                all_written.push(f.clone());
            }
        }
        for f in &s.files_edited {
            if seen_e.insert(f.clone()) {
                all_edited.push(f.clone());
            }
        }
    }

    if !all_written.is_empty() {
        lines.push(format!("Files created across all prior loops: {}", all_written.join(", ")));
    }
    if !all_edited.is_empty() {
        lines.push(format!("Files edited across all prior loops: {}", all_edited.join(", ")));
    }

    if let Some(last) = loop_summaries.last() {
        let mut parts: Vec<String> = Vec::new();
        if !last.commands_run.is_empty() {
            parts.push(format!("ran: {}", last.commands_run.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
        }
        if !last.errors.is_empty() {
            parts.push(format!("errors: {}", last.errors.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
        }
        if !parts.is_empty() {
            lines.push(format!("Last loop — {}.", parts.join(" | ")));
        }
    }

    if !pending_items.is_empty() {
        lines.push("Pending next steps:".to_string());
        for item in pending_items.iter().take(5) {
            lines.push(format!("  - {item}"));
        }
    } else {
        lines.push("No pending steps recorded — determine next action from the task file.".to_string());
    }

    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
fn build_author_task(
    workdir: &Path,
    task_rel: &str,
    task_content: &str,
    loop_idx: u32,
    agent_loops: u32,
    loop_summaries: &[LoopSummary],
    pending_items: &[String],
) -> String {
    let context = format_loop_context(loop_idx, agent_loops, loop_summaries, pending_items);
    let orient_step = if loop_idx == 1 && !task_content.is_empty() {
        format!(
            "1. The full task specification is provided below — do NOT re-read the task file.\n\n<task_specification>\n{task_content}\n</task_specification>\n"
        )
    } else if loop_idx == 1 {
        "1. Read the task file to orient yourself on the specification.\n".to_string()
    } else {
        "1. The specification and all prior work are already summarised above — do NOT re-read the task file or re-glob the workspace to orient yourself.\n".to_string()
    };

    format!(
        "{context}

Working directory: {workdir}
Task file: {task_rel} (read-only — contains the full specification)

PHASE: AUTHOR

Goal: COMPLETE the task in this single session if at all possible. Implement every
remaining item you can, verify it, and only stop when the task is fully done or you
are genuinely blocked. Do NOT implement just one item and defer the rest to a later
loop — keep working until everything that can be done this session is done.

Protocol:
{orient_step}2. Write a short PLAN (3–7 bullets) covering everything you intend to complete now.
3. Execute the plan autonomously and to completion — do NOT ask for confirmation, and
   do NOT stop after a single change while more work remains.
4. Before each set of tool calls, write one or two sentences stating what you are about to do and why.
5. Inspect the current state of relevant files before editing them.
6. After each meaningful change, run the closest available check (tests, lint, typecheck).
   If none exists, run `python -m compileall .`. Do not defer all validation to the end.
7. Continue until every task is implemented AND the checks pass, or you hit a hard blocker.

Finish with <final_answer> that includes, in this order:
- A status line of the EXACT form `STATUS: COMPLETE` if every task is implemented and all
  checks pass, otherwise `STATUS: INCOMPLETE`.
- The PLAN you followed (final form).
- Exact output of the checks you ran (verbatim, not paraphrased).
- ONLY when STATUS is INCOMPLETE: a bulleted list of NEXT STEPS (3-5 concrete items) for
  the following loop, beginning with the exact line: NEXT STEPS:
",
        context = context,
        workdir = workdir.display(),
        task_rel = task_rel,
        orient_step = orient_step,
    )
}

fn build_review_task(
    workdir: &Path,
    task_rel: &str,
    review_output_rel: &str,
    loop_index: u32,
    loop_summary: &LoopSummary,
    exec_answer: &str,
) -> String {
    let mut summary_lines = vec![format!("## Verified actions from loop {loop_index} (harness-computed)")];
    if !loop_summary.files_read.is_empty() {
        summary_lines.push(format!("Files read: {}", loop_summary.files_read.join(", ")));
    }
    if !loop_summary.files_written.is_empty() {
        summary_lines.push(format!("Files written: {}", loop_summary.files_written.join(", ")));
    }
    if !loop_summary.files_edited.is_empty() {
        summary_lines.push(format!("Files edited: {}", loop_summary.files_edited.join(", ")));
    }
    if !loop_summary.commands_run.is_empty() {
        summary_lines.push("Commands run:".to_string());
        for c in &loop_summary.commands_run {
            summary_lines.push(format!("  {c}"));
        }
    }
    if !loop_summary.errors.is_empty() {
        summary_lines.push("Errors:".to_string());
        for e in &loop_summary.errors {
            summary_lines.push(format!("  {e}"));
        }
    }
    if loop_summary.files_read.is_empty()
        && loop_summary.files_written.is_empty()
        && loop_summary.files_edited.is_empty()
        && loop_summary.commands_run.is_empty()
        && loop_summary.errors.is_empty()
    {
        summary_lines.push("  (no verified actions)".to_string());
    }
    let verified_block = summary_lines.join("\n");

    let rejected_block = if !loop_summary.rejected.is_empty() {
        let mut rejected_lines = vec!["## Attempted but NOT executed (harness-rejected — no workspace effect)".to_string()];
        for r in &loop_summary.rejected {
            rejected_lines.push(format!("  {r}"));
        }
        rejected_lines.join("\n")
    } else {
        String::new()
    };

    let mut out = format!(
        "Working directory: {}\nTask file: {task_rel} (read-only)\nReview output: {review_output_rel} (write here)\n\nPHASE: REVIEW\n\n{verified_block}\n\n",
        workdir.display()
    );
    if !rejected_block.is_empty() {
        out.push_str(&rejected_block);
        out.push_str("\n\n");
    }
    if !exec_answer.is_empty() {
        out.push_str(&format!("\n[Author agent report — loop {loop_index}]\n\n{exec_answer}\n\n"));
    }
    out.push_str(&format!(
        "Your job:
1. Cross-reference the author report's claims against the verified actions above.
   File reads listed above are verified — treat the agent's claims about those
   files as trustworthy. Only flag claims about files NOT in the verified list.
   Any claim that relies on an 'Attempted but NOT executed' call did NOT actually
   happen — flag it as unverified and carry the underlying work into pending items.
2. Write your assessment to the review output file in TOML format:

   ```toml
   loop = {loop_index}
   summary = \"One paragraph describing what actually happened.\"
   blocker = \"Main current blocker, or empty string if none.\"

   [[pending]]
   item = \"First concrete next step\"
   ```

Rules:
- If tests passed and no errors are listed above, set blocker = \"\" and leave pending empty.
- pending items must be concrete and actionable (not 'continue working').
- Limit to 5 pending items maximum.

Finish with <final_answer> confirming you wrote the review output file.
"
    ));
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Complete,
    Incomplete,
}

fn extract_status(final_text: &str) -> Option<Status> {
    for line in final_text.lines() {
        let s = line.trim().to_uppercase();
        if let Some(val) = s.strip_prefix("STATUS:") {
            let val = val.trim();
            if val.starts_with("COMPLETE") {
                return Some(Status::Complete);
            }
            if val.starts_with("INCOMPLETE") {
                return Some(Status::Incomplete);
            }
        }
    }
    None
}

/// Strip a numbered-list prefix like "1. " / "12) " (any digit length — a deliberate
/// fix vs. the Python reference, which only recognizes a single leading digit due to a
/// hardcoded `stripped[1:3]` index check).
fn strip_numbered_bullet(s: &str) -> Option<String> {
    let digit_count = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let rest = &s[digit_count..];
    let rest = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))?;
    Some(rest.trim().to_string())
}

/// Parse the `NEXT STEPS:` section from an author agent's final answer.
fn extract_pending_items(final_text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut in_section = false;
    for line in final_text.lines() {
        let upper = line.trim().to_uppercase();
        if upper.starts_with("NEXT STEPS") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        if stripped.starts_with('#') {
            break;
        }
        if stripped.starts_with('-') || stripped.starts_with('*') {
            items.push(stripped.trim_start_matches(['-', '*', ' ']).trim().to_string());
        } else if let Some(rest) = strip_numbered_bullet(stripped) {
            items.push(rest);
        }
    }
    items.truncate(5);
    items
}

fn update_cumulative_tokens<'a>(run_toml_data: &mut RunToml, summaries: impl Iterator<Item = &'a LoopSummary>) {
    let (mut input, mut output, mut cache_write, mut cache_read) = (0u64, 0u64, 0u64, 0u64);
    for s in summaries {
        input += s.total_input_tokens;
        output += s.total_output_tokens;
        cache_write += s.total_cache_creation_tokens;
        cache_read += s.total_cache_read_tokens;
    }
    run_toml_data.cumulative_input_tokens = input;
    run_toml_data.cumulative_output_tokens = output;
    run_toml_data.cumulative_cache_creation_tokens = cache_write;
    run_toml_data.cumulative_cache_read_tokens = cache_read;
}

fn ensure_within_workdir(path: &Path, workdir: &Path) -> anyhow::Result<PathBuf> {
    let path_abs = if path.is_absolute() { path.to_path_buf() } else { workdir.join(path) };
    let resolved = path_abs
        .canonicalize()
        .with_context(|| format!("task file not found: {}", path.display()))?;
    let workdir_resolved = workdir.canonicalize()?;
    if !resolved.starts_with(&workdir_resolved) {
        anyhow::bail!("Path {} is outside working directory {}", resolved.display(), workdir_resolved.display());
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Run/state records
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct RunToml {
    run_id: String,
    created_at: String,
    workdir: String,
    task_file: String,
    model: String,
    agent_loops: u32,
    agent_iterations: u32,
    cumulative_input_tokens: u64,
    cumulative_output_tokens: u64,
    cumulative_cache_creation_tokens: u64,
    cumulative_cache_read_tokens: u64,
    loops_completed: u32,
}

const VALID_PHASES: &[&str] = &["idle", "author", "review"];

#[derive(Serialize, Clone)]
struct StateToml {
    run_id: String,
    workdir: String,
    task_file: String,
    loop_index: u32,
    phase: String,
    updated_at: String,
}

fn write_state(path: &Path, mut state: StateToml) -> anyhow::Result<()> {
    if state.run_id.is_empty() {
        anyhow::bail!("state.run_id must be a non-empty string");
    }
    if !VALID_PHASES.contains(&state.phase.as_str()) {
        anyhow::bail!("state.phase must be one of {VALID_PHASES:?}, got {:?}", state.phase);
    }
    state.updated_at = iso_utc_now();
    atomic_write_toml(path, &state)
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

pub struct RunnerConfig {
    pub task_file: PathBuf,
    pub model: String,
    pub agent_loops: u32,
    pub agent_iterations: u32,
    pub verbose: bool,
    pub logging: Option<String>,
    pub workdir: Option<PathBuf>,
    pub reason: bool,
}

pub struct PromptLoopRunner {
    model: String,
    agent_loops: u32,
    agent_iterations: u32,
    verbose: bool,
    logging: Option<String>,
    reason: bool,

    workdir_path: PathBuf,
    task_path: PathBuf,
    task_rel: String,
    review_output_rel: String,
    system: String,
    run_id: String,
    paths: LoopPaths,
    chat_history: Vec<serde_json::Value>,
    bash_allow: HashSet<String>,
    loop_summaries: Vec<LoopSummary>,
    pending_items: Vec<String>,
    loops_completed: u32,
    task_mtime: Option<std::time::SystemTime>,
    phase_metadata_files: Vec<PathBuf>,
    run_toml_data: RunToml,
    state: StateToml,
}

impl PromptLoopRunner {
    pub fn new(cfg: RunnerConfig) -> anyhow::Result<Self> {
        let RunnerConfig { task_file, model, agent_loops, agent_iterations, verbose, logging, workdir, reason } = cfg;

        let workdir_path = match &workdir {
            Some(w) => w
                .canonicalize()
                .with_context(|| format!("workdir does not exist: {}", w.display()))?,
            None => std::env::current_dir()?,
        };
        let task_path = ensure_within_workdir(&task_file, &workdir_path)?;
        let run_id = new_run_id();
        let paths = get_loop_paths(&workdir_path);
        let task_rel = task_path
            .strip_prefix(&workdir_path)
            .unwrap_or(&task_path)
            .to_string_lossy()
            .to_string();
        let review_output_rel = paths
            .review_output_toml
            .strip_prefix(&workdir_path)
            .unwrap_or(&paths.review_output_toml)
            .to_string_lossy()
            .to_string();
        let system = build_system_prompt();

        let run_toml_data = RunToml {
            run_id: run_id.clone(),
            created_at: iso_utc_now(),
            workdir: workdir_path.display().to_string(),
            task_file: task_path.display().to_string(),
            model: model.clone(),
            agent_loops,
            agent_iterations,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_cache_creation_tokens: 0,
            cumulative_cache_read_tokens: 0,
            loops_completed: 0,
        };
        let state = StateToml {
            run_id: run_id.clone(),
            workdir: workdir_path.display().to_string(),
            task_file: task_path.display().to_string(),
            loop_index: 0,
            phase: "idle".to_string(),
            updated_at: iso_utc_now(),
        };

        let mut runner = Self {
            model,
            agent_loops,
            agent_iterations,
            verbose,
            logging,
            reason,
            workdir_path,
            task_path,
            task_rel,
            review_output_rel,
            system,
            run_id,
            paths,
            chat_history: Vec::new(),
            bash_allow: HashSet::new(),
            loop_summaries: Vec::new(),
            pending_items: Vec::new(),
            loops_completed: 0,
            task_mtime: None,
            phase_metadata_files: Vec::new(),
            run_toml_data,
            state,
        };

        runner.load_task_context()?;
        runner.initialize_paths()?;
        runner.initialize_run_record()?;
        runner.initialize_loop_state()?;

        Ok(runner)
    }

    fn load_task_context(&mut self) -> anyhow::Result<()> {
        let (messages, meta) = crate::chat_template::load_chat_template(&self.task_path)?;
        self.chat_history = Self::messages_to_json(messages);
        self.bash_allow = meta.bash_allow;
        Ok(())
    }

    fn messages_to_json(messages: Vec<crate::chat_template::ChatMessage>) -> Vec<serde_json::Value> {
        messages
            .into_iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect()
    }

    /// Resolved paths the agent's own tools may never read/write/edit — currently just
    /// the task file, which grants tool policy at construction time (see `bash_allow`)
    /// and must not be tool-mutable mid-run.
    fn protected_paths(&self) -> HashSet<PathBuf> {
        HashSet::from([self.task_path.clone()])
    }

    fn initialize_paths(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.paths.run_dir)?;
        crate::telemetry::ensure_loop_metadata_dir(&self.paths.run_dir)?;
        Ok(())
    }

    fn initialize_run_record(&self) -> anyhow::Result<()> {
        atomic_write_toml(&self.paths.run_toml, &self.run_toml_data)
    }

    fn initialize_loop_state(&mut self) -> anyhow::Result<()> {
        write_state(&self.paths.state_toml, self.state.clone())
    }

    fn update_state(&mut self, loop_index: u32, phase: &str) -> anyhow::Result<()> {
        self.state.loop_index = loop_index;
        self.state.phase = phase.to_string();
        write_state(&self.paths.state_toml, self.state.clone())
    }

    /// Only the task content (`chat_history`) is refreshed here. The bash allowlist /
    /// tool policy is fixed once in `load_task_context()` at construction time and must
    /// never be re-derived mid-run: the task file lives inside the agent's own writable
    /// root, so re-reading its `[tools].bash` on every edit would let the agent grant
    /// itself new bash commands by rewriting its own task file.
    fn reload_task_context_if_needed(&mut self) -> anyhow::Result<()> {
        let current_mtime = std::fs::metadata(&self.task_path).and_then(|m| m.modified()).ok();
        if current_mtime != self.task_mtime {
            let (messages, _meta) = crate::chat_template::load_chat_template(&self.task_path)?;
            self.chat_history = Self::messages_to_json(messages);
            self.task_mtime = current_mtime;
        }
        Ok(())
    }

    fn persist_run_progress(&mut self, loop_idx: u32) -> anyhow::Result<()> {
        update_cumulative_tokens(&mut self.run_toml_data, self.loop_summaries.iter());
        self.run_toml_data.loops_completed = loop_idx;
        atomic_write_toml(&self.paths.run_toml, &self.run_toml_data)?;

        let run_doc = toml::Value::try_from(&self.run_toml_data)?
            .as_table()
            .cloned()
            .unwrap_or_default();
        crate::telemetry::write_loop_manifest(&self.paths.metadata_dir, &run_doc, &self.phase_metadata_files)?;
        self.loops_completed = loop_idx;
        Ok(())
    }

    fn build_author_logging(&self) -> Box<dyn crate::logging::ToolLogSink> {
        let author_log = ToolLogToml::new(Some(self.paths.author_toml.display().to_string()));
        if let Some(extra) = &self.logging {
            let extra_log = ToolLogToml::new(Some(extra.clone()));
            Box::new(MultiToolLogSink::new(vec![Box::new(author_log), Box::new(extra_log)]))
        } else {
            Box::new(author_log)
        }
    }

    fn build_observer(&self) -> Box<dyn RunObserver> {
        if self.verbose {
            Box::new(ConsoleObserver)
        } else {
            Box::new(NoopObserver)
        }
    }

    fn build_author_agent(&self, logging: Box<dyn crate::logging::ToolLogSink>) -> anyhow::Result<Agent> {
        let model = Box::new(AnthropicModel::new(self.model.clone(), self.reason)?);
        let tools = make_tools(&self.workdir_path, &self.bash_allow, &self.protected_paths());
        Ok(Agent::new(model, tools, self.agent_iterations, self.build_observer(), logging))
    }

    fn read_initial_task_content(&self, loop_idx: u32) -> String {
        if loop_idx != 1 {
            return String::new();
        }
        let full_path = self.workdir_path.join(&self.task_rel);
        match std::fs::read(&full_path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => String::new(),
        }
    }

    fn run_author_phase(&mut self, loop_idx: u32) -> anyhow::Result<(LoopSummary, String, Option<Status>)> {
        if self.verbose {
            println!("\n▶  loop {loop_idx} [author]");
        }

        self.update_state(loop_idx, "author")?;
        atomic_write_text(&self.paths.author_toml, "")?;

        let author_log = self.build_author_logging();
        let author_agent = self.build_author_agent(author_log)?;
        let task_content = self.read_initial_task_content(loop_idx);

        let author_task = build_author_task(
            &self.workdir_path,
            &self.task_rel,
            &task_content,
            loop_idx,
            self.agent_loops,
            &self.loop_summaries,
            &self.pending_items,
        );

        let phase_timer = Timer::new();
        let author_result = author_agent.run(&author_task, &self.system, &self.chat_history)?;
        let phase_duration_s = phase_timer.ms() / 1000.0;
        let author_answer = author_result.final_text.clone().unwrap_or_default();
        let loop_summary = loop_summary_from_result(loop_idx, &author_result);

        self.loop_summaries.push(loop_summary.clone());
        self.pending_items = extract_pending_items(&author_answer);

        let phase_file = self.write_phase_metadata(loop_idx, "author", &author_result, phase_duration_s)?;
        self.phase_metadata_files.push(phase_file);
        self.persist_run_progress(loop_idx)?;

        let status = extract_status(&author_answer);
        Ok((loop_summary, author_answer, status))
    }

    fn write_phase_metadata(
        &self,
        loop_idx: u32,
        phase: &str,
        result: &RunResult,
        duration_s: f64,
    ) -> anyhow::Result<PathBuf> {
        crate::telemetry::write_loop_phase_metadata(
            &self.paths.metadata_dir,
            &self.run_id,
            loop_idx,
            phase,
            &self.model,
            &self.task_path.display().to_string(),
            &self.workdir_path.display().to_string(),
            &result.stop_reason,
            result.final_text.is_some(),
            &result.usage,
            result.iterations,
            &result.tool_results,
            &result.rejected_calls,
            duration_s,
        )
    }

    fn build_review_tools(&self) -> Vec<Box<dyn AgentTool>> {
        // Deliberately excludes "env"/"printenv": those dump the process environment
        // (including ANTHROPIC_API_KEY) into tool output, which gets persisted in
        // plaintext to review.toml/loop metadata and re-sent to the model as a tool result.
        let review_bash_allow: HashSet<String> =
            ["ls", "stat", "pwd", "find", "grep", "head", "tail", "which", "rg"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let protected_paths = self.protected_paths();
        vec![
            Box::new(ReadTool::new(Some(self.workdir_path.clone()))),
            Box::new(GlobTool::new(Some(self.workdir_path.clone()))),
            Box::new(
                WriteTool::new(Some(self.workdir_path.clone())).with_protected_paths(protected_paths.clone()),
            ),
            Box::new(
                BashTool::new(Some(self.workdir_path.clone()), true, Some(review_bash_allow))
                    .with_protected_paths(protected_paths),
            ),
        ]
    }

    fn build_review_agent(&self) -> anyhow::Result<Agent> {
        let model = Box::new(AnthropicModel::new(self.model.clone(), self.reason)?);
        let tools = self.build_review_tools();
        let max_iter = std::cmp::max(6, self.agent_iterations / 2);
        let logging = Box::new(ToolLogToml::new(Some(self.paths.run_dir.join("review.toml").display().to_string())));
        Ok(Agent::new(model, tools, max_iter, self.build_observer(), logging))
    }

    fn apply_review_data(&mut self, review_data: &toml::Table) -> bool {
        if let Some(pending_val) = review_data.get("pending").and_then(|v| v.as_array()) {
            let mut items = Vec::new();

            for pending_entry in pending_val {
                let item = if let Some(table) = pending_entry.as_table() {
                    table.get("item").and_then(|value| value.as_str())
                } else {
                    pending_entry.as_str()
                };

                if let Some(item) = item {
                    if !item.is_empty() {
                        items.push(item.to_string());
                    }
                }
            }

            self.pending_items = items;
        }
        let blocker = review_data.get("blocker").and_then(|v| v.as_str()).unwrap_or("");
        self.pending_items.is_empty() && blocker.is_empty()
    }

    fn run_review_phase(&mut self, loop_idx: u32, loop_summary: &LoopSummary, exec_answer: &str) -> anyhow::Result<bool> {
        if self.verbose {
            println!("\n▶  loop {loop_idx} [review]");
        }

        self.update_state(loop_idx, "review")?;
        let review_agent = self.build_review_agent()?;

        let review_task = build_review_task(
            &self.workdir_path,
            &self.task_rel,
            &self.review_output_rel,
            loop_idx,
            loop_summary,
            exec_answer,
        );

        let phase_timer = Timer::new();
        let review_result = review_agent.run(&review_task, &self.system, &[])?;
        let phase_duration_s = phase_timer.ms() / 1000.0;

        let phase_file = self.write_phase_metadata(loop_idx, "review", &review_result, phase_duration_s)?;
        self.phase_metadata_files.push(phase_file);
        self.persist_run_progress(loop_idx)?;

        let review_data = read_toml(&self.paths.review_output_toml);
        if review_data.is_empty() {
            return Ok(false);
        }

        if self.apply_review_data(&review_data) {
            if self.verbose {
                println!("\n✓ No pending items and no blocker after loop {loop_idx} — task complete, stopping early.");
            }
            return Ok(true);
        }

        Ok(false)
    }

    pub fn run(&mut self) -> anyhow::Result<String> {
        for loop_idx in 1..=self.agent_loops {
            self.reload_task_context_if_needed()?;
            let (loop_summary, author_answer, status) = self.run_author_phase(loop_idx)?;

            if status == Some(Status::Complete) {
                if self.verbose {
                    println!(
                        "\n✓ author agent reported STATUS: COMPLETE after loop {loop_idx} — task complete, stopping early."
                    );
                }
                self.pending_items.clear();
                break;
            }

            if self.run_review_phase(loop_idx, &loop_summary, &author_answer)? {
                break;
            }
        }

        Ok(format!(
            "completed {}/{} loop(s) — run: {} — artifacts: {}",
            self.loops_completed,
            self.agent_loops,
            self.run_id,
            self.paths.run_dir.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_status_complete() {
        let text = "Some preamble\nSTATUS: COMPLETE\nmore text";
        assert_eq!(extract_status(text), Some(Status::Complete));
    }

    #[test]
    fn extract_status_incomplete() {
        let text = "STATUS: INCOMPLETE\nNEXT STEPS:\n- do x";
        assert_eq!(extract_status(text), Some(Status::Incomplete));
    }

    #[test]
    fn extract_status_none_when_missing() {
        assert_eq!(extract_status("no status line here"), None);
    }

    #[test]
    fn extract_status_requires_leading_word_match() {
        assert_eq!(extract_status("STATUS: UNKNOWN"), None);
    }

    #[test]
    fn extract_pending_items_dash_and_star_bullets() {
        let text = "STATUS: INCOMPLETE\nNEXT STEPS:\n- first item\n* second item\n";
        let items = extract_pending_items(text);
        assert_eq!(items, vec!["first item".to_string(), "second item".to_string()]);
    }

    #[test]
    fn extract_pending_items_multi_digit_bullets_fixed() {
        let text = "STATUS: INCOMPLETE\nNEXT STEPS:\n9. ninth item\n10. tenth item\n11) eleventh item\n";
        let items = extract_pending_items(text);
        assert_eq!(
            items,
            vec!["ninth item".to_string(), "tenth item".to_string(), "eleventh item".to_string()]
        );
    }

    #[test]
    fn extract_pending_items_stops_at_heading() {
        let text = "STATUS: INCOMPLETE\nNEXT STEPS:\n- item one\n# Heading\n- should not appear";
        let items = extract_pending_items(text);
        assert_eq!(items, vec!["item one".to_string()]);
    }

    #[test]
    fn extract_pending_items_caps_at_five() {
        let text = "STATUS: INCOMPLETE\nNEXT STEPS:\n- a\n- b\n- c\n- d\n- e\n- f\n- g\n";
        let items = extract_pending_items(text);
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn ensure_within_workdir_blocks_escape() {
        let dir = std::env::temp_dir().join(format!(
            "csloop_test_loop_workdir_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let task = dir.join("task.toml");
        std::fs::write(&task, "[[chat.user]]\ncontent = 'x'\n").unwrap();

        let ok = ensure_within_workdir(&task, &dir);
        assert!(ok.is_ok());

        let escape = ensure_within_workdir(Path::new("../../../../etc/passwd"), &dir);
        assert!(escape.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
