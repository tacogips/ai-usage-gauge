use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[derive(Debug, Error)]
pub enum CacheLifecycleError {
    #[error("cache lifecycle operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("cache lifecycle database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unsafe cache file or conflicting recovery state")]
    UnsafeCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    NoLegacyCache,
    Migrated,
    DestinationExists,
    Conflict,
    InvalidLegacyCache,
}

pub struct LocalCacheMigrator {
    legacy: PathBuf,
    destination: PathBuf,
}

impl LocalCacheMigrator {
    pub fn new(legacy: impl Into<PathBuf>, destination: impl Into<PathBuf>) -> Self {
        Self {
            legacy: legacy.into(),
            destination: destination.into(),
        }
    }

    pub fn migrate_if_needed(&self) -> Result<MigrationOutcome, CacheLifecycleError> {
        let directory = self
            .destination
            .parent()
            .ok_or(CacheLifecycleError::UnsafeCache)?;
        fs::create_dir_all(directory)?;
        set_mode(directory, 0o700)?;
        if self.destination.exists() {
            if !is_safe_file(&self.destination) {
                return Err(CacheLifecycleError::UnsafeCache);
            }
            set_mode(&self.destination, 0o600)?;
            return Ok(if self.legacy.exists() {
                MigrationOutcome::Conflict
            } else {
                MigrationOutcome::DestinationExists
            });
        }
        if !self.legacy.exists() {
            return Ok(MigrationOutcome::NoLegacyCache);
        }
        if !is_safe_file(&self.legacy) || !valid_cache(&self.legacy) {
            return Ok(MigrationOutcome::InvalidLegacyCache);
        }
        {
            let db = Connection::open_with_flags(&self.legacy, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            let (busy, _, _): (i64, i64, i64) =
                db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            if busy != 0 {
                return Err(CacheLifecycleError::UnsafeCache);
            }
        }
        set_mode(&self.legacy, 0o600)?;
        if self.destination.exists() {
            return Ok(MigrationOutcome::Conflict);
        }
        match rename_exclusive(&self.legacy, &self.destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(MigrationOutcome::Conflict);
            }
            Err(error) => return Err(error.into()),
        }
        for suffix in ["-wal", "-shm"] {
            if sidecar(&self.legacy, suffix).exists() {
                return Err(CacheLifecycleError::UnsafeCache);
            }
        }
        Ok(MigrationOutcome::Migrated)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_exclusive(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exclusive rename unavailable",
    ))
}

fn valid_cache(path: &Path) -> bool {
    let result = (|| -> rusqlite::Result<bool> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let check: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        let count: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('cache_metadata', 'daily_metrics')", [], |row| row.get(0))?;
        Ok(check == "ok" && count == 2)
    })();
    result.unwrap_or(false)
}

fn is_safe_file(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.nlink() == 1 && metadata.uid() == unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

pub struct MachineCacheRecovery;

impl MachineCacheRecovery {
    pub fn reconcile(machine_id: &str, cache: &Path) -> Result<(), CacheLifecycleError> {
        validate_machine_id(machine_id)?;
        let directory = cache.parent().ok_or(CacheLifecycleError::UnsafeCache)?;
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !matches_clear_transaction(&entry.file_name().to_string_lossy(), machine_id) {
                continue;
            }
            let transaction = entry.path();
            if !entry.file_type()?.is_dir() {
                return Err(CacheLifecycleError::UnsafeCache);
            }
            if transaction.join("COMMITTED").exists() {
                fs::remove_dir_all(transaction)?;
                continue;
            }
            let staged = fs::read_dir(&transaction)?.collect::<Result<Vec<_>, _>>()?;
            for item in &staged {
                if directory.join(item.file_name()).exists() {
                    return Err(CacheLifecycleError::UnsafeCache);
                }
            }
            for item in staged {
                fs::rename(item.path(), directory.join(item.file_name()))?;
            }
            fs::remove_dir(transaction)?;
        }
        Ok(())
    }
}

pub struct MachineCacheClearCoordinator;

impl MachineCacheClearCoordinator {
    pub fn clear(
        machine_id: &str,
        cache: &Path,
        legacy_local: Option<&Path>,
    ) -> Result<(), CacheLifecycleError> {
        validate_machine_id(machine_id)?;
        MachineCacheRecovery::reconcile(machine_id, cache)?;
        let directory = cache.parent().ok_or(CacheLifecycleError::UnsafeCache)?;
        let transaction = directory.join(format!(".clear-{machine_id}-{}", Uuid::new_v4()));
        fs::create_dir(&transaction)?;
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        let result = (|| -> Result<(), CacheLifecycleError> {
            for base in std::iter::once(cache).chain(legacy_local) {
                if base.parent() != Some(directory) {
                    return Err(CacheLifecycleError::UnsafeCache);
                }
                for suffix in ["", "-journal", "-shm", "-wal"] {
                    let source = sidecar(base, suffix);
                    if source.exists() {
                        let target = transaction
                            .join(source.file_name().ok_or(CacheLifecycleError::UnsafeCache)?);
                        if target.exists() {
                            return Err(CacheLifecycleError::UnsafeCache);
                        }
                        fs::rename(&source, &target)?;
                        staged.push((source, target));
                    }
                }
            }
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(transaction.join("COMMITTED"))?;
            marker.write_all(b"committed\n")?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(transaction.join("COMMITTED"));
            for (source, target) in staged.into_iter().rev() {
                if !source.exists() {
                    let _ = fs::rename(target, source);
                }
            }
            let _ = fs::remove_dir(&transaction);
            return Err(error);
        }
        let _ = fs::remove_dir_all(transaction);
        Ok(())
    }
}

fn matches_clear_transaction(name: &str, machine_id: &str) -> bool {
    name.strip_prefix(&format!(".clear-{machine_id}-"))
        .is_some_and(|suffix| {
            !suffix.is_empty() && (Uuid::parse_str(suffix).is_ok() || !suffix.contains('-'))
        })
}

fn validate_machine_id(machine_id: &str) -> Result<(), CacheLifecycleError> {
    if machine_id.is_empty()
        || !machine_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(CacheLifecycleError::UnsafeCache);
    }
    Ok(())
}
