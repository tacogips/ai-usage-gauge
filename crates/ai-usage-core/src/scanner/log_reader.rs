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

/// Extract a 19-byte UTC ISO8601 timestamp prefix (`YYYY-MM-DDTHH:MM:SS`) for `key`
/// without invoking a JSON parser. Only accepts UTC (`Z` / `+00:00`) values so
/// lexicographical comparison matches chronological order.
pub fn raw_utc_timestamp_prefix<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pos = line.find(key)?;
    let value = line[pos + key.len()..]
        .split_once(':')?
        .1
        .trim_start()
        .strip_prefix('"')?;
    let end_quote = value.find('"')?;
    let raw = &value[..end_quote];
    if !(raw.ends_with('Z') || raw.ends_with("+00:00")) {
        return None;
    }
    let prefix = raw.get(..19)?;
    let b = prefix.as_bytes();
    let valid = b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .into_iter()
            .all(|i| b[i].is_ascii_digit());
    valid.then_some(prefix)
}

const BINARY_SEARCH_MIN_BYTES: u64 = 64 * 1024;
const BINARY_SEARCH_PROBE_BYTES: usize = 128 * 1024;

/// Locate a safe byte offset `<= first line with timestamp >= cutoff_utc_19` using
/// binary search and raw substring timestamp extraction (zero JSON parsing).
/// Returns `(start_offset, bytes_probed)`. Always fails safe (returns a smaller offset
/// or `0`) if a probe window has no parseable UTC timestamp.
pub fn find_start_offset_for_utc_cutoff(
    path: &Path,
    file_size: u64,
    key: &str,
    cutoff_utc_19: &str,
) -> io::Result<(u64, u64)> {
    if file_size < BINARY_SEARCH_MIN_BYTES {
        return Ok((0, 0));
    }
    let mut file = File::open(path)?;
    let mut lo: u64 = 0;
    let mut hi: u64 = file_size;
    let mut probed: u64 = 0;
    let mut buf = vec![0u8; BINARY_SEARCH_PROBE_BYTES];

    while hi.saturating_sub(lo) > BINARY_SEARCH_MIN_BYTES {
        let mid = lo + (hi - lo) / 2;
        file.seek(SeekFrom::Start(mid))?;
        let to_read = (hi - mid).min(BINARY_SEARCH_PROBE_BYTES as u64) as usize;
        let n = file.read(&mut buf[..to_read])?;
        probed += n as u64;
        if n == 0 {
            break;
        }
        let slice = &buf[..n];
        // Align to the first complete line starting after `mid`.
        let Some(first_nl) = slice.iter().position(|&b| b == b'\n') else {
            // Single line spans the entire probe window; fail safe by moving left.
            hi = mid;
            continue;
        };
        let mut cursor = first_nl + 1;
        let mut found: Option<(u64, bool)> = None;
        let mut lines_checked = 0usize;
        while cursor < n && lines_checked < 16 {
            let line_end = slice[cursor..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(n, |p| cursor + p);
            if line_end == n && mid + (n as u64) < file_size {
                break; // Incomplete trailing line in probe buffer.
            }
            lines_checked += 1;
            if let Ok(line) = std::str::from_utf8(&slice[cursor..line_end]) {
                if let Some(ts) = raw_utc_timestamp_prefix(line, key) {
                    found = Some((mid + cursor as u64, ts < cutoff_utc_19));
                    break;
                }
            }
            cursor = line_end + 1;
        }
        match found {
            Some((line_offset, true)) if line_offset > lo => {
                lo = line_offset;
            }
            _ => {
                hi = mid;
            }
        }
    }

    Ok((lo, probed))
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
