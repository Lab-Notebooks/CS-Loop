// Copyright (c) 2026 UChicago Argonne LLC
// CS-Loop (SF-26-122)
// SPDX-License-Identifier: GPL-3.0-only
// Full license and notices: see LICENSE and NOTICE in the repo root.

//! Task-file TOML chat-template parsing and validation.
//!
//! Ports `codescribe/lib/_filetools.py::load_chat_template` only (the rest of that
//! Python module is Fortran-specific tooling, out of scope for csloop).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, bail, Context};

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct ChatTemplateMeta {
    pub bash_allow: HashSet<String>,
}

/// True if `s` looks like a bare command name (no args, no paths): `^[A-Za-z0-9_.+-]+$`.
fn is_valid_bash_tool_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
}

/// True if the raw TOML source contains a `content = """` block (triple *double* quotes),
/// which is disallowed — multi-line content must use triple single quotes.
fn has_bad_triple_double_quote_content(raw_text: &str) -> bool {
    let mut idx = 0usize;
    while let Some(pos) = raw_text[idx..].find("content") {
        let start = idx + pos + "content".len();
        if start >= raw_text.len() {
            break;
        }
        let rest = &raw_text[start..];
        let after_ws = rest.trim_start();
        if let Some(after_eq) = after_ws.strip_prefix('=') {
            if after_eq.trim_start().starts_with("\"\"\"") {
                return true;
            }
        }
        idx = start;
    }
    false
}

/// Position-ordered `[[chat.user]]` / `[[chat.assistant]]` header occurrences in the raw
/// source, recovering the interleaving order the TOML table structure itself loses.
fn declared_roles(raw_text: &str) -> Vec<&'static str> {
    let mut positions: Vec<(usize, &'static str)> = Vec::new();
    for (idx, _) in raw_text.match_indices("[[chat.user]]") {
        positions.push((idx, "user"));
    }
    for (idx, _) in raw_text.match_indices("[[chat.assistant]]") {
        positions.push((idx, "assistant"));
    }
    positions.sort_by_key(|(idx, _)| *idx);
    positions.into_iter().map(|(_, r)| r).collect()
}

pub fn load_chat_template(path: &Path) -> anyhow::Result<(Vec<ChatMessage>, ChatTemplateMeta)> {
    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
        bail!(
            "Unsupported file extension '{}'. Expected '.toml'.",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        );
    }

    let raw_text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read task file {}", path.display()))?;
    let data: toml::Value = toml::from_str(&raw_text)
        .with_context(|| format!("failed to parse TOML in {}", path.display()))?;

    // Optional [tools].bash allowlist extension.
    let mut meta = ChatTemplateMeta::default();
    if let Some(tools_cfg) = data.get("tools") {
        let table = tools_cfg
            .as_table()
            .ok_or_else(|| anyhow!("[tools] must be a table in {}", path.display()))?;
        if let Some(bash_val) = table.get("bash") {
            let arr = bash_val
                .as_array()
                .ok_or_else(|| anyhow!("[tools].bash must be a list of strings in {}", path.display()))?;
            for item in arr {
                let name = item
                    .as_str()
                    .ok_or_else(|| anyhow!("[tools].bash must be a list of strings in {}", path.display()))?;
                if !is_valid_bash_tool_name(name) {
                    bail!(
                        "Invalid bash tool name '{name}' in {}. Use a bare command name only \
                         (e.g. 'python3.8'), no args or paths.",
                        path.display()
                    );
                }
                meta.bash_allow.insert(name.to_string());
            }
        }
    }

    if has_bad_triple_double_quote_content(&raw_text) {
        bail!(
            "Invalid TOML quoting in {}: Use triple single quotes ('''...''') for multi-line \
             content blocks instead of triple double quotes (\"\"\"...\"\"\").",
            path.display()
        );
    }

    let chat_section = data
        .get("chat")
        .ok_or_else(|| anyhow!("'chat' key not found in TOML file: {}", path.display()))?;

    let mut chat_template: Vec<ChatMessage> = Vec::new();

    match chat_section {
        toml::Value::Array(entries) => {
            for entry in entries {
                let table = entry
                    .as_table()
                    .ok_or_else(|| anyhow!("Invalid [[chat]] entry in {}", path.display()))?;
                let role = table
                    .get("role")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("Invalid [[chat]] entry in {}", path.display()))?;
                let content = table
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("Invalid [[chat]] entry in {}", path.display()))?;
                if role != "user" && role != "assistant" {
                    bail!("Invalid role '{role}' in {}", path.display());
                }
                chat_template.push(ChatMessage { role: role.to_string(), content: content.to_string() });
            }
        }
        toml::Value::Table(chat_table) => {
            let roles = declared_roles(&raw_text);
            let mut counters: std::collections::HashMap<&str, usize> =
                [("user", 0usize), ("assistant", 0usize)].into_iter().collect();

            for role in roles {
                let entries = chat_table
                    .get(role)
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        anyhow!("[[chat.{role}]] section declared but not found in {}", path.display())
                    })?;
                let idx = counters[role];
                let entry = entries.get(idx).ok_or_else(|| {
                    anyhow!(
                        "More [[chat.{role}]] headers than parsed entries for {role} in {}",
                        path.display()
                    )
                })?;
                let content = entry
                    .as_table()
                    .and_then(|t| t.get("content"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("[[chat.{role}]] entry missing 'content' in {}", path.display()))?;
                chat_template.push(ChatMessage { role: role.to_string(), content: content.to_string() });
                *counters.get_mut(role).unwrap() += 1;
            }
        }
        _ => bail!("Unexpected 'chat' structure in {}", path.display()),
    }

    if chat_template.is_empty() {
        bail!("Chat template is empty.");
    }
    if chat_template[0].role != "user" {
        bail!("Conversation must start with a 'user' message.");
    }
    if chat_template.last().unwrap().role == "assistant" {
        bail!("Conversation must end with a 'user' message.");
    }
    for i in 1..chat_template.len() {
        if chat_template[i].role == chat_template[i - 1].role {
            bail!(
                "Invalid role order at positions {} and {}: two consecutive '{}' entries found.",
                i,
                i + 1,
                chat_template[i].role
            );
        }
    }

    Ok((chat_template, meta))
}
