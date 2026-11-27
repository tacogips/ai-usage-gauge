use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DESKTOP_EXECUTABLE_ENV: &str = "AI_USAGE_GAUGE_DESKTOP_EXECUTABLE";
pub const LEGACY_DESKTOP_EXECUTABLE_ENV: &str = "CCUSAGE_GAUGE_DESKTOP_EXECUTABLE";
pub const MISSING_DIAGNOSTIC: &str =
    "Tauri dashboard executable missing. Run mise run desktop:build, or set AI_USAGE_GAUGE_DESKTOP_EXECUTABLE.";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DesktopExecutableError {
    #[error("{0}")]
    ExecutableMissing(String),
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Resolve the Tauri desktop GUI executable (`ai-usage-gauge-app`) following `DESIGN-LAUNCH-05`:
/// 1. If `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` is present in `env` (even if empty), validate that
///    exact path is an executable file and NEVER fall back to sibling lookup on failure.
/// 2. If unset, canonicalize `cli_exe_path` and look for sibling `ai-usage-gauge-app`.
pub fn resolve_desktop_executable(
    env: &HashMap<String, String>,
    cli_exe_path: &Path,
) -> Result<PathBuf, DesktopExecutableError> {
    if let Some(raw) = env
        .get(DESKTOP_EXECUTABLE_ENV)
        .or_else(|| env.get(LEGACY_DESKTOP_EXECUTABLE_ENV))
    {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DesktopExecutableError::ExecutableMissing(
                MISSING_DIAGNOSTIC.to_owned(),
            ));
        }
        let candidate = PathBuf::from(trimmed);
        if is_executable_file(&candidate) {
            return Ok(fs::canonicalize(&candidate).unwrap_or(candidate));
        }
        return Err(DesktopExecutableError::ExecutableMissing(
            MISSING_DIAGNOSTIC.to_owned(),
        ));
    }

    let resolved_cli =
        fs::canonicalize(cli_exe_path).unwrap_or_else(|_| cli_exe_path.to_path_buf());
    if let Some(dir) = resolved_cli.parent() {
        let sibling = dir.join("ai-usage-gauge-app");
        if is_executable_file(&sibling) {
            return Ok(fs::canonicalize(&sibling).unwrap_or(sibling));
        }
    }

    Err(DesktopExecutableError::ExecutableMissing(
        MISSING_DIAGNOSTIC.to_owned(),
    ))
}
