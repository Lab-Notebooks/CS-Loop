//! Artifact directory layout under `.csloop/loop/` (a new namespace, deliberately not
//! `.codescribe/loop/` — isolation from the Python tool's existing artifacts/tooling).
//!
//! Ports the paths section of `codescribe/lib/_loop.py`.

use std::path::{Path, PathBuf};

pub struct LoopPaths {
    pub run_dir: PathBuf,
    pub metadata_dir: PathBuf,
    pub run_toml: PathBuf,
    pub state_toml: PathBuf,
    pub author_toml: PathBuf,
    pub review_output_toml: PathBuf,
}

pub fn get_loop_paths(workdir: &Path) -> LoopPaths {
    let run_dir = workdir.join(".csloop").join("loop");
    LoopPaths {
        metadata_dir: run_dir.join("metadata"),
        run_toml: run_dir.join("run.toml"),
        state_toml: run_dir.join("state.toml"),
        author_toml: run_dir.join("author.toml"),
        review_output_toml: run_dir.join("review_output.toml"),
        run_dir,
    }
}
