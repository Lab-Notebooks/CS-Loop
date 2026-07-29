//! Low-level telemetry primitives: timestamps, run ids, atomic TOML/text writes,
//! and the append-only `[[event]]` TOML log used for tool-call diagnostics.
//!
//! Ports `codescribe/lib/_logging.py`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use time::OffsetDateTime;

pub fn iso_utc_now() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Locally-unique run id: UTC timestamp + a PID/timestamp-derived hex suffix.
///
/// Deliberately not Python's `local-time + secrets.token_hex(3)` — run ids are directory
/// names, not security tokens, so cryptographic randomness and local-time-of-day aren't
/// load-bearing here (see project decision to avoid a `rand` dependency).
pub fn new_run_id() -> String {
    let now = OffsetDateTime::now_utc();
    let ts = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let pid = std::process::id() as u64;
    let nanos = now.nanosecond() as u64;
    let suffix = pid.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(nanos) & 0xFF_FFFF;
    format!("{ts}-{suffix:06x}")
}

/// Atomically write `text` to `path` (write to a sibling `.tmp` file, then rename).
pub fn atomic_write_text(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// Atomically write a TOML-serializable value to `path`.
pub fn atomic_write_toml<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let text = toml::to_string(value)?;
    atomic_write_text(path, &text)?;
    Ok(())
}

/// Read a TOML file, returning an empty table if missing or unreadable.
pub fn read_toml(path: &Path) -> toml::Table {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => toml::Table::new(),
    }
}

// ---------------------------------------------------------------------------
// Append-only TOML event log
//
// Writes are hand-rolled raw-text appends (O(1) per event, no in-memory history,
// crash-safe under interruption) rather than "collect a Vec<Event> and rewrite the
// whole file", matching the append-only design of `_logging.py::append_toml_event`.
// Reads use the real `toml` parser since they're infrequent (inspection-only).
// ---------------------------------------------------------------------------

fn escape_basic(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\t', "\\t")
}

fn escape_basic_with_newlines(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Render a JSON value as a TOML literal, matching `_logging.py::_toml_val`.
/// Returns `None` for `Value::Null` (caller should omit the field entirely).
fn toml_val(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(if *b { "true".to_string() } else { "false".to_string() }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Some(u.to_string())
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_nan() {
                    Some("nan".to_string())
                } else if f.is_infinite() {
                    Some(if f > 0.0 { "inf".to_string() } else { "-inf".to_string() })
                } else {
                    Some(format!("{f:.6}"))
                }
            }
        }
        serde_json::Value::String(s) => {
            if s.contains('\n') || s.contains('\r') {
                if !s.contains("'''") {
                    Some(format!("'''\n{s}'''"))
                } else {
                    Some(format!("\"{}\"", escape_basic_with_newlines(s)))
                }
            } else {
                Some(format!("\"{}\"", escape_basic(s)))
            }
        }
        v @ (serde_json::Value::Array(_) | serde_json::Value::Object(_)) => {
            let encoded = serde_json::to_string(v).ok()?;
            Some(format!("\"{}\"", escape_basic_with_newlines(&encoded)))
        }
    }
}

/// Append one event as a `[[event]]` block to a TOML log file (raw text append, no
/// re-parse of existing content).
pub fn append_toml_event(path: &Path, event: &serde_json::Map<String, serde_json::Value>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut buf = String::from("\n[[event]]");
    for (k, v) in event {
        if let Some(rendered) = toml_val(v) {
            buf.push('\n');
            buf.push_str(k);
            buf.push_str(" = ");
            buf.push_str(&rendered);
        }
    }
    buf.push('\n');
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(buf.as_bytes())
}

// `read_toml_events` (and its helpers below) is inspection-only tooling ported for full
// parity with `_logging.py` — nothing in the `loop` command itself reads events back
// during a run, so it's unused by this binary today but kept as public API surface.
#[allow(dead_code)]
fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::json!(*f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(arr) => serde_json::Value::Array(arr.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect())
        }
    }
}

/// Fields that are JSON-encoded-as-string by `append_toml_event` for complex
/// (object/array) values, and should be decoded back on read. Matches Python's
/// `_JSON_FIELDS` allowlist exactly — other strings are left alone.
#[allow(dead_code)]
const JSON_FIELDS: &[&str] = &["args", "usage"];

/// Read all `[[event]]` entries from a TOML log file (inspection-only; not on the hot path).
#[allow(dead_code)]
pub fn read_toml_events(path: &Path) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return Vec::new();
    };
    let events = doc
        .get("event")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    events
        .into_iter()
        .filter_map(|ev| ev.as_table().cloned())
        .map(|table| {
            let mut out = serde_json::Map::new();
            for (k, v) in table {
                let jv = toml_to_json(&v);
                if JSON_FIELDS.contains(&k.as_str()) {
                    if let serde_json::Value::String(s) = &jv {
                        if let Ok(parsed) = serde_json::from_str(s) {
                            out.insert(k, parsed);
                            continue;
                        }
                    }
                }
                out.insert(k, jv);
            }
            out
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tool log sinks
// ---------------------------------------------------------------------------

pub trait ToolLogSink {
    fn emit(&self, event: &serde_json::Map<String, serde_json::Value>);
}

/// Kept for API completeness matching `_logging.py`'s `NullToolLogSink` — this binary's
/// `Agent` is always constructed with a real logging sink, so it's unconstructed today.
#[allow(dead_code)]
pub struct NullToolLogSink;

impl ToolLogSink for NullToolLogSink {
    fn emit(&self, _event: &serde_json::Map<String, serde_json::Value>) {}
}

/// Fan-out sink: emits each event to all child sinks, swallowing per-sink errors
/// (matches Python's `MultiToolLogSink` try/except-pass behavior).
pub struct MultiToolLogSink {
    sinks: Vec<Box<dyn ToolLogSink>>,
}

impl MultiToolLogSink {
    pub fn new(sinks: Vec<Box<dyn ToolLogSink>>) -> Self {
        Self { sinks }
    }
}

impl ToolLogSink for MultiToolLogSink {
    fn emit(&self, event: &serde_json::Map<String, serde_json::Value>) {
        for sink in &self.sinks {
            sink.emit(event);
        }
    }
}

/// Append-only TOML event log sink. Defaults to `.csloop/logs/toolusage.toml` when no
/// path (or an empty path) is given.
pub struct ToolLogToml {
    path: PathBuf,
}

impl ToolLogToml {
    pub fn new(path: Option<String>) -> Self {
        let path = match path {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => PathBuf::from(".csloop").join("logs").join("toolusage.toml"),
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Self { path }
    }
}

impl ToolLogSink for ToolLogToml {
    fn emit(&self, event: &serde_json::Map<String, serde_json::Value>) {
        let mut event = event.clone();
        event
            .entry("ts".to_string())
            .or_insert_with(|| serde_json::Value::String(iso_utc_now()));
        let _ = append_toml_event(&self.path, &event);
    }
}

/// Monotonic timer (matches Python's `Timer` using `time.perf_counter()`).
pub struct Timer {
    start: std::time::Instant,
}

impl Timer {
    pub fn new() -> Self {
        Self { start: std::time::Instant::now() }
    }

    pub fn ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("csloop_test_{tag}_{nanos}")).join("events.toml")
    }

    #[test]
    fn toml_event_log_round_trip() {
        let path = unique_test_path("log_roundtrip");

        let mut ev1 = serde_json::Map::new();
        ev1.insert("event".to_string(), serde_json::Value::String("tool_start".to_string()));
        ev1.insert("iteration".to_string(), serde_json::json!(1));
        ev1.insert("args".to_string(), serde_json::json!({"path": "a.txt", "nested": [1, 2, 3]}));

        let mut ev2 = serde_json::Map::new();
        ev2.insert("event".to_string(), serde_json::Value::String("multiline".to_string()));
        ev2.insert("text".to_string(), serde_json::Value::String("line one\nline two".to_string()));

        append_toml_event(&path, &ev1).unwrap();
        append_toml_event(&path, &ev2).unwrap();

        let events = read_toml_events(&path);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].get("event").and_then(|v| v.as_str()), Some("tool_start"));
        // "args" is JSON-encoded-as-string on write and must decode back to a structured value.
        assert_eq!(
            events[0].get("args").and_then(|v| v.get("path")).and_then(|v| v.as_str()),
            Some("a.txt")
        );
        assert_eq!(
            events[0].get("args").and_then(|v| v.get("nested")).and_then(|v| v.as_array()).map(|a| a.len()),
            Some(3)
        );
        assert_eq!(events[1].get("text").and_then(|v| v.as_str()), Some("line one\nline two"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn atomic_write_toml_creates_parent_dirs() {
        let path = unique_test_path("atomic_write");
        #[derive(Serialize)]
        struct Doc {
            a: u32,
        }
        atomic_write_toml(&path, &Doc { a: 42 }).unwrap();
        let read_back = read_toml(&path);
        assert_eq!(read_back.get("a").and_then(|v| v.as_integer()), Some(42));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
