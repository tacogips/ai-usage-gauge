use crate::types::{AppConfiguration, ChartColors};
use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("configuration {path}: {message}")]
pub struct ConfigError {
    pub path: PathBuf,
    pub message: String,
}

pub struct ConfigStore {
    file_path: PathBuf,
}
impl ConfigStore {
    pub fn new(file_path: PathBuf) -> Self {
        Self { file_path }
    }
    fn error(&self, e: impl std::fmt::Display) -> ConfigError {
        ConfigError {
            path: self.file_path.clone(),
            message: e.to_string(),
        }
    }
    pub fn load(&self) -> Result<AppConfiguration, ConfigError> {
        let bytes = fs::read(&self.file_path).map_err(|e| self.error(e))?;
        let config: AppConfiguration = serde_json::from_slice(&bytes).map_err(|e| self.error(e))?;
        validate(&config).map_err(|e| self.error(e))?;
        Ok(config)
    }
    pub fn load_or_create(&self) -> Result<AppConfiguration, ConfigError> {
        if self.file_path.exists() {
            return self.load();
        }
        let parent = self.file_path.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| self.error(e))?;
        let config = AppConfiguration {
            ccusage_path: None,
            default_reset_term: "daily".into(),
            dashboard_port: 18081,
            dashboard_autostart: true,
            poll_interval_seconds: 20,
            cache_retention_days: 365,
            remote_retry_count: 3,
            remote_retry_delay_seconds: 2,
            remote_timeout_seconds: 15,
            chart_colors: ChartColors::default(),
        };
        let mut value = serde_json::to_value(&config).map_err(|e| self.error(e))?;
        value["ccusagePath"] = serde_json::Value::Null;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|e| self.error(e))?;
        let mut temp = tempfile::Builder::new()
            .prefix(".config-")
            .tempfile_in(parent)
            .map_err(|e| self.error(e))?;
        temp.write_all(&bytes)
            .and_then(|_| temp.write_all(b"\n"))
            .map_err(|e| self.error(e))?;
        match temp.persist_noclobber(&self.file_path) {
            Ok(_) => Ok(config),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => self.load(),
            Err(e) => Err(self.error(e.error)),
        }
    }
}
fn validate(c: &AppConfiguration) -> Result<(), &'static str> {
    let term = c.default_reset_term.as_str();
    let valid_term = matches!(term, "hourly" | "daily" | "weekly" | "monthly");
    if !valid_term {
        return Err("invalid defaultResetTerm");
    }
    if c.dashboard_port == 0 || c.poll_interval_seconds == 0 || c.cache_retention_days == 0 {
        return Err("port and intervals must be positive");
    }
    if c.remote_retry_count > 10
        || c.remote_retry_delay_seconds > 600
        || !(1..=600).contains(&c.remote_timeout_seconds)
    {
        return Err("invalid remote retry or timeout");
    }
    if c.ccusage_path.as_ref().is_some_and(|p| !p.starts_with('/')) {
        return Err("ccusagePath must be absolute");
    }
    for scheme in [&c.chart_colors.light, &c.chart_colors.dark] {
        for (key, value) in scheme.machines.iter().chain(&scheme.models) {
            if key.is_empty()
                || key.len() > 256
                || key.bytes().any(|b| b < 32 || b == 127)
                || value.len() != 7
                || !value.starts_with('#')
                || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
            {
                return Err("invalid chart color");
            }
        }
    }
    Ok(())
}
