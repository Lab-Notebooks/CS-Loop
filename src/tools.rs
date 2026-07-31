//! Filesystem + shell tools for the coding agent.
//!
//! Ports `codescribe/lib/_tools.py`.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};

pub trait AgentTool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> &serde_json::Value;
    fn enabled(&self) -> bool;
    // Backs Agent::enable_tool/disable_tool, which are themselves unused today — kept
    // for API-shape completeness matching `_tools.py`'s `AgentTool`.
    #[allow(dead_code)]
    fn set_enabled(&mut self, v: bool);
    /// Never panics; failures are returned as `"Error: ..."` strings (matches the
    /// Python convention so `is_error_output` detection in `agent.rs` keeps working).
    fn run(&self, args: &serde_json::Value) -> String;

    fn to_openai_tool(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Path containment
// ---------------------------------------------------------------------------

/// Resolve `path` the way Python's `Path.resolve()` does: normalize `.`/`..` and follow
/// symlinks in whatever prefix exists, but tolerate a nonexistent final component (needed
/// so `write` can target a brand-new file). `std::fs::canonicalize` alone can't do this —
/// it hard-errors on any missing path component.
fn lexical_resolve(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => suffix.push(name.to_os_string()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut resolved = existing.canonicalize()?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

pub fn resolve_within_root(root: &Path, target: &str) -> anyhow::Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("root does not exist: {}", root.display()))?;
    let candidate = Path::new(target);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let resolved =
        lexical_resolve(&candidate).map_err(|_| anyhow!("Path escapes working directory: {target}"))?;
    if !resolved.starts_with(&root) {
        bail!("Path escapes working directory: {target}");
    }
    Ok(resolved)
}

/// True if any path component is `.git` (repo metadata/hooks/config) — used to keep the
/// agent's own write/edit tools from hand-writing git plumbing (e.g. `.git/config`,
/// `.git/hooks/*`) which could otherwise turn a later plain `git` invocation (via the
/// bounded bash tool) into arbitrary command execution.
pub fn is_git_internal<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().components().any(|c| c.as_os_str() == ".git")
}

/// True if an already-resolved path (as returned by `resolve_within_root`/`lexical_resolve`)
/// is one of `protected_paths` (e.g. the task file, which grants tool policy at construction
/// and must never be readable/writable through the agent's own sandboxed tools). Takes a
/// pre-resolved path rather than resolving internally, since `canonicalize()` requires the
/// target to exist and callers like `WriteTool` must also protect not-yet-existing paths.
pub fn is_protected_path(resolved_path: &Path, protected_paths: &HashSet<PathBuf>) -> bool {
    !protected_paths.is_empty() && protected_paths.contains(resolved_path)
}

// ---------------------------------------------------------------------------
// ReadTool
// ---------------------------------------------------------------------------

pub struct ReadTool {
    root: Option<PathBuf>,
    enabled: bool,
    description: String,
    parameters: serde_json::Value,
}

fn split_keepends(s: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        current.push(c);
        if c == '\n' {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

impl ReadTool {
    pub fn new(root: Option<PathBuf>) -> Self {
        let mut desc = "Read a text file. Supports optional 1-indexed offset and line limit. \
                         By default, prefixes each returned line with a 1-indexed line number."
            .to_string();
        if root.is_some() {
            desc.push_str(" Access is restricted to the working directory tree.");
        }
        let parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read."},
                "offset": {"type": "integer", "description": "1-indexed starting line number.", "minimum": 1},
                "limit": {"type": "integer", "description": "Maximum number of lines to read.", "minimum": 1},
                "with_line_numbers": {"type": "integer", "description": "Set to 1 (default) to prefix each returned line with its line number; 0 to return raw text."}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        Self { root, enabled: true, description: desc, parameters }
    }
}

impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = v;
    }

    fn run(&self, args: &serde_json::Value) -> String {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return "Error: missing required argument 'path'".to_string(),
        };
        let offset = args.get("offset").and_then(|v| v.as_i64());
        let limit = args.get("limit").and_then(|v| v.as_i64());
        let with_line_numbers = args.get("with_line_numbers").and_then(|v| v.as_i64()).unwrap_or(1) != 0;

        let resolved: PathBuf = if let Some(root) = &self.root {
            match resolve_within_root(root, path) {
                Ok(p) => p,
                Err(e) => return format!("Error: {e}"),
            }
        } else {
            PathBuf::from(path)
        };

        if !resolved.exists() {
            return format!("Error: file not found: {}", resolved.display());
        }
        if !resolved.is_file() {
            return format!("Error: not a file: {}", resolved.display());
        }

        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(e) => return format!("Error: {e}"),
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let lines = split_keepends(&content);

        let start = (offset.unwrap_or(1).max(1) - 1) as usize;
        if start >= lines.len() {
            return String::new();
        }
        let end_unclamped = match limit {
            Some(l) => start + (l.max(1) as usize),
            None => lines.len(),
        };
        let end_clamped = end_unclamped.min(lines.len());
        let chunk = &lines[start..end_clamped];
        if chunk.is_empty() {
            return String::new();
        }

        if !with_line_numbers {
            return chunk.concat();
        }

        let width = std::cmp::max(4, end_unclamped.to_string().len());
        let header = format!(
            "# path: {}\n# lines: {}-{}\n",
            resolved.display(),
            start + 1,
            end_clamped
        );
        let mut numbered = String::new();
        for (i, line) in chunk.iter().enumerate() {
            numbered.push_str(&format!("{:0width$}: {line}", start + i + 1, width = width));
        }
        header + &numbered
    }
}

// ---------------------------------------------------------------------------
// GlobTool
// ---------------------------------------------------------------------------

pub struct GlobTool {
    root: Option<PathBuf>,
    enabled: bool,
    description: String,
    parameters: serde_json::Value,
}

impl GlobTool {
    pub fn new(root: Option<PathBuf>) -> Self {
        let mut desc = "List files matching a glob pattern (e.g. '**/*.py'). \
                         Returns newline-separated paths (relative to root when bounded)."
            .to_string();
        if root.is_some() {
            desc.push_str(" Access is restricted to the working directory tree.");
        }
        let parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern (supports **)."},
                "root": {"type": "string", "description": "Optional root directory for the search (default: current/bounded root)."},
                "include_dirs": {"type": "integer", "description": "Set to 1 to include directories; 0/omitted to include only files."},
                "limit": {"type": "integer", "description": "Maximum number of results to return.", "minimum": 1}
            },
            "required": ["pattern"],
            "additionalProperties": false
        });
        Self { root, enabled: true, description: desc, parameters }
    }
}

impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = v;
    }

    fn run(&self, args: &serde_json::Value) -> String {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return "Error: missing required argument 'pattern'".to_string(),
        };
        let root_arg = args.get("root").and_then(|v| v.as_str());
        let include_dirs = args.get("include_dirs").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
        let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(2000).max(1) as usize;

        let base: PathBuf = if let Some(root) = &self.root {
            match root_arg {
                Some(r) => match resolve_within_root(root, r) {
                    Ok(p) => p,
                    Err(e) => return format!("Error: {e}"),
                },
                None => root.clone(),
            }
        } else {
            match root_arg {
                Some(r) => match Path::new(r).canonicalize() {
                    Ok(p) => p,
                    Err(e) => return format!("Error: {e}"),
                },
                None => std::env::current_dir().unwrap_or_default(),
            }
        };

        let pattern_path = base.join(pattern);
        let entries = match glob::glob(&pattern_path.to_string_lossy()) {
            Ok(paths) => paths,
            Err(e) => return format!("Error: {e}"),
        };

        let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for entry in entries {
            let p = match entry {
                Ok(p) => p,
                Err(_) => continue,
            };
            let p = if let Some(root) = &self.root {
                match resolve_within_root(root, &p.to_string_lossy()) {
                    Ok(pp) => pp,
                    Err(_) => continue,
                }
            } else {
                match p.canonicalize() {
                    Ok(pp) => pp,
                    Err(_) => continue,
                }
            };
            if !include_dirs && p.is_dir() {
                continue;
            }
            let rel = p
                .strip_prefix(&base)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| p.to_string_lossy().to_string());
            out.insert(rel);
        }

        let mut out: Vec<String> = out.into_iter().collect();
        out.truncate(limit);
        out.join("\n")
    }
}

// ---------------------------------------------------------------------------
// BashTool
// ---------------------------------------------------------------------------

// Newline/carriage-return are deliberately included (a fix vs. the Python reference,
// which omits them): `validate_command` only inspects the FIRST shell token via
// `shell_words::split`, which tokenizes across newlines as if they were plain
// whitespace. Without blocking them here, a command like "ls\nrm -rf /" passes the
// allowlist check (first token "ls" is allowed) but `bash -c` still executes both
// lines as separate statements — a command-injection bypass of the allowlist itself.
const BLOCKED_CHARS: &[char] = &['|', '&', ';', '>', '<', '`', '$', '\n', '\r'];

pub fn default_allowed_commands() -> HashSet<String> {
    ["ls", "pwd", "find", "grep", "head", "tail", "wc", "git", "test", "echo", "sed"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub struct BashTool {
    cwd: Option<PathBuf>,
    bounded: bool,
    allowed_commands: HashSet<String>,
    protected_paths: HashSet<PathBuf>,
    enabled: bool,
    description: String,
    parameters: serde_json::Value,
}

fn run_with_timeout(
    mut child: std::process::Child,
    timeout: Option<Duration>,
) -> std::io::Result<std::process::Output> {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let status = match timeout {
        None => child.wait()?,
        Some(dur) => {
            let start = std::time::Instant::now();
            loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if start.elapsed() >= dur {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

impl BashTool {
    pub fn new(cwd: Option<PathBuf>, bounded: bool, allowed_commands: Option<HashSet<String>>) -> Self {
        let allowed_commands = allowed_commands.unwrap_or_else(default_allowed_commands);
        let mut desc = "Execute a bash command in the current working directory.".to_string();
        if cwd.is_some() {
            desc.push_str(
                " Commands execute with the working directory set to the bounded root.\
                 Only access files and paths within the working directory;\
                 do not navigate to or read from paths outside it.",
            );
        }
        if bounded {
            desc.push_str(" Potentially-dangerous shell syntax is blocked (pipes, redirects, $(), etc).");
            let mut allowed: Vec<&str> = allowed_commands.iter().map(|s| s.as_str()).collect();
            allowed.sort_unstable();
            desc.push_str(&format!(" Allowed commands: {}.", allowed.join(", ")));

            // Surfaced up front so the model doesn't burn iterations discovering these
            // by trial and error: each restriction below is enforced in validate_command
            // / harden_command, not just documented here.
            let mut caveats: Vec<&str> = Vec::new();
            if allowed_commands.contains("find") || allowed_commands.contains("bfs") || allowed_commands.contains("gfind")
            {
                caveats.push(
                    "find: -exec/-execdir/-ok/-okdir are rejected (they run an arbitrary program per match); \
                     use find only to locate paths, then act on results with other tools.",
                );
            }
            if allowed_commands.contains("rg") {
                caveats.push("rg: --pre/--pre-glob are rejected (they run an arbitrary preprocessor per file).");
            }
            if allowed_commands.contains("sed") {
                caveats.push(
                    "sed: always runs with --sandbox enforced, so e/w/r commands \
                     (e.g. '1e cmd', 's/x/y/e') are disabled; use sed only for text substitution.",
                );
            }
            if allowed_commands.contains("git") {
                caveats.push(
                    "git: the config subcommand and -c/-p/--paginate/--exec-path/--git-dir/\
                     --work-tree/--namespace/--upload-pack/--receive-pack/--upload-archive flags \
                     are rejected, as are ext:: and fd:: transport URLs.",
                );
            }
            if !caveats.is_empty() {
                desc.push(' ');
                desc.push_str(&caveats.join(" "));
            }
        }
        let parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Bash command to execute."},
                "timeout": {"type": "integer", "description": "Optional timeout in seconds.", "minimum": 1}
            },
            "required": ["command"],
            "additionalProperties": false
        });
        Self {
            cwd,
            bounded,
            allowed_commands,
            protected_paths: HashSet::new(),
            enabled: true,
            description: desc,
            parameters,
        }
    }

    /// Marks resolved paths (e.g. the task file) that no bash command may target, even to
    /// read — see `tools::is_protected_path`.
    pub fn with_protected_paths(mut self, protected_paths: HashSet<PathBuf>) -> Self {
        self.protected_paths = protected_paths;
        self
    }

    fn validate_command(&self, command: &str) -> Option<String> {
        if !self.bounded {
            return None;
        }
        if command.chars().any(|c| BLOCKED_CHARS.contains(&c)) {
            return Some("blocked shell syntax detected".to_string());
        }
        let parts = match shell_words::split(command) {
            Ok(p) => p,
            Err(_) => return Some("could not parse command".to_string()),
        };
        let exe = match parts.first() {
            Some(e) => e,
            None => return Some("empty command".to_string()),
        };
        if !self.allowed_commands.contains(exe.as_str()) {
            return Some(format!("command not allowed: {exe:?}"));
        }

        // Allowlisting the executable name isn't enough on its own: `find`/`bfs` and
        // `rg` have built-in flags that spawn an arbitrary program per matched file,
        // fully bypassing the allowlist without needing any of the blocked shell
        // metacharacters above. Reject those flags outright — there's no legitimate
        // use of them for a read-only search/inspection tool.
        if (exe == "find" || exe == "bfs" || exe == "gfind")
            && let Some(bad) = parts[1..]
                .iter()
                .find(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"))
        {
            return Some(format!(
                "disallowed {exe} flag {bad:?}: spawns an arbitrary program per match or deletes files"
            ));
        }
        if exe == "rg"
            && parts[1..]
                .iter()
                .any(|a| a == "--pre" || a == "--pre-glob" || a.starts_with("--pre=") || a.starts_with("--pre-glob="))
        {
            return Some("disallowed rg flag --pre/--pre-glob: spawns an arbitrary preprocessor program".to_string());
        }

        // git's own config surface is a full command-execution vector on its own, with no
        // blocked shell metacharacters needed: `config`/`-c` can set core.pager,
        // core.fsmonitor, alias.*, diff.external, etc, any of which turns a later plain
        // `git status`/`git log`/`git <alias>` into arbitrary command execution. The
        // remaining flags below extend the same escape (forcing pager execution, pointing
        // git at a different repo/exec path, or driving a remote-command flag), and
        // ext::/fd:: are transport-helper URL schemes that spawn an arbitrary program.
        if exe == "git" {
            if parts[1..].iter().any(|a| a == "config") {
                return Some(
                    "disallowed git subcommand 'config': can set core.pager/core.fsmonitor/alias.*/etc, \
                     turning a later plain git invocation into arbitrary command execution"
                        .to_string(),
                );
            }
            const GIT_FLAG_PREFIXES: &[&str] = &[
                "--exec-path",
                "--git-dir",
                "--work-tree",
                "--namespace",
                "--upload-pack",
                "--receive-pack",
                "--upload-archive",
            ];
            if let Some(bad) = parts[1..].iter().find(|a| {
                matches!(a.as_str(), "-c" | "-p" | "--paginate")
                    || GIT_FLAG_PREFIXES.iter().any(|p| a.starts_with(p))
            }) {
                return Some(format!("disallowed git flag {bad:?}: can override config/paths or force hook execution"));
            }
            if let Some(bad) = parts[1..].iter().find(|a| a.contains("ext::") || a.contains("fd::")) {
                return Some(format!(
                    "disallowed git argument {bad:?}: ext:: and fd:: transports spawn an arbitrary helper program"
                ));
            }
        }

        // `current_dir` alone only bounds where the process starts, not what its
        // arguments point at (e.g. `grep secret /etc/passwd`, `find / -name id_rsa`
        // would otherwise read anywhere the OS user can). Reject any non-flag argument
        // that resolves outside the workdir, the same containment check the file tools use.
        // Also reject any argument that resolves to a protected path (e.g. the task
        // file) — read or write — since tool-mediated access to it is never legitimate.
        if let Some(root) = &self.cwd {
            for arg in &parts[1..] {
                if arg.starts_with('-') {
                    continue;
                }
                let resolved = match resolve_within_root(root, arg) {
                    Ok(p) => p,
                    Err(_) => return Some(format!("argument escapes working directory: {arg:?}")),
                };
                if is_protected_path(&resolved, &self.protected_paths) {
                    return Some(format!("argument targets a protected path: {arg:?}"));
                }
            }
        }
        None
    }

    /// GNU sed's `e`/`w`/`r` commands (e.g. `1e some-command`, `s/x/y/e`) run arbitrary
    /// shell commands even inside a script with none of the blocked shell metacharacters.
    /// Rather than pattern-match every way the `e` flag can be spelled inside a sed
    /// script, force `--sandbox` (which GNU sed uses to disable e/w/r unconditionally)
    /// onto every bounded `sed` invocation that doesn't already request it.
    fn harden_command(command: &str) -> String {
        let Ok(mut parts) = shell_words::split(command) else { return command.to_string() };
        if parts.first().map(String::as_str) != Some("sed") {
            return command.to_string();
        }
        if parts.iter().any(|p| p == "--sandbox") {
            return command.to_string();
        }
        parts.insert(1, "--sandbox".to_string());
        shell_words::join(parts)
    }
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = v;
    }

    fn run(&self, args: &serde_json::Value) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => return "Error: missing required argument 'command'".to_string(),
        };
        if let Some(err) = self.validate_command(command) {
            return format!("Error: {err}");
        }
        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_i64())
            .map(|t| Duration::from_secs(t.max(0) as u64));

        let command = if self.bounded { Self::harden_command(command) } else { command.to_string() };

        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg(&command);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return format!("Error: {e}"),
        };

        let output = match run_with_timeout(child, timeout) {
            Ok(o) => o,
            Err(e) => return format!("Error: {e}"),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        let mut out = format!("exit_code: {code}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}");
        while out.ends_with(['\n', '\r', ' ', '\t']) {
            out.pop();
        }
        out.push('\n');
        out
    }
}

// ---------------------------------------------------------------------------
// EditTool
// ---------------------------------------------------------------------------

pub struct EditTool {
    root: Option<PathBuf>,
    protected_paths: HashSet<PathBuf>,
    enabled: bool,
    description: String,
    parameters: serde_json::Value,
}

impl EditTool {
    pub fn new(root: Option<PathBuf>) -> Self {
        let mut desc = "Edit a file using exact text replacements (all oldText must match exactly once).".to_string();
        if root.is_some() {
            desc.push_str(" Access is restricted to the working directory tree.");
        }
        let parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit."},
                "edits": {
                    "type": "array",
                    "description": "List of replacements.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    },
                    "minItems": 1
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        });
        Self { root, protected_paths: HashSet::new(), enabled: true, description: desc, parameters }
    }

    /// Marks resolved paths (e.g. the task file) that may never be edited — see
    /// `tools::is_protected_path`.
    pub fn with_protected_paths(mut self, protected_paths: HashSet<PathBuf>) -> Self {
        self.protected_paths = protected_paths;
        self
    }

    /// Compact snippet around a [start:end] char range, matching `EditTool.snippet`.
    fn snippet(text: &str, start: usize, end: usize, context: usize) -> String {
        let chars: Vec<char> = text.chars().collect();
        let left = start.saturating_sub(context);
        let right = (end + context).min(chars.len());
        let prefix = if left > 0 { "…" } else { "" };
        let suffix = if right < chars.len() { "…" } else { "" };
        let body: String = chars[left..right].iter().collect();
        format!("{prefix}{body}{suffix}")
    }
}

struct Replacement {
    index: usize,
    match_count: usize,
    old_len_chars: usize,
    new_len_chars: usize,
    before_snippet: String,
    old_text: String,
    new_text: String,
}

impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = v;
    }

    fn run(&self, args: &serde_json::Value) -> String {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return "Error: missing required argument 'path'".to_string(),
        };
        let edits = match args.get("edits").and_then(|v| v.as_array()) {
            Some(e) if !e.is_empty() => e,
            _ => return "Error: missing required argument 'edits'".to_string(),
        };

        let resolved = if let Some(root) = &self.root {
            match resolve_within_root(root, path) {
                Ok(p) => p,
                Err(e) => return format!("Error: {e}"),
            }
        } else {
            PathBuf::from(path)
        };

        if is_git_internal(&resolved) {
            return "Error: editing files under .git/ is not allowed".to_string();
        }
        if is_protected_path(&resolved, &self.protected_paths) {
            return "Error: editing this file is not allowed".to_string();
        }

        if !resolved.exists() {
            return format!("Error: file not found: {}", resolved.display());
        }
        if !resolved.is_file() {
            return format!("Error: not a file: {}", resolved.display());
        }

        let content_before = match std::fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return format!("Error: {e}"),
        };

        let mut replacements: Vec<Replacement> = Vec::new();
        for (idx, e) in edits.iter().enumerate() {
            let (old, new) = match (
                e.get("oldText").and_then(|v| v.as_str()),
                e.get("newText").and_then(|v| v.as_str()),
            ) {
                (Some(o), Some(n)) => (o, n),
                _ => return "Error: each edit must include oldText and newText".to_string(),
            };

            let count = content_before.matches(old).count();
            if count != 1 {
                return format!("Error: oldText must match exactly once; found {count} matches for: {old:?}");
            }
            let byte_start = content_before.find(old).unwrap();
            let byte_end = byte_start + old.len();
            let char_start = content_before[..byte_start].chars().count();
            let char_end = content_before[..byte_end].chars().count();
            replacements.push(Replacement {
                index: idx,
                match_count: count,
                old_len_chars: old.chars().count(),
                new_len_chars: new.chars().count(),
                before_snippet: Self::snippet(&content_before, char_start, char_end, 80),
                old_text: old.to_string(),
                new_text: new.to_string(),
            });
        }

        let mut content_after = content_before;
        for r in &replacements {
            content_after = content_after.replace(&r.old_text, &r.new_text);
        }

        if let Err(e) = std::fs::write(&resolved, &content_after) {
            return format!("Error: {e}");
        }

        let mut applied = Vec::new();
        for r in &replacements {
            let after_snippet = if let Some(byte_pos) = content_after.find(&r.new_text) {
                let char_pos = content_after[..byte_pos].chars().count();
                let char_end = char_pos + r.new_text.chars().count();
                Self::snippet(&content_after, char_pos, char_end, 80)
            } else {
                "(newText not found after write)".to_string()
            };
            applied.push(serde_json::json!({
                "index": r.index,
                "match_count": r.match_count,
                "old_len": r.old_len_chars,
                "new_len": r.new_len_chars,
                "before_snippet": r.before_snippet,
                "after_snippet": after_snippet,
            }));
        }

        let report = serde_json::json!({
            "ok": true,
            "path": resolved.display().to_string(),
            "applied": replacements.len(),
            "replacements": applied,
        });
        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "Error: failed to serialize report".to_string())
    }
}

// ---------------------------------------------------------------------------
// WriteTool
// ---------------------------------------------------------------------------

pub struct WriteTool {
    root: Option<PathBuf>,
    protected_paths: HashSet<PathBuf>,
    enabled: bool,
    description: String,
    parameters: serde_json::Value,
}

impl WriteTool {
    pub fn new(root: Option<PathBuf>) -> Self {
        let mut desc = "Write a file (create or overwrite).".to_string();
        if root.is_some() {
            desc.push_str(" Access is restricted to the working directory tree.");
        }
        let parameters = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to write."},
                "content": {"type": "string", "description": "Full content to write."}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        });
        Self { root, protected_paths: HashSet::new(), enabled: true, description: desc, parameters }
    }

    /// Marks resolved paths (e.g. the task file) that may never be written — see
    /// `tools::is_protected_path`.
    pub fn with_protected_paths(mut self, protected_paths: HashSet<PathBuf>) -> Self {
        self.protected_paths = protected_paths;
        self
    }
}

impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = v;
    }

    fn run(&self, args: &serde_json::Value) -> String {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return "Error: missing required argument 'path'".to_string(),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: missing required argument 'content'".to_string(),
        };

        let resolved = if let Some(root) = &self.root {
            match resolve_within_root(root, path) {
                Ok(p) => p,
                Err(e) => return format!("Error: {e}"),
            }
        } else {
            PathBuf::from(path)
        };

        if is_git_internal(&resolved) {
            return "Error: writing files under .git/ is not allowed".to_string();
        }
        if is_protected_path(&resolved, &self.protected_paths) {
            return "Error: writing this file is not allowed".to_string();
        }

        if let Some(parent) = resolved.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("Error: {e}");
            }
        }
        match std::fs::write(&resolved, content) {
            Ok(()) => format!("Wrote {} ({} bytes)", resolved.display(), content.len()),
            Err(e) => format!("Error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

/// `protected_paths` are resolved paths (e.g. the task file) that write/edit/bash may
/// never target, even though they live inside `root`.
pub fn make_tools(
    root: &Path,
    bash_allow: &HashSet<String>,
    protected_paths: &HashSet<PathBuf>,
) -> Vec<Box<dyn AgentTool>> {
    let mut allowed = default_allowed_commands();
    allowed.extend(bash_allow.iter().cloned());
    vec![
        Box::new(ReadTool::new(Some(root.to_path_buf()))),
        Box::new(GlobTool::new(Some(root.to_path_buf()))),
        Box::new(
            BashTool::new(Some(root.to_path_buf()), true, Some(allowed))
                .with_protected_paths(protected_paths.clone()),
        ),
        Box::new(EditTool::new(Some(root.to_path_buf())).with_protected_paths(protected_paths.clone())),
        Box::new(WriteTool::new(Some(root.to_path_buf())).with_protected_paths(protected_paths.clone())),
    ]
}

// Ported for parity with `_tools.py::make_readonly_tools` (used there by the
// read-only `inspect` command, which is out of scope for the loop-only csloop binary).
#[allow(dead_code)]
pub fn make_readonly_tools(root: &Path, bash_allow: &HashSet<String>) -> Vec<Box<dyn AgentTool>> {
    let mut allowed = default_allowed_commands();
    allowed.extend(bash_allow.iter().cloned());
    vec![
        Box::new(ReadTool::new(Some(root.to_path_buf()))),
        Box::new(GlobTool::new(Some(root.to_path_buf()))),
        Box::new(BashTool::new(Some(root.to_path_buf()), true, Some(allowed))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csloop_test_{tag}_{nanos}"))
    }

    #[test]
    fn bash_blocks_newline_command_injection() {
        // Regression test for a command-injection bypass: validate_command only
        // inspects the FIRST shell token, so without blocking newlines a string like
        // "ls\nrm -rf /" would pass the allowlist check (first token "ls" is allowed)
        // while `bash -c` still executes the second line unchecked.
        let allow: HashSet<String> = ["ls"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "ls\ntouch /tmp/csloop_should_not_run_marker"}));
        assert!(out.starts_with("Error:"), "expected newline-injected command to be rejected, got: {out}");
        assert!(!std::path::Path::new("/tmp/csloop_should_not_run_marker").exists());
    }

    #[test]
    fn bash_allows_single_allowed_command() {
        let allow: HashSet<String> = ["echo"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "echo hi"}));
        assert!(!out.starts_with("Error:"), "unexpected error: {out}");
        assert!(out.contains("hi"));
    }

    #[test]
    fn bash_description_surfaces_caveats_only_for_allowed_commands() {
        let with_all: HashSet<String> = ["find", "rg", "sed"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(with_all));
        assert!(tool.description().contains("-exec/-execdir/-ok/-okdir"));
        assert!(tool.description().contains("--pre/--pre-glob"));
        assert!(tool.description().contains("--sandbox"));

        let echo_only: HashSet<String> = ["echo"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(echo_only));
        assert!(!tool.description().contains("-exec"));
        assert!(!tool.description().contains("--pre"));
        assert!(!tool.description().contains("--sandbox"));
    }

    #[test]
    fn bash_rejects_disallowed_command() {
        let allow: HashSet<String> = ["ls"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "cat /etc/passwd"}));
        assert!(out.starts_with("Error:"));
    }

    #[test]
    fn bash_blocks_pipe_and_substitution_syntax() {
        let allow: HashSet<String> = ["echo"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        for cmd in ["echo hi | cat", "echo $(whoami)", "echo `whoami`", "echo hi; whoami"] {
            let out = tool.run(&serde_json::json!({"command": cmd}));
            assert!(out.starts_with("Error:"), "expected {cmd:?} to be rejected, got: {out}");
        }
    }

    #[test]
    fn bash_blocks_argument_path_escape() {
        // Regression test: `current_dir` alone doesn't stop an allowed command's
        // *arguments* from pointing outside the workdir (e.g. `grep secret /etc/passwd`).
        let dir = unique_test_dir("bash_arg_escape");
        std::fs::create_dir_all(&dir).unwrap();
        let outside = unique_test_dir("bash_arg_escape_outside");
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "TOP-SECRET").unwrap();

        let allow: HashSet<String> = ["grep", "find"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));

        let abs_out = tool.run(&serde_json::json!({"command": format!("grep TOP-SECRET {}", outside_file.display())}));
        assert!(abs_out.starts_with("Error:"), "expected absolute-path escape to be rejected, got: {abs_out}");
        assert!(!abs_out.contains("TOP-SECRET"));

        let traversal_out = tool.run(&serde_json::json!({"command": "grep TOP-SECRET ../bash_arg_escape_outside_nonexistent"}));
        assert!(traversal_out.starts_with("Error:"), "expected relative traversal to be rejected, got: {traversal_out}");

        let find_out = tool.run(&serde_json::json!({"command": "find / -name secret.txt"}));
        assert!(find_out.starts_with("Error:"), "expected absolute find root to be rejected, got: {find_out}");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn bash_allows_argument_within_workdir() {
        let dir = unique_test_dir("bash_arg_ok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), "hello world").unwrap();

        let allow: HashSet<String> = ["grep"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "grep hello file.txt"}));
        assert!(!out.starts_with("Error:"), "unexpected error for in-workdir argument: {out}");
        assert!(out.contains("hello world"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_requires_exactly_one_match() {
        let dir = unique_test_dir("edit_ambiguous");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "aa").unwrap();
        let tool = EditTool::new(Some(dir.clone()));
        let out = tool.run(&serde_json::json!({
            "path": "f.txt",
            "edits": [{"oldText": "a", "newText": "b"}]
        }));
        assert!(out.starts_with("Error:"), "expected ambiguous match to error, got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_applies_unique_match() {
        let dir = unique_test_dir("edit_unique");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let tool = EditTool::new(Some(dir.clone()));
        let out = tool.run(&serde_json::json!({
            "path": "f.txt",
            "edits": [{"oldText": "world", "newText": "there"}]
        }));
        assert!(!out.starts_with("Error:"), "unexpected error: {out}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_within_root_blocks_escape() {
        let dir = unique_test_dir("root_escape");
        std::fs::create_dir_all(&dir).unwrap();
        let result = resolve_within_root(&dir, "../../etc/passwd");
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_within_root_allows_new_file() {
        let dir = unique_test_dir("root_new_file");
        std::fs::create_dir_all(&dir).unwrap();
        let result = resolve_within_root(&dir, "brand_new_file.txt");
        assert!(result.is_ok(), "should allow resolving a path to a not-yet-existing file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_blocks_find_exec() {
        // Regression test: `find`'s -exec/-execdir/-ok/-okdir spawn an arbitrary
        // program per matched file, fully bypassing the command allowlist without
        // needing any of the blocked shell metacharacters.
        let dir = unique_test_dir("find_exec");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let allow: HashSet<String> = ["find"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        for cmd in ["find . -exec touch pwned {} +", "find . -execdir id {} ;", "find . -ok id {} +", "find . -okdir id {} +"] {
            let out = tool.run(&serde_json::json!({"command": cmd}));
            assert!(out.starts_with("Error:"), "expected {cmd:?} to be rejected, got: {out}");
        }
        assert!(!dir.join("pwned").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_find_without_exec_still_works() {
        let dir = unique_test_dir("find_ok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let allow: HashSet<String> = ["find"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "find . -name a.txt"}));
        assert!(!out.starts_with("Error:"), "unexpected error: {out}");
        assert!(out.contains("a.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_blocks_rg_pre() {
        // Regression test: ripgrep's --pre/--pre-glob run an arbitrary preprocessor
        // program per file, e.g. `rg --pre sh ...` executes the searched file's
        // contents as a shell script.
        let dir = unique_test_dir("rg_pre");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "echo INJECTED\ntouch RG_MARKER\n").unwrap();
        let allow: HashSet<String> = ["rg"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        for cmd in ["rg --pre sh -e INJECTED .", "rg --pre=sh -e INJECTED .", "rg --pre-glob '*.txt' --pre sh -e INJECTED ."] {
            let out = tool.run(&serde_json::json!({"command": cmd}));
            assert!(out.starts_with("Error:"), "expected {cmd:?} to be rejected, got: {out}");
        }
        assert!(!dir.join("RG_MARKER").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_sandboxes_sed_e_command() {
        // Regression test: GNU sed's `e` command (e.g. `1e some-command`, `s/x/y/e`)
        // executes arbitrary shell commands; `--sandbox` is force-injected to disable it.
        let dir = unique_test_dir("sed_e");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "line1\n").unwrap();
        let allow: HashSet<String> = ["sed"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "sed -n '1e echo INJECTED_BY_SED' f.txt"}));
        assert!(!out.contains("INJECTED_BY_SED"), "sed e-command should be sandboxed, got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_sed_normal_substitution_still_works() {
        let dir = unique_test_dir("sed_ok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "hello world\n").unwrap();
        let allow: HashSet<String> = ["sed"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "sed -i 's/world/there/' f.txt"}));
        assert!(!out.starts_with("Error:"), "unexpected error: {out}");
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "hello there\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_blocks_git_config_subcommand() {
        // Regression test: `git config core.pager=...`/`core.fsmonitor=...`/`alias.*`
        // turn a later plain `git status`/`git log`/`git <alias>` into arbitrary command
        // execution, with none of the blocked shell metacharacters needed.
        let allow: HashSet<String> = ["git"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "git config core.pager touch"}));
        assert!(out.starts_with("Error:"), "expected git config to be rejected, got: {out}");
    }

    #[test]
    fn bash_blocks_git_dash_c_flag() {
        let allow: HashSet<String> = ["git"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "git -c core.fsmonitor=touch status"}));
        assert!(out.starts_with("Error:"), "expected git -c to be rejected, got: {out}");
    }

    #[test]
    fn bash_blocks_git_transport_helper_urls() {
        // Regression test: git's ext::/fd:: transport helpers spawn an arbitrary program.
        let allow: HashSet<String> = ["git"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        for cmd in ["git clone ext::sh -c id /tmp/x", "git fetch fd::3"] {
            let out = tool.run(&serde_json::json!({"command": cmd}));
            assert!(out.starts_with("Error:"), "expected {cmd:?} to be rejected, got: {out}");
        }
    }

    #[test]
    fn bash_blocks_git_remote_command_flags() {
        let allow: HashSet<String> = ["git"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(None, true, Some(allow));
        for cmd in ["git -p log", "git --upload-pack=evil fetch", "git --git-dir=/etc status"] {
            let out = tool.run(&serde_json::json!({"command": cmd}));
            assert!(out.starts_with("Error:"), "expected {cmd:?} to be rejected, got: {out}");
        }
    }

    #[test]
    fn bash_plain_git_status_still_allowed() {
        let dir = unique_test_dir("git_plain");
        std::fs::create_dir_all(&dir).unwrap();
        let allow: HashSet<String> = ["git"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        for cmd in ["git status", "git log", "git diff"] {
            let out = tool.run(&serde_json::json!({"command": cmd}));
            assert!(!out.starts_with("Error:"), "expected {cmd:?} to pass validation, got: {out}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_blocks_find_delete() {
        // Regression test: `find`'s -delete was the one destructive flag left unblocked
        // alongside -exec/-execdir/-ok/-okdir.
        let dir = unique_test_dir("find_delete");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let allow: HashSet<String> = ["find"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow));
        let out = tool.run(&serde_json::json!({"command": "find . -name a.txt -delete"}));
        assert!(out.starts_with("Error:"), "expected -delete to be rejected, got: {out}");
        assert!(dir.join("a.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_blocks_git_internal_path() {
        let dir = unique_test_dir("write_git_internal");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let tool = WriteTool::new(Some(dir.clone()));
        let out = tool.run(&serde_json::json!({"path": ".git/config", "content": "[core]\n\tpager = touch pwned\n"}));
        assert!(out.starts_with("Error:"), "expected .git write to be rejected, got: {out}");
        assert!(!dir.join(".git").join("config").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_blocks_git_internal_path() {
        let dir = unique_test_dir("edit_git_internal");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("config"), "orig").unwrap();
        let tool = EditTool::new(Some(dir.clone()));
        let out = tool.run(&serde_json::json!({
            "path": ".git/config",
            "edits": [{"oldText": "orig", "newText": "bad"}]
        }));
        assert!(out.starts_with("Error:"), "expected .git edit to be rejected, got: {out}");
        assert_eq!(std::fs::read_to_string(dir.join(".git").join("config")).unwrap(), "orig");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_blocks_protected_path() {
        // Regression test: the task file grants tool policy ([tools].bash) at construction
        // and must not be tool-writable, or an agent could rewrite it to grant itself new
        // bash commands mid-run.
        let dir = unique_test_dir("write_protected");
        std::fs::create_dir_all(&dir).unwrap();
        let task_file = dir.join("task.toml");
        std::fs::write(&task_file, "orig").unwrap();
        let protected = HashSet::from([task_file.canonicalize().unwrap()]);
        let tool = WriteTool::new(Some(dir.clone())).with_protected_paths(protected);
        let out = tool.run(&serde_json::json!({"path": "task.toml", "content": "evil"}));
        assert!(out.starts_with("Error:"), "expected protected-path write to be rejected, got: {out}");
        assert_eq!(std::fs::read_to_string(&task_file).unwrap(), "orig");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_blocks_protected_path() {
        let dir = unique_test_dir("edit_protected");
        std::fs::create_dir_all(&dir).unwrap();
        let task_file = dir.join("task.toml");
        std::fs::write(&task_file, "orig").unwrap();
        let protected = HashSet::from([task_file.canonicalize().unwrap()]);
        let tool = EditTool::new(Some(dir.clone())).with_protected_paths(protected);
        let out = tool.run(&serde_json::json!({
            "path": "task.toml",
            "edits": [{"oldText": "orig", "newText": "evil"}]
        }));
        assert!(out.starts_with("Error:"), "expected protected-path edit to be rejected, got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_blocks_protected_path_argument() {
        let dir = unique_test_dir("bash_protected");
        std::fs::create_dir_all(&dir).unwrap();
        let task_file = dir.join("task.toml");
        std::fs::write(&task_file, "secret plan").unwrap();
        let protected = HashSet::from([task_file.canonicalize().unwrap()]);
        let allow: HashSet<String> = ["grep", "find", "sed"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow)).with_protected_paths(protected);

        let out = tool.run(&serde_json::json!({"command": "grep secret task.toml"}));
        assert!(out.starts_with("Error:"), "expected read of protected path to be rejected, got: {out}");
        assert!(!out.contains("secret plan"));

        let delete_out = tool.run(&serde_json::json!({"command": "find . -name task.toml -delete"}));
        assert!(delete_out.starts_with("Error:"));
        assert!(task_file.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_allows_non_protected_path_argument() {
        let dir = unique_test_dir("bash_not_protected");
        std::fs::create_dir_all(&dir).unwrap();
        let task_file = dir.join("task.toml");
        std::fs::write(&task_file, "secret plan").unwrap();
        std::fs::write(dir.join("other.txt"), "hello world").unwrap();
        let protected = HashSet::from([task_file.canonicalize().unwrap()]);
        let allow: HashSet<String> = ["grep"].iter().map(|s| s.to_string()).collect();
        let tool = BashTool::new(Some(dir.clone()), true, Some(allow)).with_protected_paths(protected);

        let out = tool.run(&serde_json::json!({"command": "grep hello other.txt"}));
        assert!(!out.starts_with("Error:"), "unexpected error for non-protected file: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
