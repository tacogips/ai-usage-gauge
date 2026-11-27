use crate::types::TimestampedUsageEvent;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct CachedUsageEvent {
    pub day: String,
    pub event: TimestampedUsageEvent,
}
#[derive(Debug, Clone)]
pub struct UsageEventFileScan<C> {
    pub canonical_path: PathBuf,
    pub file_size: u64,
    pub modified: Option<SystemTime>,
    pub scanned_through: u64,
    pub boundary: Vec<u8>,
    pub since: String,
    pub timezone_id: String,
    pub events: HashMap<String, CachedUsageEvent>,
    pub context: C,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStep {
    Reuse,
    Resume,
    Rescan,
}

pub fn boundary(path: &Path, offset: u64) -> io::Result<Vec<u8>> {
    let length = offset.min(256) as usize;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset - length as u64))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
pub fn scan_step<C>(
    path: &Path,
    entry: Option<&UsageEventFileScan<C>>,
    since: &str,
    timezone_id: &str,
) -> io::Result<ScanStep> {
    let meta = fs::metadata(path)?;
    scan_step_with_meta(path, &meta, entry, since, timezone_id)
}

pub fn scan_step_with_meta<C>(
    path: &Path,
    meta: &fs::Metadata,
    entry: Option<&UsageEventFileScan<C>>,
    since: &str,
    timezone_id: &str,
) -> io::Result<ScanStep> {
    let Some(entry) = entry else {
        return Ok(ScanStep::Rescan);
    };
    if entry.timezone_id != timezone_id || entry.since.as_str() > since {
        return Ok(ScanStep::Rescan);
    }
    if entry.file_size == meta.len() && entry.modified == meta.modified().ok() {
        return Ok(ScanStep::Reuse);
    }
    if meta.len() < entry.file_size
        || meta.len() < entry.scanned_through
        || boundary(path, entry.scanned_through).ok().as_deref() != Some(entry.boundary.as_slice())
    {
        return Ok(ScanStep::Rescan);
    }
    Ok(ScanStep::Resume)
}

#[derive(Debug, Clone)]
pub struct ScanCacheRetention<C> {
    pub retention_floor: String,
    pub entries: HashMap<PathBuf, UsageEventFileScan<C>>,
    pub active_boundary: Option<(String, HashMap<PathBuf, UsageEventFileScan<C>>)>,
}
impl<C> ScanCacheRetention<C> {
    pub fn new(retention_floor: String) -> Self {
        Self {
            retention_floor,
            entries: HashMap::new(),
            active_boundary: None,
        }
    }
    pub fn set_boundary(&mut self, day: Option<&str>) {
        let day = day.filter(|d| *d < self.retention_floor.as_str());
        if self.active_boundary.as_ref().map(|v| v.0.as_str()) != day {
            self.active_boundary = day.map(|d| (d.to_owned(), HashMap::new()));
        }
    }
    pub fn prune(&mut self, floor: &str) {
        self.retention_floor = floor.to_owned();
        for entry in self.entries.values_mut() {
            entry.events.retain(|_, e| e.day.as_str() >= floor);
            if entry.since.as_str() < floor {
                entry.since = floor.to_owned();
            }
        }
        if self
            .active_boundary
            .as_ref()
            .is_some_and(|v| v.0.as_str() >= floor)
        {
            self.active_boundary = None;
        }
    }
    pub fn insert_boundary(&mut self, mut entry: UsageEventFileScan<C>) {
        if let Some((day, entries)) = &mut self.active_boundary {
            entry.events.retain(|_, e| e.day == *day);
            entries.insert(entry.canonical_path.clone(), entry);
        }
    }
}
