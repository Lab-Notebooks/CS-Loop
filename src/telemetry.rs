// Copyright (c) 2026 UChicago Argonne LLC
// CS-Loop (SF-26-122)
// SPDX-License-Identifier: GPL-3.0-only
// Full license and notices: see LICENSE and NOTICE in the repo root.

//! Durable per-phase telemetry (metadata/*.toml + manifest.toml) for plotting/inspection.
//!
//! Ports `codescribe/lib/_telemetry.py`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::{RejectedCall, ToolResult, TokenUsage};
use crate::logging::{atomic_write_toml, iso_utc_now};

pub fn ensure_loop_metadata_dir(loop_dir: &Path) -> std::io::Result<PathBuf> {
    let path = loop_dir.join("metadata");
    fs::create_dir_all(&path)?;
    Ok(path)
}

const MAX_FIELD_CHARS: usize = 500;

/// Recursively truncate long strings (e.g. a `write` call's full file `content`, or
/// `edit`'s `oldText`/`newText`) before they're persisted to metadata TOML. Mirrors the
/// 500-char cap already applied to `output_preview` — without this, `args` has no size
/// limit at all, so a tool call touching a large/secret-bearing file persists it in full,
/// in plaintext, to `<loop_dir>/metadata/*.toml` forever.
fn cap_strings(value: &serde_json::Value, limit: usize) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let char_count = s.chars().count();
            if char_count > limit {
                let truncated: String = s.chars().take(limit).collect();
                serde_json::Value::String(format!("{truncated}... [truncated, {char_count} chars total]"))
            } else {
                value.clone()
            }
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| cap_strings(v, limit)).collect())
        }
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), cap_strings(v, limit))).collect())
        }
        _ => value.clone(),
    }
}

/// Convert a JSON value into a TOML value tree. `Null` becomes an empty string, since
/// TOML has no null type and tool-call args occasionally carry JSON nulls for optional
/// fields; this keeps phase-metadata serialization infallible rather than panicking or
/// silently dropping the whole row on an unexpected null.
fn json_to_toml(v: &serde_json::Value) -> toml::Value {
    match v {
        serde_json::Value::Null => toml::Value::String(String::new()),
        serde_json::Value::Bool(b) => toml::Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => toml::Value::String(s.clone()),
        serde_json::Value::Array(arr) => toml::Value::Array(arr.iter().map(json_to_toml).collect()),
        serde_json::Value::Object(map) => {
            let mut t = toml::Table::new();
            for (k, v) in map {
                t.insert(k.clone(), json_to_toml(v));
            }
            toml::Value::Table(t)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn write_loop_phase_metadata(
    metadata_dir: &Path,
    run_id: &str,
    loop_index: u32,
    phase: &str,
    model: &str,
    task_file: &str,
    workdir: &str,
    stop_reason: &str,
    final_text_present: bool,
    usage: &TokenUsage,
    iterations: u32,
    tool_results: &[ToolResult],
    rejected_calls: &[RejectedCall],
    duration_s: f64,
) -> anyhow::Result<PathBuf> {
    let tool_errors = tool_results.iter().filter(|t| !t.ok).count();

    let mut usage_t = toml::Table::new();
    usage_t.insert("input".into(), toml::Value::Integer(usage.input as i64));
    usage_t.insert("output".into(), toml::Value::Integer(usage.output as i64));
    usage_t.insert("reasoning".into(), toml::Value::Integer(usage.reasoning as i64));
    usage_t.insert("cache_write".into(), toml::Value::Integer(usage.cache_write as i64));
    usage_t.insert("cache_read".into(), toml::Value::Integer(usage.cache_read as i64));

    let mut counts_t = toml::Table::new();
    counts_t.insert("executed".into(), toml::Value::Integer(tool_results.len() as i64));
    counts_t.insert("rejected".into(), toml::Value::Integer(rejected_calls.len() as i64));
    counts_t.insert("errors".into(), toml::Value::Integer(tool_errors as i64));
    counts_t.insert(
        "ok".into(),
        toml::Value::Integer((tool_results.len() - tool_errors) as i64),
    );

    let tools_arr: Vec<toml::Value> = tool_results
        .iter()
        .map(|t| {
            let mut row = toml::Table::new();
            row.insert("name".into(), toml::Value::String(t.name.clone()));
            row.insert("args".into(), json_to_toml(&cap_strings(&t.args, MAX_FIELD_CHARS)));
            row.insert("ok".into(), toml::Value::Boolean(t.ok));
            row.insert("output_preview".into(), toml::Value::String(t.output_preview.clone()));
            toml::Value::Table(row)
        })
        .collect();

    let rejected_arr: Vec<toml::Value> = rejected_calls
        .iter()
        .map(|r| {
            let mut row = toml::Table::new();
            row.insert("name".into(), toml::Value::String(r.name.clone()));
            row.insert("args".into(), json_to_toml(&cap_strings(&r.args, MAX_FIELD_CHARS)));
            row.insert("reason".into(), toml::Value::String(r.reason.as_str().to_string()));
            toml::Value::Table(row)
        })
        .collect();

    let mut doc = toml::Table::new();
    doc.insert("run_id".into(), toml::Value::String(run_id.to_string()));
    doc.insert("loop_index".into(), toml::Value::Integer(loop_index as i64));
    doc.insert("phase".into(), toml::Value::String(phase.to_string()));
    doc.insert("model".into(), toml::Value::String(model.to_string()));
    doc.insert("task_file".into(), toml::Value::String(task_file.to_string()));
    doc.insert("workdir".into(), toml::Value::String(workdir.to_string()));
    doc.insert("stop_reason".into(), toml::Value::String(stop_reason.to_string()));
    doc.insert("final_text_present".into(), toml::Value::Boolean(final_text_present));
    doc.insert("iterations".into(), toml::Value::Integer(iterations as i64));
    doc.insert(
        "duration_s".into(),
        toml::Value::Float((duration_s * 1000.0).round() / 1000.0),
    );
    doc.insert("usage".into(), toml::Value::Table(usage_t));
    doc.insert("tool_calls".into(), toml::Value::Table(counts_t));
    doc.insert("tools".into(), toml::Value::Array(tools_arr));
    doc.insert("rejected_calls".into(), toml::Value::Array(rejected_arr));
    doc.insert("created_at".into(), toml::Value::String(iso_utc_now()));

    let out = metadata_dir.join(format!("loop_{loop_index:03}_{phase}.toml"));
    atomic_write_toml(&out, &toml::Value::Table(doc))?;
    Ok(out)
}

pub fn write_loop_manifest(
    metadata_dir: &Path,
    run_doc: &toml::Table,
    phase_files: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    let mut names: Vec<String> = phase_files
        .iter()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .collect();
    names.sort();
    let names_arr: Vec<toml::Value> = names.into_iter().map(toml::Value::String).collect();

    let mut manifest = toml::Table::new();
    manifest.insert("run".into(), toml::Value::Table(run_doc.clone()));
    manifest.insert("phase_files".into(), toml::Value::Array(names_arr));
    manifest.insert("updated_at".into(), toml::Value::String(iso_utc_now()));

    let out = metadata_dir.join("manifest.toml");
    atomic_write_toml(&out, &toml::Value::Table(manifest))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{RejectReason, TokenUsage};

    fn unique_test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csloop_test_telemetry_{tag}_{nanos}"))
    }

    #[test]
    fn cap_strings_truncates_long_string() {
        let long = "x".repeat(10_000);
        let capped = cap_strings(&serde_json::json!(long), MAX_FIELD_CHARS);
        let s = capped.as_str().unwrap();
        assert!(s.len() < 10_000);
        assert!(s.contains("truncated, 10000 chars total"));
    }

    #[test]
    fn cap_strings_leaves_short_string_untouched() {
        let capped = cap_strings(&serde_json::json!("short"), MAX_FIELD_CHARS);
        assert_eq!(capped.as_str().unwrap(), "short");
    }

    #[test]
    fn cap_strings_recurses_into_nested_args() {
        let value = serde_json::json!({
            "path": "secret.env",
            "content": "x".repeat(10_000),
            "edits": [{"oldText": "y".repeat(10_000), "newText": "z"}]
        });
        let capped = cap_strings(&value, MAX_FIELD_CHARS);
        assert!(capped["content"].as_str().unwrap().len() < 10_000);
        assert!(capped["edits"][0]["oldText"].as_str().unwrap().len() < 10_000);
        assert_eq!(capped["edits"][0]["newText"].as_str().unwrap(), "z");
        assert_eq!(capped["path"].as_str().unwrap(), "secret.env");
    }

    #[test]
    fn write_loop_phase_metadata_caps_persisted_args() {
        // Regression test: unlike `output_preview` (already capped to 500 chars), `args`
        // had no size limit, so a `write` call's full file content — potentially a secret
        // — was persisted to metadata TOML in plaintext, uncapped.
        let dir = unique_test_dir("caps_args");
        std::fs::create_dir_all(&dir).unwrap();

        let big_content = "x".repeat(10_000);
        let tool_results = vec![crate::agent::ToolResult {
            name: "write".to_string(),
            args: serde_json::json!({"path": "secret.env", "content": big_content}),
            ok: true,
            output_preview: "Wrote secret.env".to_string(),
        }];

        let out = write_loop_phase_metadata(
            &dir,
            "r1",
            1,
            "author",
            "m",
            "t",
            "w",
            "done",
            true,
            &TokenUsage::default(),
            1,
            &tool_results,
            &[],
            1.0,
        )
        .unwrap();

        let doc: toml::Table = toml::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        let persisted_content = doc["tools"][0]["args"]["content"].as_str().unwrap();
        assert!(persisted_content.len() < 1000, "expected capped content, got {} chars", persisted_content.len());
        assert!(persisted_content.contains("truncated"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_loop_phase_metadata_caps_rejected_args_too() {
        let dir = unique_test_dir("caps_rejected_args");
        std::fs::create_dir_all(&dir).unwrap();

        let rejected = vec![crate::agent::RejectedCall {
            name: "write".to_string(),
            args: serde_json::json!({"content": "y".repeat(10_000)}),
            reason: RejectReason::RepeatBlocked,
        }];

        let out = write_loop_phase_metadata(
            &dir,
            "r1",
            1,
            "author",
            "m",
            "t",
            "w",
            "done",
            true,
            &TokenUsage::default(),
            1,
            &[],
            &rejected,
            1.0,
        )
        .unwrap();

        let doc: toml::Table = toml::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        let persisted_content = doc["rejected_calls"][0]["args"]["content"].as_str().unwrap();
        assert!(persisted_content.len() < 1000);

        std::fs::remove_dir_all(&dir).ok();
    }
}
