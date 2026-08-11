// Copyright (c) 2026 UChicago Argonne LLC
// CS-Loop (SF-26-122)
// SPDX-License-Identifier: GPL-3.0-only
// Full license and notices: see LICENSE and NOTICE in the repo root.

mod agent;
mod chat_template;
mod cli;
mod logging;
mod loop_runner;
mod model;
mod observer;
mod paths;
mod telemetry;
mod tools;

use clap::Parser;

use cli::Cli;
use loop_runner::{PromptLoopRunner, RunnerConfig};

fn main() {
    let cli = Cli::parse();
    let logging = cli::resolve_logging(cli.log_enabled, cli.log_path);

    let cfg = RunnerConfig {
        task_file: cli.task_file,
        model: cli.model,
        agent_loops: cli.agent_loops,
        agent_iterations: cli.agent_iterations,
        verbose: cli.verbose,
        logging,
        workdir: cli.workdir,
        reason: cli.reason,
    };

    let result = PromptLoopRunner::new(cfg).and_then(|mut runner| runner.run());

    match result {
        Ok(summary) => println!("{summary}"),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}
