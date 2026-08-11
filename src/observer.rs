// Copyright (c) 2026 UChicago Argonne LLC
// CS-Loop (SF-26-122)
// SPDX-License-Identifier: GPL-3.0-only
// Full license and notices: see LICENSE and NOTICE in the repo root.

//! Verbose-mode console tracking: per-iteration spinner, dimmed reasoning printing,
//! tool start/end one-liners, comma-grouped token usage lines.
//!
//! Ports `codescribe/lib/_agent.py`'s `RunObserver`/`ConsoleObserver`.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

use crate::agent::{summarize_tool_output, RunResult, TokenUsage};

/// RAII guard for the duration of one blocking model call. Dropping it ends whatever
/// per-iteration presentation (e.g. a spinner) was started — the Rust replacement for
/// Python's `@contextmanager def model_call(...)`.
pub trait ModelCallGuard {}

pub trait RunObserver {
    fn on_run_start(&self, _run_id: &str, _max_iterations: u32) {}
    fn model_call_start(&self, iteration: u32) -> Box<dyn ModelCallGuard>;
    fn on_model_response(&self, _iteration: u32, _usage: &TokenUsage, _reasoning: &str) {}
    fn on_tool_start(&self, _name: &str, _args_preview: &str) {}
    fn on_tool_end(&self, _name: &str, _output: &str) {}
    fn on_run_end(&self, _result: &RunResult) {}
}

// ---------------------------------------------------------------------------
// No-op observer (non-verbose mode)
// ---------------------------------------------------------------------------

pub struct NoopGuard;
impl ModelCallGuard for NoopGuard {}

pub struct NoopObserver;

impl RunObserver for NoopObserver {
    fn model_call_start(&self, _iteration: u32) -> Box<dyn ModelCallGuard> {
        Box::new(NoopGuard)
    }
}

// ---------------------------------------------------------------------------
// Console observer (verbose mode)
// ---------------------------------------------------------------------------

pub struct ConsoleObserver;

struct SpinnerGuard {
    bar: ProgressBar,
    start: Instant,
}

impl ModelCallGuard for SpinnerGuard {}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.bar.finish_with_message(format!("done in {:.1}s", elapsed.as_secs_f64()));
    }
}

/// Comma-group a number's digits (Rust has no `{:,}` formatter, unlike Python's `f"{n:,}"`).
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let chunks: Vec<&[u8]> = bytes.rchunks(3).collect();
    chunks
        .iter()
        .rev()
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(",")
}

fn cache_part(usage: &TokenUsage) -> String {
    if usage.cache_write > 0 || usage.cache_read > 0 {
        format!("  cache_write {}  cache_read {}", grouped(usage.cache_write), grouped(usage.cache_read))
    } else {
        String::new()
    }
}

fn print_reasoning_block(text: &str) {
    let use_ansi = std::io::stdout().is_terminal();
    let (dim, reset) = if use_ansi { ("\x1b[2m", "\x1b[0m") } else { ("", "") };
    if text.is_empty() {
        println!("    │ {dim}{reset}");
        return;
    }
    for line in text.lines() {
        println!("    │ {dim}{line}{reset}");
    }
}

fn diagnostic_result_hint(name: &str, output: &str) -> String {
    let summary = summarize_tool_output(name, output);
    let first = summary.lines().next().unwrap_or("(empty)").trim();
    if first.chars().count() > 70 {
        let truncated: String = first.chars().take(70).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}

impl RunObserver for ConsoleObserver {
    fn model_call_start(&self, iteration: u32) -> Box<dyn ModelCallGuard> {
        let bar = ProgressBar::new_spinner();
        if let Ok(style) = ProgressStyle::with_template("  iter {msg}  {spinner}  ({elapsed})") {
            bar.set_style(style.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⠋"));
        }
        bar.set_message(iteration.to_string());
        bar.enable_steady_tick(Duration::from_millis(120));
        Box::new(SpinnerGuard { bar, start: Instant::now() })
    }

    fn on_model_response(&self, _iteration: u32, usage: &TokenUsage, reasoning: &str) {
        if !reasoning.is_empty() {
            print_reasoning_block(reasoning);
        }
        let rsn_part = if usage.reasoning > 0 { format!("  rsn {}", grouped(usage.reasoning)) } else { String::new() };
        println!(
            "    usage  in {}  out {}{}{}  total {}",
            grouped(usage.input),
            grouped(usage.output),
            rsn_part,
            cache_part(usage),
            grouped(usage.total()),
        );
    }

    fn on_tool_start(&self, name: &str, args_preview: &str) {
        use std::io::Write;
        print!("    ▸ {name:<5}  {args_preview:<60}");
        let _ = std::io::stdout().flush();
    }

    fn on_tool_end(&self, name: &str, output: &str) {
        println!("  {}", diagnostic_result_hint(name, output));
    }

    fn on_run_end(&self, result: &RunResult) {
        let u = &result.usage;
        let rsn_part = if u.reasoning > 0 { format!("  rsn {}", grouped(u.reasoning)) } else { String::new() };
        println!();
        println!(
            "  tokens  in {}  out {}{}{}  total {}",
            grouped(u.input),
            grouped(u.output),
            rsn_part,
            cache_part(u),
            grouped(u.total()),
        );
    }
}
