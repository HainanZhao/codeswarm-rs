//! Durable prompt history compatible with the retired Python client.
//!
//! Python CodeSwarm stored one JSON object per line (`input` and
//! `timestamp`).  Early Rust snapshots stored bare JSON strings instead.  The
//! reader accepts both shapes so upgrading does not silently discard a
//! user's prompt history; damaged or partially-written lines are ignored just
//! like the Python implementation.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Number of entries retained in memory by the prompt editor.
pub const MAX_HISTORY_ENTRIES: usize = 50;

/// Decode one history line.  `None` means that the line is malformed or does
/// not contain a string prompt.
pub fn parse_entry(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    match value {
        // Current Python-compatible representation.
        serde_json::Value::Object(object) => object
            .get("input")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        // Compatibility with the first Rust history writer.
        serde_json::Value::String(prompt) => Some(prompt),
        _ => None,
    }
}

/// Read valid prompts from a JSONL history file, retaining the newest bounded
/// window in original chronological order.  Missing files are empty history.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Some(prompt) = parse_entry(&line)
            && !prompt.trim().is_empty()
        {
            entries.push(prompt);
            if entries.len() > MAX_HISTORY_ENTRIES {
                entries.remove(0);
            }
        }
    }
    Ok(entries)
}

/// Append one prompt in the Python-compatible object format.
///
/// Empty prompts are deliberately no-ops.  The write is intentionally not
/// fsynced on the input/render path; session event persistence owns explicit
/// durability checkpoints and prompt history is recoverable convenience state.
pub fn append(path: impl AsRef<Path>, prompt: &str) -> io::Result<()> {
    if prompt.trim().is_empty() {
        return Ok(());
    }
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default();
    let record = serde_json::json!({ "input": prompt, "timestamp": timestamp });
    let encoded = serde_json::to_string(&record).map_err(io::Error::other)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(encoded.as_bytes())?;
    file.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{MAX_HISTORY_ENTRIES, append, parse_entry, read};

    fn temp_path(prefix: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("codeswarm-{prefix}-{unique}.jsonl"))
    }

    #[test]
    fn parses_python_and_early_rust_records_but_skips_damage() {
        assert_eq!(
            parse_entry(r#"{"input":"git status","timestamp":1}"#),
            Some("git status".into())
        );
        assert_eq!(parse_entry(r#""git log""#), Some("git log".into()));
        assert_eq!(parse_entry("not json"), None);
        assert_eq!(parse_entry("123"), None);
        assert_eq!(parse_entry(r#"{"input":7}"#), None);
    }

    #[test]
    fn reads_valid_entries_around_torn_lines_and_bounds_to_newest() {
        let path = temp_path("history-read");
        let mut content = String::new();
        for index in 0..(MAX_HISTORY_ENTRIES + 3) {
            content.push_str(&format!(
                r#"{{"input":"prompt-{index}","timestamp":{index}}}"#
            ));
            content.push('\n');
        }
        content.push_str("{\"input\":\"torn\n");
        std::fs::write(&path, content).expect("write");
        let entries = read(&path).expect("read");
        assert_eq!(entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(entries.first().map(String::as_str), Some("prompt-3"));
        assert_eq!(entries.last().map(String::as_str), Some("prompt-52"));
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn append_writes_python_compatible_record_and_empty_is_noop() {
        let path = temp_path("history-write");
        append(&path, "").expect("empty append");
        assert!(!path.exists());
        append(&path, "make verify").expect("append");
        let line = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(value["input"], "make verify");
        assert!(value["timestamp"].is_number());
        assert_eq!(read(&path).expect("history"), ["make verify"]);
        std::fs::remove_file(path).expect("cleanup");
    }
}
