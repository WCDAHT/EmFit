//! BenchLog: append-only JSONL of operation timings (STANDARDS §5.5).
//!
//! One event per line, written to the same directory as the application logs.
//! When an operation's speed matters to users — a full MFT sweep, an export —
//! the operation records how long it took, so a regression shows up in a file
//! instead of a feeling.
//!
//! Rules, per the standard:
//! - **Local only.** Never uploaded; these tools handle forensic evidence.
//! - **Fields are only ever added,** never renamed or removed, so old lines
//!   stay parseable next to new ones.
//! - **Best-effort.** Telemetry must never fail the operation it measures;
//!   callers log and ignore the error.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::service::logging;

/// File name inside the log directory.
pub const FILE_NAME: &str = "bench.jsonl";

/// Where events land: `<log dir>/bench.jsonl`.
pub fn path() -> Result<PathBuf> {
    Ok(logging::log_dir()?.join(FILE_NAME))
}

/// Append one event to the default BenchLog.
///
/// `fields` is an object of operation-specific extras merged in beside the
/// standard `ts` / `op` / `duration_ms` / `ok` keys (which win on collision).
/// Returns the path written, mostly so callers can surface it.
pub fn append(
    op: &str,
    duration: Duration,
    ok: bool,
    fields: serde_json::Value,
) -> Result<PathBuf> {
    let path = path()?;
    append_to(&path, op, duration, ok, fields)?;
    Ok(path)
}

/// As [`append`], to an explicit file. The testable core.
pub fn append_to(
    path: &Path,
    op: &str,
    duration: Duration,
    ok: bool,
    fields: serde_json::Value,
) -> Result<()> {
    let mut event = serde_json::Map::new();
    if let serde_json::Value::Object(extra) = fields {
        event.extend(extra);
    }
    // The standard keys win over any colliding extra.
    event.insert("ts".into(), timestamp().into());
    event.insert("op".into(), op.into());
    event.insert(
        "duration_ms".into(),
        u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .into(),
    );
    event.insert("ok".into(), ok.into());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;

    // One serde_json line per event — correct escaping for any volume label
    // or error text, unlike v1's hand-rolled writers.
    writeln!(file, "{}", serde_json::Value::Object(event)).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// RFC 3339 UTC, seconds precision — matches the example in STANDARDS §5.5.
fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_append_as_parseable_jsonl() {
        let dir = std::env::temp_dir().join(format!("emfit-benchlog-{}", std::process::id()));
        let file = dir.join("bench.jsonl");
        let _ = std::fs::remove_file(&file);

        append_to(
            &file,
            "scan_ntfs",
            Duration::from_millis(1843),
            true,
            serde_json::json!({ "volume": "C:", "records_read": 5 }),
        )
        .unwrap();
        append_to(
            &file,
            "scan_ntfs",
            Duration::from_millis(2),
            false,
            serde_json::json!({ "error": "denied \"quoted\"" }),
        )
        .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["op"], "scan_ntfs");
        assert_eq!(first["duration_ms"], 1843);
        assert_eq!(first["ok"], true);
        assert_eq!(first["volume"], "C:");
        assert!(first["ts"].as_str().unwrap().ends_with('Z'));

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["ok"], false);
        assert_eq!(second["error"], "denied \"quoted\"");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_standard_keys_win_over_colliding_extras() {
        let dir = std::env::temp_dir().join(format!("emfit-benchlog-c-{}", std::process::id()));
        let file = dir.join("bench.jsonl");

        append_to(
            &file,
            "real-op",
            Duration::ZERO,
            true,
            serde_json::json!({ "op": "forged", "extra": 1 }),
        )
        .unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        let event: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(event["op"], "real-op");
        assert_eq!(event["extra"], 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
