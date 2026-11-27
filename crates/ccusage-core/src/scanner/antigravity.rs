//! Antigravity transcript discovery and read-only metadata decoding.
use crate::types::TimestampedUsageEvent;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use url::Url;
use uuid::Uuid;

/// A transcript path paired with its conversation identifier.
pub fn transcripts(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(root.join("brain")) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new())
        }
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if Uuid::parse_str(&id).is_err() {
            continue;
        }
        let logs = entry.path().join(".system_generated/logs");
        for name in ["transcript.jsonl", "transcript_full.jsonl"] {
            let path = logs.join(name);
            if path.is_file() {
                paths.push((id.clone(), path.canonicalize()?));
                break;
            }
        }
    }
    paths.sort();
    Ok(paths)
}

#[derive(Deserialize)]
struct TranscriptRow {
    step_index: i64,
    source: String,
    created_at: String,
    model: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
}

/// Decode a single complete transcript line. The identity is stable per conversation and step.
pub fn parse_line(
    line: &[u8],
    conversation: &str,
    directory: Option<&str>,
) -> Option<TimestampedUsageEvent> {
    let row: TranscriptRow = serde_json::from_slice(line).ok()?;
    if row.source != "MODEL" || row.model.is_empty() {
        return None;
    }
    let timestamp = DateTime::parse_from_rfc3339(&row.created_at)
        .ok()?
        .with_timezone(&Utc);
    let counts = [
        row.input_tokens.unwrap_or(0),
        row.output_tokens.unwrap_or(0),
        row.cache_read_tokens.unwrap_or(0),
        row.cache_creation_tokens.unwrap_or(0),
    ];
    if !counts.iter().any(|&count| count > 0)
        || counts
            .iter()
            .any(|&count| !(0..=i32::MAX as i64).contains(&count))
    {
        return None;
    }
    let mut model = row.model.as_str();
    let mut effort = None;
    for suffix in ["xhigh", "high", "medium", "low", "max", "minimal"] {
        if let Some(prefix) = model.strip_suffix(&format!("-{suffix}")) {
            model = prefix;
            effort = Some(suffix.to_owned());
            break;
        }
    }
    let step = row.step_index.to_string();
    Some(TimestampedUsageEvent {
        identity: format!("{conversation}\u{1f}{step}"),
        timestamp,
        agent: "agy".into(),
        session_id: conversation.into(),
        request_id: step.clone(),
        message_id: step,
        model: model.into(),
        input_tokens: counts[0] as u64,
        output_tokens: counts[1] as u64,
        cache_read_tokens: counts[2] as u64,
        cache_creation_tokens: counts[3] as u64,
        cache_creation_5m_tokens: 0,
        cache_creation_1h_tokens: 0,
        directory: directory.map(str::to_owned),
        effort,
    })
}

/// Read indexed workspace paths without opening the database for writes.
pub fn workspaces(root: &Path) -> Option<HashMap<String, String>> {
    let path = root.join("conversation_summaries.db");
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let shm = PathBuf::from(format!("{}-shm", path.display()));
    if wal.exists() {
        if !shm.exists() {
            return None;
        }
        return read_workspaces(&path.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY);
    }
    let path = path.canonicalize().ok()?;
    let mut uri = Url::from_file_path(&path).ok()?;
    uri.set_query(Some("immutable=1"));
    read_workspaces(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn read_workspaces(path: &str, flags: OpenFlags) -> Option<HashMap<String, String>> {
    let connection = Connection::open_with_flags(path, flags).ok()?;
    connection
        .busy_timeout(std::time::Duration::from_millis(200))
        .ok()?;
    let mut statement = connection
        .prepare("SELECT conversation_id, workspace_uris FROM conversation_summaries")
        .ok()?;
    let mut rows = statement.query([]).ok()?;
    let mut result = HashMap::new();
    while let Some(row) = rows.next().ok()? {
        let (Ok(Some(id)), Ok(Some(raw))) = (
            row.get::<_, Option<String>>(0),
            row.get::<_, Option<String>>(1),
        ) else {
            continue;
        };
        let Some(path) = serde_json::from_str::<Vec<String>>(&raw)
            .ok()
            .and_then(|items| items.into_iter().next())
            .and_then(|item| Url::parse(&item).ok())
            .and_then(|url| url.to_file_path().ok())
            .filter(|path| path.is_absolute())
        else {
            continue;
        };
        result.insert(id, path.to_string_lossy().into_owned());
    }
    Some(result)
}
