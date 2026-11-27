use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub fn discover_jsonl(root: &Path, modified_since: Option<SystemTime>) -> io::Result<Vec<PathBuf>> {
    fn walk(root: &Path, floor: Option<SystemTime>, out: &mut Vec<PathBuf>) -> io::Result<()> {
        if !root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(&entry.path(), floor, out)?;
            } else if kind.is_file() && entry.path().extension().is_some_and(|v| v == "jsonl") {
                let metadata = entry.metadata()?;
                if floor.is_none_or(|f| metadata.modified().is_ok_and(|m| m >= f)) {
                    out.push(fs::canonicalize(entry.path())?);
                }
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    walk(root, modified_since, &mut result)?;
    result.sort();
    Ok(result)
}

/// Returns complete lines, the last complete offset, and the bytes actually read.
pub fn for_each_line_range(
    path: &Path,
    start: u64,
    end: u64,
    needles: &[&str],
) -> io::Result<(Vec<String>, u64, u64)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut bytes)?;
    let read = bytes.len() as u64;
    let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
    let lines = bytes[..complete]
        .split(|b| *b == b'\n')
        .filter(|v| !v.is_empty())
        .filter_map(|v| std::str::from_utf8(v).ok())
        .filter(|line| needles.is_empty() || needles.iter().any(|n| line.contains(n)))
        .map(str::to_owned)
        .collect();
    Ok((lines, start + complete as u64, read))
}

pub fn for_each_line_from_end(path: &Path, end: u64, needles: &[&str]) -> io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let mut cursor = end;
    let mut pending = Vec::new();
    let mut lines = Vec::new();
    while cursor > 0 {
        let count = cursor.min(8192);
        cursor -= count;
        file.seek(SeekFrom::Start(cursor))?;
        let mut chunk = vec![0; count as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&pending);
        let mut parts: Vec<_> = chunk.split(|b| *b == b'\n').collect();
        pending = parts.remove(0).to_vec();
        for part in parts.into_iter().rev() {
            if let Ok(line) = std::str::from_utf8(part) {
                if !line.is_empty()
                    && (needles.is_empty() || needles.iter().any(|n| line.contains(n)))
                {
                    lines.push(line.to_owned());
                }
            }
        }
    }
    if let Ok(line) = std::str::from_utf8(&pending) {
        if !line.is_empty() && (needles.is_empty() || needles.iter().any(|n| line.contains(n))) {
            lines.push(line.to_owned());
        }
    }
    Ok(lines)
}

pub fn raw_timestamp_day(line: &str) -> Option<&str> {
    let pos = line.find("\"timestamp\"")?;
    let value = line[pos..]
        .split_once(':')?
        .1
        .trim_start()
        .strip_prefix('"')?;
    let day = value.get(..10)?;
    (day.as_bytes().get(4) == Some(&b'-') && day.as_bytes().get(7) == Some(&b'-')).then_some(day)
}

/// Traverse complete lines backwards, stopping as soon as the visitor returns false.
/// Returns the last complete offset and the number of bytes read.
pub fn visit_reverse(
    path: &Path,
    end: u64,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> io::Result<(u64, u64)> {
    let mut file = File::open(path)?;
    let mut cursor = end;
    let mut pending = Vec::new();
    let mut complete_end = None;
    let mut read = 0;
    while cursor > 0 {
        let count = cursor.min(8192);
        cursor -= count;
        file.seek(SeekFrom::Start(cursor))?;
        let mut chunk = vec![0; count as usize];
        file.read_exact(&mut chunk)?;
        read += count;
        if complete_end.is_none() {
            if let Some(pos) = chunk.iter().rposition(|&b| b == b'\n') {
                complete_end = Some(cursor + pos as u64 + 1);
                chunk.truncate(pos + 1);
            } else {
                continue;
            }
        }
        chunk.extend_from_slice(&pending);
        let mut parts: Vec<_> = chunk.split(|&b| b == b'\n').collect();
        pending = parts.remove(0).to_vec();
        for line in parts.into_iter().rev().filter(|line| !line.is_empty()) {
            if !visit(line) {
                return Ok((complete_end.unwrap_or(0), read));
            }
        }
    }
    if !pending.is_empty() {
        visit(&pending);
    }
    Ok((complete_end.unwrap_or(0), read))
}

const FIRST_LINE_PROBE_LIMIT: u64 = 1 << 20;

pub fn first_line(path: &Path) -> io::Result<(Vec<u8>, u64)> {
    let mut reader = BufReader::new(File::open(path)?.take(FIRST_LINE_PROBE_LIMIT));
    let mut bytes = Vec::new();
    reader.read_until(b'\n', &mut bytes)?;
    let read = bytes.len() as u64;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok((bytes, read))
}

/// Stream complete lines in a bounded byte range without retaining the file in memory.
pub fn visit_forward(
    path: &Path,
    start: u64,
    end: u64,
    mut visit: impl FnMut(&str),
) -> io::Result<(u64, u64)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(end.saturating_sub(start)));
    let mut line = Vec::new();
    let mut offset = start;
    let mut read = 0;
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        read += count as u64;
        if count == 0 || line.last() != Some(&b'\n') {
            break;
        }
        offset += count as u64;
        if let Ok(text) = std::str::from_utf8(&line[..line.len() - 1]) {
            visit(text);
        }
    }
    Ok((offset, read))
}
