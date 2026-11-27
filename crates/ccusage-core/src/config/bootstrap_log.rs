use chrono::{DateTime, Utc};
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct BootstrapLogger {
    primary: PathBuf,
    fallback: Option<PathBuf>,
    runtime: String,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    max_bytes: u64,
    retention: Duration,
    selected: Mutex<Option<(PathBuf, bool)>>,
}
impl BootstrapLogger {
    pub fn new(
        primary: PathBuf,
        fallback: Option<PathBuf>,
        runtime: impl Into<String>,
        clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
        max_bytes: u64,
        retention: Duration,
    ) -> Self {
        Self {
            primary,
            fallback,
            runtime: safe(runtime.into().as_str(), 64),
            clock: Arc::new(clock),
            max_bytes,
            retention,
            selected: Mutex::new(None),
        }
    }
    pub fn activate(&self) {
        let Ok(mut selected) = self.selected.lock() else {
            return;
        };
        if selected.is_some() {
            return;
        }
        for (path, fallback) in
            std::iter::once((&self.primary, false)).chain(self.fallback.as_ref().map(|p| (p, true)))
        {
            if prepare_directory(path).is_ok() && with_lock(path, || self.cleanup(path)).is_ok() {
                *selected = Some((path.clone(), fallback));
                break;
            }
        }
    }
    pub fn append(&self, severity: &str, phase: &str, code: &str, message: &str) {
        self.activate();
        let Ok(mut selected) = self.selected.lock() else {
            return;
        };
        let Some((dir, fallback)) = selected.as_mut() else {
            return;
        };
        let _ = with_lock(dir, || {
            if *fallback {
                self.write_record(
                    dir,
                    "warning",
                    "logging",
                    "log_primary_unavailable",
                    "Primary log location unavailable; using fallback",
                )?;
                *fallback = false;
            }
            self.write_record(dir, severity, phase, code, message)
        });
    }
    fn write_record(
        &self,
        dir: &Path,
        severity: &str,
        phase: &str,
        code: &str,
        message: &str,
    ) -> io::Result<()> {
        let now = (self.clock)();
        let record = json!({"timestamp": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "severity": safe(severity, 32), "runtime": self.runtime, "phase": safe(phase, 64), "code": safe(code, 128), "message": if code == "log_primary_unavailable" && message == "Primary log location unavailable; using fallback" { message } else { "[redacted]" }});
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        if line.len() > 16 * 1024 {
            return Err(io::Error::other("record too large"));
        }
        let active = dir.join("ccusage-gauge.jsonl");
        let mut file = private_file(&active)?;
        if file.metadata()?.len().saturating_add(line.len() as u64) > self.max_bytes
            && file.metadata()?.len() > 0
        {
            drop(file);
            let stamp = now.format("%Y%m%dT%H%M%S%3fZ");
            let mut seq = 0u64;
            loop {
                let dest = dir.join(format!("ccusage-gauge-{stamp}-{seq}.jsonl"));
                if fs::symlink_metadata(&dest).is_err() {
                    fs::rename(&active, dest)?;
                    break;
                }
                seq += 1;
            }
            let _ = self.cleanup(dir);
            file = private_file(&active)?;
        }
        file.write_all(&line)
    }
    fn cleanup(&self, dir: &Path) -> io::Result<()> {
        let cutoff = (self.clock)().timestamp()
            - i64::try_from(self.retention.as_secs()).unwrap_or(i64::MAX);
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("ccusage-gauge-") || !name.ends_with(".jsonl") {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if valid_file(&meta) && meta.mtime() < cutoff {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}
fn safe(value: &str, limit: usize) -> String {
    let lower = value.to_ascii_lowercase();
    if value.contains('/')
        || value.contains('\\')
        || value.contains('\n')
        || value.contains('\r')
        || [
            "token",
            "secret",
            "password",
            "authorization",
            "stderr",
            "bearer",
        ]
        .iter()
        .any(|v| lower.contains(v))
    {
        return "[redacted]".into();
    }
    value
        .chars()
        .take(limit)
        .filter(|c| !c.is_control())
        .collect()
}
fn prepare_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let m = fs::symlink_metadata(path)?;
    if !m.file_type().is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700
    {
        return Err(io::Error::other("unsafe log directory"));
    }
    Ok(())
}
fn valid_file(m: &fs::Metadata) -> bool {
    m.file_type().is_file()
        && m.uid() == unsafe { libc::geteuid() }
        && m.nlink() == 1
        && m.mode() & 0o777 == 0o600
}
fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !valid_file(&file.metadata()?) {
        return Err(io::Error::other("unsafe log file"));
    }
    Ok(file)
}
fn with_lock<T>(dir: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let lock = private_file(&dir.join(".ccusage-gauge.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let result = f();
    unsafe {
        libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
    }
    result
}
