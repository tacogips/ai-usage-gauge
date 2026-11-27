pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod file_scan;
pub mod log_reader;

use crate::types::{ScanFileTransition, ScanStats, TimestampedUsageEvent};
use chrono_tz::Tz;
use file_scan::{CachedUsageEvent, ScanStep, UsageEventFileScan};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum ScanContext {
    Claude,
    Codex(codex::CodexScanContext),
    Antigravity {
        conversation: String,
        directory: Option<String>,
    },
}

#[derive(Default)]
pub struct LocalUsageScanner {
    pub entries: HashMap<PathBuf, UsageEventFileScan<ScanContext>>,
    pub stats: ScanStats,
    boundary_day: Option<String>,
    boundary_entries: HashMap<PathBuf, UsageEventFileScan<ScanContext>>,
}
impl LocalUsageScanner {
    pub fn new() -> Self {
        Self::default()
    }
    /// Pin a single historical day for active rolling-window scans. Passing None clears it.
    /// Historical one-off queries use scan_file and cannot populate this slot.
    pub fn set_active_boundary(&mut self, day: Option<&str>, retention_floor: &str) {
        for entry in self.entries.values_mut() {
            if entry.since.as_str() < retention_floor {
                entry
                    .events
                    .retain(|_, event| event.day.as_str() >= retention_floor);
                entry.since = retention_floor.to_owned();
            }
        }
        let day = day.filter(|day| *day < retention_floor);
        if self.boundary_day.as_deref() != day {
            self.boundary_entries.clear();
            self.boundary_day = day.map(str::to_owned);
        }
    }

    /// Scan a historical file without modifying either long-lived cache.
    pub fn scan_historical_once(
        &mut self,
        path: &Path,
        since: &str,
        timezone: Tz,
        initial: ScanContext,
    ) -> io::Result<(Vec<TimestampedUsageEvent>, ScanFileTransition)> {
        let mut temporary = Self::new();
        let result = temporary.scan_file(path, since, timezone, initial);
        self.stats.files_reused += temporary.stats.files_reused;
        self.stats.files_resumed += temporary.stats.files_resumed;
        self.stats.files_rescanned += temporary.stats.files_rescanned;
        self.stats.bytes_read += temporary.stats.bytes_read;
        result
    }

    pub fn scan_active_boundary(
        &mut self,
        path: &Path,
        timezone: Tz,
        initial: ScanContext,
    ) -> io::Result<(Vec<TimestampedUsageEvent>, ScanFileTransition)> {
        let day = self.boundary_day.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "active boundary is not pinned")
        })?;
        std::mem::swap(&mut self.entries, &mut self.boundary_entries);
        let result = self.scan_file(path, &day, timezone, initial);
        for entry in self.entries.values_mut() {
            entry.events.retain(|_, event| event.day == day);
        }
        std::mem::swap(&mut self.entries, &mut self.boundary_entries);
        result.map(|(mut events, transition)| {
            events.retain(|event| {
                event
                    .timestamp
                    .with_timezone(&timezone)
                    .format("%Y-%m-%d")
                    .to_string()
                    == day
            });
            (events, transition)
        })
    }

    pub fn scan_file(
        &mut self,
        path: &Path,
        since: &str,
        timezone: Tz,
        initial: ScanContext,
    ) -> io::Result<(Vec<TimestampedUsageEvent>, ScanFileTransition)> {
        let path = fs::canonicalize(path)?;
        let metadata = fs::metadata(&path)?;
        if let Some(entry) = self.entries.get(&path) {
            if entry.timezone_id == timezone.name()
                && entry.since.as_str() <= since
                && (entry.file_size != metadata.len() || entry.modified != metadata.modified().ok())
                && metadata.len() >= entry.file_size
                && metadata.len() >= entry.scanned_through
            {
                self.stats.bytes_read += entry.boundary.len() as u64;
            }
        }
        let step = file_scan::scan_step_with_meta(
            &path,
            &metadata,
            self.entries.get(&path),
            since,
            timezone.name(),
        )?;
        if step == ScanStep::Reuse {
            self.stats.files_reused += 1;
            let events = self.entries[&path]
                .events
                .values()
                .filter(|v| v.day.as_str() >= since)
                .map(|v| v.event.clone())
                .collect();
            return Ok((events, ScanFileTransition::Reuse));
        }
        let old = if step == ScanStep::Resume {
            self.entries.remove(&path)
        } else {
            None
        };
        let retained_since = old
            .as_ref()
            .map_or_else(|| since.to_owned(), |v| v.since.clone());
        let start = old.as_ref().map_or(0, |v| v.scanned_through);
        let mut events = old.as_ref().map_or_else(HashMap::new, |v| v.events.clone());
        let mut context = old.map_or(initial, |v| v.context);
        if let ScanContext::Codex(ctx) = &mut context {
            if ctx.session_id.is_empty() {
                ctx.session_id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
            }
        }
        let mut appended = Vec::new();
        let end;
        if step == ScanStep::Rescan && !since.is_empty() && matches!(context, ScanContext::Codex(_))
        {
            let ScanContext::Codex(initial_context) = &context else {
                unreachable!()
            };
            let (head, head_bytes) = log_reader::first_line(&path)?;
            let mut parser =
                codex::CodexReverseParser::new(initial_context.clone(), &head, since, timezone);
            let (offset, bytes) =
                log_reader::visit_reverse(&path, metadata.len(), |line| parser.consume(line))?;
            self.stats.bytes_read += bytes + head_bytes;
            let (found, final_context) = parser.finish();
            events.extend(
                found
                    .into_iter()
                    .map(|(key, (day, event))| (key, CachedUsageEvent { day, event })),
            );
            context = ScanContext::Codex(final_context);
            end = offset;
        } else {
            let (offset, bytes) =
                log_reader::visit_forward(&path, start, metadata.len(), |line| {
                    let parsed = match &mut context {
                        ScanContext::Claude => claude::parse_line(line),
                        ScanContext::Antigravity {
                            conversation,
                            directory,
                        } => antigravity::parse_line(
                            line.as_bytes(),
                            conversation,
                            directory.as_deref(),
                        ),
                        ScanContext::Codex(ctx) => {
                            codex::consume_with_context(ctx, line.as_bytes(), timezone)
                                .map(|(_, event)| event)
                        }
                    };
                    if let Some(event) = parsed {
                        let day = event
                            .timestamp
                            .with_timezone(&timezone)
                            .format("%Y-%m-%d")
                            .to_string();
                        if step == ScanStep::Resume {
                            appended.push(event.clone());
                        }
                        if day.as_str() >= retained_since.as_str()
                            && !(event.agent == "claude"
                                && events
                                    .get(&event.identity)
                                    .is_some_and(|v| v.event.timestamp > event.timestamp))
                        {
                            events.insert(event.identity.clone(), CachedUsageEvent { day, event });
                        }
                    }
                })?;
            end = offset;
            self.stats.bytes_read += bytes;
        }
        let transition = if step == ScanStep::Resume {
            self.stats.files_resumed += 1;
            ScanFileTransition::Resume {
                prev_offset: start,
                new_offset: end,
                new_events: appended,
            }
        } else {
            self.stats.files_rescanned += 1;
            ScanFileTransition::Rescan
        };
        let result = events
            .values()
            .filter(|v| v.day.as_str() >= since)
            .map(|v| v.event.clone())
            .collect();
        self.stats.bytes_read += end.min(256);
        self.entries.insert(
            path.clone(),
            UsageEventFileScan {
                canonical_path: path.clone(),
                file_size: metadata.len(),
                modified: metadata.modified().ok(),
                scanned_through: end,
                boundary: file_scan::boundary(&path, end)?,
                since: retained_since,
                timezone_id: timezone.name().into(),
                events,
                context,
            },
        );
        Ok((result, transition))
    }
}
