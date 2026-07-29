//! CLI surface for `csloop` — a flat command (no subcommand) since the binary's only
//! job is running the author/review loop.
//!
//! Ports the `loop` command's `@click.option` decorations from
//! `codescribe/cli/_commands.py`. Two deliberate deviations, both necessary rather than
//! stylistic:
//! - `--model` takes a bare Anthropic model id (no `anthropic-`/`openai-`/`oaic-`
//!   prefix) since this binary is Anthropic-only.
//! - `-nloop`/`-niter` become long-only `--agent-loops`/`--agent-iterations` — clap's
//!   `.short()` is single-character only, so the literal multi-char short forms from
//!   the Python CLI aren't reproducible.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "csloop", version, about = "Bounded author/review agent loop against Anthropic models")]
pub struct Cli {
    /// TOML task file (chat template) describing the work to do.
    pub task_file: PathBuf,

    /// Anthropic model id, e.g. claude-sonnet-4-6.
    #[arg(short = 'm', long, env = "CSLOOP_MODEL")]
    pub model: String,

    /// Maximum number of bounded agent loops.
    #[arg(long = "agent-loops", default_value_t = 5)]
    pub agent_loops: u32,

    /// Maximum tool-call iterations per agent session.
    #[arg(long = "agent-iterations", default_value_t = 30)]
    pub agent_iterations: u32,

    /// Working directory bound for the agent; defaults to the current directory.
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Print agent diagnostics (per-iteration reasoning and tool calls) to stdout.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Write agent diagnostic events (TOML) to the default path: .csloop/logs/toolusage.toml.
    #[arg(long = "log")]
    pub log_enabled: bool,

    /// Write agent diagnostic events (TOML) to PATH (implies --log).
    #[arg(long = "log-path")]
    pub log_path: Option<String>,

    /// Enable adaptive thinking (extended reasoning).
    #[arg(long)]
    pub reason: bool,
}

pub fn resolve_logging(log_enabled: bool, log_path: Option<String>) -> Option<String> {
    if let Some(p) = log_path {
        return Some(p);
    }
    if log_enabled {
        // Empty string means "use default log path" in ToolLogToml.
        return Some(String::new());
    }
    None
}
